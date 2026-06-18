// Google Gemini provider.
//
// Supports three variants:
//   Cli:          standard Gemini models via cloudcode-pa.googleapis.com (OAuth)
//   Antigravity:  Gemini 3 via daily-cloudcode-pa.sandbox.googleapis.com (OAuth)
//   ApiKey:       Gemini 3 via generativelanguage.googleapis.com (GEMINI_API_KEY env var)
//
// OAuth credential management and project discovery are in gemini_auth.rs.

use std::collections::{HashMap, HashSet};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use ri::{
    ApiError, AuthMethod, BlobHash, ContentBlock, EventStream, LlmProvider, Message, Model, ModelCost,
    RequestOptions, Role, StreamEvent, ThinkingLevel, Usage,
};
use crate::sse::{self, SseEvent, SseInterpreter};
use crate::gemini_auth;
use crate::media;

// -- Variant --

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiVariant {
    Cli,
    Antigravity,
    /// Direct Gemini API via GEMINI_API_KEY env var. No OAuth, no project.
    ApiKey,
}

// -- API key file storage --

fn api_key_path() -> eyre::Result<std::path::PathBuf> {
    Ok(crate::creds::ri_dir()?.join("gemini_api_key"))
}

fn load_api_key() -> Option<String> {
    // Saved file takes priority; env var is the fallback.
    if let Ok(path) = api_key_path() {
        if let Ok(key) = std::fs::read_to_string(&path) {
            let key = key.trim().to_string();
            if !key.is_empty() { return Some(key); }
        }
    }
    std::env::var("GEMINI_API_KEY").ok().filter(|k| !k.is_empty())
}

fn save_api_key(key: &str) -> eyre::Result<()> {
    let path = api_key_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, key)?;
    crate::creds::restrict_file_permissions(&path)?;
    Ok(())
}

// -- Provider struct --

pub struct GeminiProvider {
    variant: GeminiVariant,
    /// PKCE verifier and state live here across the `begin_login` ->
    /// `complete_login` HTTP round trip. Access tokens and project IDs are
    /// read fresh from disk via `gemini_auth::load_creds` on each call.
    login: Mutex<LoginInProgress>,
    /// Mandatory Files-API cache: `hash -> (fileUri, expiry)`. A large or
    /// video blob is uploaded ONCE and reused across turns; entries past the
    /// 48h TTL are dropped on access. Interior mutability with a `std::sync`
    /// Mutex -- the critical section is short and never held across an await.
    /// The `fileUri` is never persisted into a message; only the hash is.
    files_cache: std::sync::Mutex<HashMap<BlobHash, (String, chrono::DateTime<chrono::Utc>)>>,
}

#[derive(Default)]
struct LoginInProgress {
    verifier: Option<String>,
    state: Option<String>,
}

impl GeminiProvider {
    pub fn new(variant: GeminiVariant) -> Self {
        Self {
            variant,
            login: Mutex::new(LoginInProgress::default()),
            files_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Return `(access_token, project_id)` for this provider. For OAuth
    /// variants, refreshes under the shared credential lock if the on-disk
    /// token is past its expiry buffer. For the API-key variant, just reads
    /// the key.
    async fn ensure_valid_token(&self) -> eyre::Result<(String, String)> {
        if self.variant == GeminiVariant::ApiKey {
            return Ok((load_api_key().unwrap_or_default(), String::new()));
        }

        let creds = match gemini_auth::load_creds(self.variant) {
            Some(c) => c,
            None => return Ok((String::new(), String::new())),
        };
        if !creds.is_expired() {
            return Ok((creds.access_token, creds.project_id.unwrap_or_default()));
        }

        let path = gemini_auth::creds_path(self.variant)?;
        let guard = crate::creds::CredsGuard::acquire(path).await?;
        let mut current = creds;
        if let Some(fresh) = guard.read() {
            if !fresh.is_expired() {
                return Ok((fresh.access_token, fresh.project_id.unwrap_or_default()));
            }
            current = fresh;
        }
        let refreshed = gemini_auth::refresh_token(&current, self.variant).await
            .map_err(|e| eyre::eyre!(
                "Google OAuth refresh failed -- re-login required ({e})"
            ))?;
        guard.write(&refreshed).await?;
        Ok((refreshed.access_token, refreshed.project_id.unwrap_or_default()))
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn id(&self) -> &str {
        match self.variant {
            GeminiVariant::Cli => "google-gemini-cli",
            GeminiVariant::Antigravity => "google-antigravity",
            GeminiVariant::ApiKey => "google-gemini-api",
        }
    }

    fn name(&self) -> &str {
        match self.variant {
            GeminiVariant::Cli => "Google Gemini CLI",
            GeminiVariant::Antigravity => "Google Antigravity",
            GeminiVariant::ApiKey => "Google Gemini API",
        }
    }

    fn models(&self) -> Vec<Model> {
        match self.variant {
            GeminiVariant::Cli => vec![
                Model {
                    id: "gemini-2.5-pro".into(), name: "Gemini 2.5 Pro".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 1.25, output: 10.0, cache_read: 0.125, cache_write: 0.375 },
                },
                Model {
                    id: "gemini-2.5-flash".into(), name: "Gemini 2.5 Flash".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 0.3, output: 2.5, cache_read: 0.03, cache_write: 0.0 },
                },
            ],
            GeminiVariant::Antigravity => vec![
                Model {
                    id: "gemini-3.1-pro-high".into(), name: "Gemini 3.1 Pro".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 2.0, output: 12.0, cache_read: 0.2, cache_write: 0.375 },
                },
                Model {
                    id: "gemini-3.1-pro-low".into(), name: "Gemini 3.1 Pro (Low)".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 2.0, output: 12.0, cache_read: 0.2, cache_write: 0.375 },
                },
                Model {
                    id: "gemini-3-flash".into(), name: "Gemini 3 Flash".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 0.5, output: 3.0, cache_read: 0.05, cache_write: 0.0 },
                },
            ],
            GeminiVariant::ApiKey => vec![
                Model {
                    id: "gemini-3.1-pro-preview".into(), name: "Gemini 3.1 Pro".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 2.0, output: 12.0, cache_read: 0.2, cache_write: 0.375 },
                },
                Model {
                    id: "gemini-3.5-flash".into(), name: "Gemini 3.5 Flash".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 1.5, output: 9.0, cache_read: 0.15, cache_write: 0.0 },
                },
                Model {
                    id: "gemini-3-flash-preview".into(), name: "Gemini 3 Flash".into(),
                    reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
                    cost: ModelCost { input: 0.5, output: 3.0, cache_read: 0.05, cache_write: 0.0 },
                },
            ],
        }
    }

    fn is_authenticated(&self) -> bool {
        match self.variant {
            GeminiVariant::ApiKey => load_api_key().is_some(),
            _ => gemini_auth::load_creds(self.variant).is_some(),
        }
    }

    fn account_label(&self) -> Option<String> {
        match self.variant {
            GeminiVariant::ApiKey => {
                if load_api_key().is_some() { Some("API key".to_string()) } else { None }
            }
            _ => gemini_auth::load_creds(self.variant).and_then(|c| c.email),
        }
    }

    fn can_logout(&self) -> bool {
        match self.variant {
            // Can logout if key is saved to file (not just env var).
            GeminiVariant::ApiKey => api_key_path().ok()
                .map(|p| p.exists())
                .unwrap_or(false),
            _ => gemini_auth::load_creds(self.variant).is_some(),
        }
    }

    async fn begin_login(&self) -> eyre::Result<Option<AuthMethod>> {
        if self.variant == GeminiVariant::ApiKey {
            return Ok(Some(AuthMethod::TextInput {
                prompt: "Enter your Gemini API key (from aistudio.google.com):".into(),
                placeholder: "API key...".into(),
            }));
        }
        let cfg = gemini_auth::config_for(self.variant);
        let verifier = crate::creds::generate_verifier();
        let challenge = crate::creds::challenge(&verifier);
        let login_state = crate::creds::generate_verifier();

        let auth_url = gemini_auth::build_auth_url(self.variant, &challenge, &login_state);

        let mut login = self.login.lock().await;
        login.verifier = Some(verifier);
        login.state = Some(login_state);

        Ok(Some(AuthMethod::LocalCallback {
            url: auth_url,
            port: cfg.port,
            path: cfg.callback_path.to_string(),
        }))
    }

    async fn complete_login(&self, code: &str) -> eyre::Result<()> {
        if self.variant == GeminiVariant::ApiKey {
            let key = code.trim();
            if key.is_empty() {
                return Err(eyre::eyre!("API key cannot be empty"));
            }
            save_api_key(key)?;
            return Ok(());
        }
        let (verifier, login_state) = {
            let mut login = self.login.lock().await;
            let v = login.verifier.take()
                .ok_or_else(|| eyre::eyre!("No login in progress"))?;
            let s = login.state.take()
                .ok_or_else(|| eyre::eyre!("No login state"))?;
            (v, s)
        };

        let (actual_code, returned_state) = match code.split_once('#') {
            Some((c, s)) => (c.to_string(), s.to_string()),
            None => (code.to_string(), login_state.clone()),
        };

        if returned_state != login_state {
            return Err(eyre::eyre!("OAuth state mismatch"));
        }

        let creds = gemini_auth::exchange_code(self.variant, &actual_code, &verifier).await?;
        let guard = crate::creds::CredsGuard::acquire(gemini_auth::creds_path(self.variant)?).await?;
        guard.write(&creds).await?;
        Ok(())
    }

    async fn logout(&self) -> eyre::Result<()> {
        let path = match self.variant {
            GeminiVariant::ApiKey => api_key_path()?,
            _ => gemini_auth::creds_path(self.variant)?,
        };
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError> {
        let (token, project_id) = self.ensure_valid_token().await
            .map_err(|e| ApiError::other(format!("{e:#}")))?;

        // The Files API (large/video uploads) is reachable only with the
        // public API key via `x-goog-api-key`; OAuth variants resolve
        // inline-or-placeholder only.
        let api_key = if self.variant == GeminiVariant::ApiKey && !token.is_empty() {
            Some(token.clone())
        } else {
            None
        };
        let resolved = self.resolve_blobs(&opts, api_key.as_deref()).await;
        let request = build_request(self.variant, &token, &project_id, &opts, &resolved);
        let bytes = sse::send(request, classify_error).await?;
        Ok(Box::pin(sse::drive_sse_stream(bytes, GeminiState::new())))
    }
}

/// Map a Gemini error response to an `ApiError`. Gemini's transient failures are
/// 429 `RESOURCE_EXHAUSTED` (a per-minute throttle) and 500/503 `UNAVAILABLE`;
/// it delivers its retry delay in the body as a `google.rpc.RetryInfo` detail. A
/// 429 that names a daily quota (`PerDay`/`QUOTA_EXHAUSTED`) is permanent -- it
/// would just keep failing -- so it is NOT retried. Used for both the HTTP
/// response (status set) and a mid-stream error chunk (status from `error.code`).
fn classify_error(status: u16, body: &str, retry_after_ms: Option<u64>) -> ApiError {
    let json = serde_json::from_str::<Value>(body).ok();
    let error = json.as_ref().and_then(|p| p.get("error"));
    let gstatus = error.and_then(|e| e["status"].as_str());

    let rate_limited = status == 429 || gstatus == Some("RESOURCE_EXHAUSTED");
    let server_error = (500..=599).contains(&status) || gstatus == Some("UNAVAILABLE");

    let err = sse::HttpApiError { status, body: body.to_string() };
    if rate_limited && is_permanent_quota(error) {
        return ApiError::other(err); // daily/balance exhaustion -- would keep failing
    }
    if rate_limited || server_error {
        let delay = retry_after_ms.or_else(|| retry_info_delay_ms(error)).unwrap_or(0);
        return ApiError::retryable(delay, err);
    }
    ApiError::other(err)
}

/// Whether a Gemini 429 names a daily/long-window quota or exhausted balance --
/// the throttle won't clear on a short backoff (the `QuotaFailure`/`ErrorInfo`
/// details carry the metric name).
fn is_permanent_quota(error: Option<&Value>) -> bool {
    let Some(error) = error else { return false };
    let hay = error.to_string().to_lowercase();
    hay.contains("perday") || hay.contains("daily") || hay.contains("quota_exhausted")
}

/// Gemini's retry delay lives in the body as a `google.rpc.RetryInfo` detail
/// with a `retryDelay` like "15s" or "27.2s".
fn retry_info_delay_ms(error: Option<&Value>) -> Option<u64> {
    let details = error?["details"].as_array()?;
    for d in details {
        if d["@type"].as_str().is_some_and(|t| t.ends_with("RetryInfo")) {
            let s = d["retryDelay"].as_str()?;
            let secs: f64 = s.trim().trim_end_matches('s').parse().ok()?;
            return Some((secs * 1000.0) as u64);
        }
    }
    None
}

/// Maximum raw blob size sent inline as base64. Larger blobs -- and all video,
/// which the API accepts only through the Files API -- are uploaded to the
/// Files API and referenced by `fileUri` instead.
const GEMINI_INLINE_LIMIT: u64 = 18_000_000;

/// Files API entries live 48h; we expire our cache slightly early (47h) so a
/// uri is never referenced after the server has swept it.
const FILES_API_TTL_HOURS: i64 = 47;

impl GeminiProvider {
    /// A still-valid cached fileUri for a hash, dropping the entry if past TTL.
    fn cached_file_uri(&self, hash: &BlobHash) -> Option<String> {
        let mut cache = self.files_cache.lock().unwrap();
        match cache.get(hash) {
            Some((uri, expiry)) if *expiry > chrono::Utc::now() => Some(uri.clone()),
            Some(_) => { cache.remove(hash); None }
            None => None,
        }
    }

    fn store_file_uri(&self, hash: BlobHash, uri: String) {
        let expiry = chrono::Utc::now() + chrono::Duration::hours(FILES_API_TTL_HOURS);
        self.files_cache.lock().unwrap().insert(hash, (uri, expiry));
    }

    /// Capability-aware resolution pass: Gemini takes image/audio/video/pdf.
    /// Non-video blobs at or under [`GEMINI_INLINE_LIMIT`] resolve to inline
    /// base64; everything else (bigger, or any video) goes through the Files
    /// API to a `fileUri`. A non-sendable modality, a missing blob, or an
    /// upload that can't be performed surfaces as a placeholder -- never a
    /// silent drop, never a raw 400.
    async fn resolve_blobs(&self, opts: &RequestOptions, api_key: Option<&str>) -> media::ResolvedMap {
        let mut map = media::ResolvedMap::new();
        for (mime, hash, size) in media::collect_blobs(&opts.messages) {
            let is_video = mime.starts_with("video/");
            let sendable = mime.starts_with("image/")
                || mime.starts_with("audio/")
                || is_video
                || mime == "application/pdf";
            let resolved = if !sendable {
                media::Resolved::Placeholder(format!(
                    "[attachment {}, {} - this model can't read it]",
                    mime, media::human_size(size)
                ))
            } else if !is_video && size <= GEMINI_INLINE_LIMIT {
                match media::read_blob_b64(&opts.blobs, &hash).await {
                    Some(b64) => media::Resolved::Inline { media_type: mime.clone(), b64 },
                    None => media::Resolved::Placeholder(format!("[missing attachment {}]", mime)),
                }
            } else {
                self.resolve_via_files_api(opts, &mime, &hash, size, is_video, api_key).await
            };
            map.insert(hash, resolved);
        }
        map
    }

    /// The Files-API path for large blobs and all video: cache-hit -> reuse;
    /// otherwise read the raw bytes off the Tokio path, upload once, cache,
    /// and reference by `fileUri`.
    async fn resolve_via_files_api(
        &self,
        opts: &RequestOptions,
        mime: &str,
        hash: &BlobHash,
        size: u64,
        is_video: bool,
        api_key: Option<&str>,
    ) -> media::Resolved {
        if let Some(uri) = self.cached_file_uri(hash) {
            return media::Resolved::FileUri { media_type: mime.to_string(), uri };
        }
        let Some(api_key) = api_key else {
            return media::Resolved::Placeholder(format!(
                "[attachment {}, {} - too large to inline; Files API needs an API key]",
                mime, media::human_size(size)
            ));
        };
        // Read the raw bytes off the Tokio worker pool (a large video must not
        // block an async worker).
        let bytes = {
            let blobs = opts.blobs.clone();
            let h = hash.clone();
            match tokio::task::spawn_blocking(move || blobs.get(&h)).await {
                Ok(Ok(Some(b))) => b,
                _ => return media::Resolved::Placeholder(format!("[missing attachment {}]", mime)),
            }
        };
        match upload_to_files_api(api_key, mime, bytes, is_video).await {
            Ok(uri) => {
                self.store_file_uri(hash.clone(), uri.clone());
                media::Resolved::FileUri { media_type: mime.to_string(), uri }
            }
            Err(e) => {
                tracing::warn!("Gemini Files API upload failed for {mime}: {e:#}");
                media::Resolved::Placeholder(format!(
                    "[attachment {}, {} - upload failed]",
                    mime, media::human_size(size)
                ))
            }
        }
    }
}

/// Upload raw bytes to the Gemini Files API and return the file's https `uri`.
///
/// Uses the simple **media** upload (`uploadType=media`, raw body) rather than
/// reqwest's `multipart::Form`: the latter emits `multipart/form-data`, which
/// the Files endpoint rejects, and `uploadType=media` needs no extra crate
/// feature. The response is `{file:{uri,name,state,mimeType}}`. Images/audio
/// are `ACTIVE` immediately; video enters `PROCESSING` and is polled to
/// `ACTIVE` (or `FAILED`) before the uri is usable.
async fn upload_to_files_api(
    api_key: &str,
    mime: &str,
    bytes: Vec<u8>,
    is_video: bool,
) -> eyre::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://generativelanguage.googleapis.com/upload/v1beta/files?uploadType=media")
        .header("x-goog-api-key", api_key)
        .header("content-type", mime)
        .body(bytes)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(eyre::eyre!("Files API upload {}: {}", status, text));
    }
    let parsed: Value = serde_json::from_str(&text)?;
    let file = &parsed["file"];
    let uri = file["uri"].as_str()
        .ok_or_else(|| eyre::eyre!("Files API response missing file.uri: {}", text))?
        .to_string();
    let name = file["name"].as_str().unwrap_or_default().to_string();
    let state = file["state"].as_str().unwrap_or_default();
    if is_video || state == "PROCESSING" {
        poll_file_active(&client, api_key, &name).await?;
    } else if state == "FAILED" {
        return Err(eyre::eyre!("Files API processing FAILED for {}", name));
    }
    Ok(uri)
}

/// Poll `GET /v1beta/{name}` until the file reaches `ACTIVE`. Errors on
/// `FAILED` or a timeout so the caller falls back to a placeholder rather than
/// referencing an unusable file.
async fn poll_file_active(client: &reqwest::Client, api_key: &str, name: &str) -> eyre::Result<()> {
    if name.is_empty() {
        return Err(eyre::eyre!("Files API returned no file name to poll"));
    }
    let url = format!("https://generativelanguage.googleapis.com/v1beta/{}", name);
    // Up to ~5 minutes at a 5s interval -- ample for short clips.
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let resp = client.get(&url).header("x-goog-api-key", api_key).send().await?;
        let parsed: Value = resp.json().await?;
        match parsed["state"].as_str().unwrap_or_default() {
            "ACTIVE" => return Ok(()),
            "FAILED" => return Err(eyre::eyre!("Files API processing FAILED for {}", name)),
            _ => continue,
        }
    }
    Err(eyre::eyre!("Files API processing timed out for {}", name))
}

/// Emit the provider JSON part for a resolved blob (inline / fileData / text).
fn resolved_part(resolved: &media::ResolvedMap, block: &ContentBlock) -> Value {
    if let ContentBlock::Blob { hash, .. } = block {
        match resolved.get(hash) {
            Some(media::Resolved::Inline { media_type, b64 }) => {
                json!({ "inlineData": { "mimeType": media_type, "data": b64 } })
            }
            Some(media::Resolved::FileUri { media_type, uri }) => {
                json!({ "fileData": { "mimeType": media_type, "fileUri": uri } })
            }
            Some(media::Resolved::Placeholder(t)) => json!({ "text": t }),
            None => json!({ "text": block.as_text_or_placeholder() }),
        }
    } else {
        json!({ "text": block.as_text_or_placeholder() })
    }
}

// -- Request building --

fn build_request(
    variant: GeminiVariant,
    token: &str,
    project_id: &str,
    opts: &RequestOptions,
    resolved: &media::ResolvedMap,
) -> reqwest::RequestBuilder {
    if variant == GeminiVariant::ApiKey {
        return build_api_key_request(token, opts, resolved);
    }

    let body = build_cloud_body(variant, project_id, opts, resolved);
    let endpoint = match variant {
        GeminiVariant::Antigravity => gemini_auth::ANTIGRAVITY_DAILY_ENDPOINT,
        GeminiVariant::Cli => gemini_auth::GEMINI_CLI_ENDPOINT,
        GeminiVariant::ApiKey => unreachable!(),
    };
    let url = format!("{}/v1internal:streamGenerateContent?alt=sse", endpoint);

    let ua = match variant {
        GeminiVariant::Antigravity => "antigravity/1.18.0 darwin/arm64",
        GeminiVariant::Cli => "google-cloud-sdk vscode_cloudshelleditor/0.1",
        GeminiVariant::ApiKey => unreachable!(),
    };

    tracing::trace!(%url, %body, "Gemini Cloud Code Assist request");

    reqwest::Client::new()
        .post(&url)
        .header("authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("user-agent", ua)
        .header("x-goog-api-client", "gl-node/22.17.0")
        .header("client-metadata", json!({
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI"
        }).to_string())
        .body(body.to_string())
}

/// Build request for the public Gemini API (API key auth, no Cloud Code Assist wrapper).
fn build_api_key_request(api_key: &str, opts: &RequestOptions, resolved: &media::ResolvedMap) -> reqwest::RequestBuilder {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
        opts.model.id, api_key,
    );

    let body = build_api_key_body(opts, resolved);

    tracing::trace!("Gemini API key request to model [{}]", opts.model.id);

    reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(body.to_string())
}

/// Body for the public Gemini API -- a direct GenerateContentRequest
/// (no project/model/request envelope like Cloud Code Assist).
fn build_api_key_body(opts: &RequestOptions, resolved: &media::ResolvedMap) -> Value {
    let contents = build_contents(&opts.messages, &opts.model.id, resolved);

    let max_tokens = opts.max_tokens
        .unwrap_or_else(|| (opts.model.max_tokens / 3).max(4096));

    let mut generation_config = json!({ "maxOutputTokens": max_tokens });

    if opts.thinking != ThinkingLevel::Off && opts.model.reasoning {
        if let Some(level_str) = thinking_level_string(opts.thinking, &opts.model.id) {
            generation_config["thinkingConfig"] = json!({
                "includeThoughts": true,
                "thinkingLevel": level_str,
            });
        }
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": generation_config,
    });

    if !opts.system_prompt.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": opts.system_prompt }] });
    }

    if !opts.tools.is_empty() {
        let declarations: Vec<Value> = opts.tools.iter().map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        }).collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    } else if opts.native_tools {
        body["tools"] = native_gemini_tools();
    }

    body
}

fn build_cloud_body(variant: GeminiVariant, project_id: &str, opts: &RequestOptions, resolved: &media::ResolvedMap) -> Value {
    let contents = build_contents(&opts.messages, &opts.model.id, resolved);

    let max_tokens = opts.max_tokens
        .unwrap_or_else(|| (opts.model.max_tokens / 3).max(4096));

    let mut generation_config = json!({ "maxOutputTokens": max_tokens });

    if opts.thinking != ThinkingLevel::Off && opts.model.reasoning {
        if is_gemini3(&opts.model.id) {
            if let Some(level_str) = thinking_level_string(opts.thinking, &opts.model.id) {
                generation_config["thinkingConfig"] = json!({
                    "includeThoughts": true,
                    "thinkingLevel": level_str,
                });
            }
        } else {
            if let Some(budget) = thinking_budget(opts.thinking) {
                generation_config["thinkingConfig"] = json!({
                    "includeThoughts": true,
                    "thinkingBudget": budget,
                });
            }
        }
    }

    let mut request = json!({ "contents": contents });

    let system_text = if variant == GeminiVariant::Antigravity {
        format!("{}\n\n{}\n{}", ANTIGRAVITY_SYSTEM_INSTRUCTION, BRIDGE_PROMPT, opts.system_prompt)
    } else {
        opts.system_prompt.to_string()
    };
    request["systemInstruction"] = json!({ "parts": [{ "text": system_text }] });

    request["generationConfig"] = generation_config;

    if !opts.tools.is_empty() {
        let declarations: Vec<Value> = opts.tools.iter().map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        }).collect();
        request["tools"] = json!([{ "functionDeclarations": declarations }]);
    } else if opts.native_tools {
        request["tools"] = native_gemini_tools();
    }

    let mut body = json!({
        "project": project_id,
        "model": opts.model.id,
        "request": request,
        "userAgent": if variant == GeminiVariant::Antigravity { "antigravity" } else { "ri-coding-agent" },
        "requestId": format!("{}-{}-{}",
            if variant == GeminiVariant::Antigravity { "agent" } else { "ri" },
            chrono::Utc::now().timestamp_millis(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
    });

    if variant == GeminiVariant::Antigravity {
        body["requestType"] = json!("agent");
    }

    body
}

// -- Message conversion --

fn build_contents(messages: &[Message], model_id: &str, resolved: &media::ResolvedMap) -> Vec<Value> {
    let gemini3 = is_gemini3(model_id);

    // The set of tool calls we replay as native Gemini protocol (functionCall +
    // functionResponse). A pair qualifies only when both halves are present and,
    // on Gemini 3 (which validates `thoughtSignature`), the call carries a valid
    // one. A call produced by another model never has a Gemini signature, so on
    // Gemini 3 it -- and its result -- fall to the read-only text path; Gemini 2.5
    // imposes no signature requirement and keeps replaying its own calls natively.
    // One set governs both the call and the result so they never disagree --
    // otherwise the model is handed a functionResponse for a call it never sees.
    let complete = ri::complete_tool_pairs(messages);
    let native: HashSet<&str> = messages.iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, sig, .. }
                if complete.contains(id.as_str())
                    && (!gemini3 || sig.as_deref().filter(|s| is_valid_signature(s)).is_some()) =>
                Some(id.as_str()),
            _ => None,
        })
        .collect();

    let mut tool_names: HashMap<String, String> = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id.clone(), name.clone());
            }
        }
    }

    let mut contents: Vec<Value> = Vec::new();

    for msg in messages {
        if msg.role == Role::System { continue; }

        let filtered_content: Vec<&ContentBlock> = msg.content.iter()
            .filter(|c| !matches!(c, ContentBlock::Error { .. }))
            .collect();

        if filtered_content.is_empty() { continue; }

        let has_tool_results = filtered_content.iter().any(|c| matches!(c, ContentBlock::ToolResult { .. }));

        if has_tool_results {
            // A tool-result message becomes one user Content. Each result is a
            // native functionResponse when its call is being replayed natively,
            // otherwise read-only text -- emitting a functionResponse whose
            // functionCall was demoted hands the model a result for a call it never
            // made. Media carried inside a result is hoisted to sibling parts either
            // way, so bytes are never lost on downgrade.
            let mut parts: Vec<Value> = Vec::new();
            let mut hoisted: Vec<Value> = Vec::new();
            for c in &filtered_content {
                if let ContentBlock::ToolResult { tool_use_id, content, is_error, .. } = c {
                    if native.contains(tool_use_id.as_str()) {
                        let tool_name = tool_names.get(tool_use_id.as_str())
                            .cloned()
                            .unwrap_or_else(|| {
                                tracing::warn!("No tool name found for tool_use_id [{}]", tool_use_id);
                                "unknown".to_string()
                            });
                        // as_text_or_placeholder so a Blob yields descriptive text
                        // in the functionResponse, not an empty string.
                        let output_text: String = content.iter()
                            .map(|b| b.as_text_or_placeholder())
                            .collect::<Vec<_>>()
                            .join("\n");
                        let response = if *is_error {
                            json!({ "error": output_text })
                        } else {
                            json!({ "output": output_text })
                        };
                        parts.push(json!({
                            "functionResponse": { "name": tool_name, "response": response }
                        }));
                    } else if let Some(t) = c.tool_as_text() {
                        parts.push(json!({ "text": t }));
                    }
                    // Hoist any injectable inner blob to a sibling media part.
                    for b in content {
                        if let ContentBlock::Blob { hash, .. } = b {
                            match resolved.get(hash) {
                                Some(media::Resolved::Inline { media_type, b64 }) => {
                                    hoisted.push(json!({ "inlineData": { "mimeType": media_type, "data": b64 } }));
                                }
                                Some(media::Resolved::FileUri { media_type, uri }) => {
                                    hoisted.push(json!({ "fileData": { "mimeType": media_type, "fileUri": uri } }));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            // Media after the responses, all within the one Content.
            parts.extend(hoisted);
            push_parts(&mut contents, "user", parts);
            continue;
        }

        let gemini_role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
            Role::System => continue,
        };

        // Blocks that keep this message's role accumulate in `parts`; foreign tool
        // calls that can't be replayed natively accumulate in `downgraded` and are
        // emitted as a trailing user Content (after this message's prose -- any
        // intra-message text/call interleaving is flattened), so the model reads
        // them as external observations rather than its own behavior to imitate.
        // (A demoted call left in the model turn is few-shot bait: Gemini 3 echoes
        // the text shape and emits phantom, non-executing "tool calls".)
        let mut parts: Vec<Value> = Vec::new();
        let mut downgraded: Vec<Value> = Vec::new();
        for block in filtered_content {
            match block {
                ContentBlock::Text { text, .. } => {
                    if text.trim().is_empty() { continue; }
                    parts.push(json!({ "text": text }));
                }
                ContentBlock::Thinking { thinking, replay } => {
                    if thinking.trim().is_empty() { continue; }
                    if let Some(ri::ThinkingReplay::Signature(s)) = replay {
                        if is_valid_signature(s) {
                            let mut part = json!({ "text": thinking, "thought": true });
                            part["thoughtSignature"] = json!(s);
                            parts.push(part);
                            continue;
                        }
                    }
                    parts.push(json!({ "text": thinking }));
                }
                ContentBlock::ToolUse { id, name, input, sig, .. } => {
                    if native.contains(id.as_str()) {
                        let mut part = json!({ "functionCall": { "name": name, "args": input } });
                        if let Some(s) = sig.as_deref().filter(|s| is_valid_signature(s)) {
                            part["thoughtSignature"] = json!(s);
                        }
                        parts.push(part);
                    } else if let Some(t) = block.tool_as_text() {
                        downgraded.push(json!({ "text": t }));
                    }
                }
                ContentBlock::ToolResult { .. } => {}
                ContentBlock::Blob { .. } => {
                    parts.push(resolved_part(resolved, block));
                }
                ContentBlock::Error { .. } => {}
                ContentBlock::Unknown(_) => {}
            }
        }

        push_parts(&mut contents, gemini_role, parts);
        push_parts(&mut contents, "user", downgraded);
    }

    contents
}

/// Append `parts` to `contents` under `role`, merging into the trailing Content
/// when it already carries that role. Gemini 400s on two consecutive same-role
/// Contents, so every Content is built here -- which also guarantees each one
/// already has a `parts` array for the merge to extend. Empty `parts` is a no-op.
fn push_parts(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() { return; }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(arr) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                arr.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

fn is_gemini3(model_id: &str) -> bool {
    model_id.starts_with("gemini-3")
}

/// Provider-native tools (search grounding and sandboxed code execution)
/// enabled when `native_tools` is set and no function-calling tools are present.
fn native_gemini_tools() -> Value {
    json!([
        { "google_search": {} },
        { "code_execution": {} },
    ])
}

fn thinking_level_string(level: ThinkingLevel, model_id: &str) -> Option<&'static str> {
    let is_pro = model_id.contains("3-pro") || model_id.contains("3.1-pro");
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("LOW"),
        ThinkingLevel::Medium => if is_pro { Some("HIGH") } else { Some("MEDIUM") },
        ThinkingLevel::High => Some("HIGH"),
        ThinkingLevel::XHigh => Some("HIGH"),
    }
}

fn thinking_budget(level: ThinkingLevel) -> Option<u32> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some(2048),
        ThinkingLevel::Medium => Some(8192),
        ThinkingLevel::High => Some(16384),
        ThinkingLevel::XHigh => Some(32768),
    }
}

fn is_valid_signature(sig: &str) -> bool {
    if sig.is_empty() || sig.len() % 4 != 0 { return false; }
    sig.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

// -- SSE interpretation --

#[derive(Debug)]
enum GeminiBlock {
    Text { sig: Option<String> },
    Thinking { sig: Option<String> },
    ToolCall { id: String, sig: Option<String> },
}

struct GeminiState {
    current_block: Option<GeminiBlock>,
    tool_call_counter: u64,
    usage: Usage,
    done: bool,
}

impl GeminiState {
    fn new() -> Self {
        Self { current_block: None, tool_call_counter: 0, usage: Usage::default(), done: false }
    }

    fn emit_usage(&self, out: &mut Vec<Result<StreamEvent, ApiError>>) {
        let u = &self.usage;
        if u.input_tokens > 0 || u.output_tokens > 0 {
            out.push(Ok(StreamEvent::Usage(u.clone())));
        }
    }

    fn interpret_sse(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>> {
        let mut out = Vec::new();

        if sse.data.is_empty() { return out; }
        if sse.data == "[DONE]" {
            self.finish_block(&mut out);
            self.emit_usage(&mut out);
            out.push(Ok(StreamEvent::Done));
            self.done = true;
            return out;
        }

        let chunk: Value = match serde_json::from_str(&sse.data) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to parse Gemini SSE payload: {}", e);
                return out;
            }
        };

        let response = chunk.get("response").unwrap_or(&chunk);

        if let Some(error) = chunk.get("error").or_else(|| response.get("error")) {
            // Flush any in-progress block, then surface this as a classified
            // error so a retryable rate-limit/overload here rides out through
            // the same policy as an HTTP error rather than ending the turn.
            self.finish_block(&mut out);
            let code = error.get("code").and_then(|c| c.as_u64()).unwrap_or(0) as u16;
            let body = serde_json::json!({ "error": error }).to_string();
            out.push(Err(classify_error(code, &body, None)));
            self.done = true;
            return out;
        }

        let candidate = match response.get("candidates").and_then(|c| c.get(0)) {
            Some(c) => c,
            None => return out,
        };

        if let Some(parts) = candidate.get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                self.process_part(part, &mut out);
            }
        }

        if let Some(um) = response.get("usageMetadata") {
            if let Some(n) = um["promptTokenCount"].as_u64() { self.usage.input_tokens = n; }
            if let Some(n) = um["candidatesTokenCount"].as_u64() { self.usage.output_tokens = n; }
            if let Some(n) = um["cachedContentTokenCount"].as_u64() { self.usage.cache_read_tokens = n; }
            self.usage.extras = Some(um.clone());
        }

        if let Some(reason) = candidate.get("finishReason").and_then(|r| r.as_str()) {
            self.finish_block(&mut out);
            self.emit_usage(&mut out);
            self.done = true;
            match reason {
                "STOP" | "MAX_TOKENS" => out.push(Ok(StreamEvent::Done)),
                other => {
                    out.push(Ok(StreamEvent::Error(format!("Gemini finish reason: {}", other))));
                    out.push(Ok(StreamEvent::Done));
                }
            }
        }

        out
    }

    fn process_part(&mut self, part: &Value, out: &mut Vec<Result<StreamEvent, ApiError>>) {
        let thought_sig: Option<String> = part.get("thoughtSignature")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());

        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            let is_thinking = part.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);

            if is_thinking {
                if !matches!(&self.current_block, Some(GeminiBlock::Thinking { .. })) {
                    self.finish_block(out);
                    self.current_block = Some(GeminiBlock::Thinking { sig: None });
                    out.push(Ok(StreamEvent::ThinkingStart));
                }
                if let Some(GeminiBlock::Thinking { sig }) = &mut self.current_block {
                    if thought_sig.is_some() { *sig = thought_sig.clone(); }
                }
                out.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
            } else {
                if !matches!(&self.current_block, Some(GeminiBlock::Text { .. })) {
                    self.finish_block(out);
                    self.current_block = Some(GeminiBlock::Text { sig: None });
                    out.push(Ok(StreamEvent::TextStart));
                }
                if let Some(GeminiBlock::Text { sig }) = &mut self.current_block {
                    if thought_sig.is_some() { *sig = thought_sig.clone(); }
                }
                out.push(Ok(StreamEvent::TextDelta(text.to_string())));
            }
        }

        if let Some(fc) = part.get("functionCall") {
            self.finish_block(out);

            let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let args = fc.get("args").cloned().unwrap_or(json!({}));

            self.tool_call_counter += 1;
            let tool_call_id = fc.get("id")
                .and_then(|i| i.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{}_{}", name, self.tool_call_counter));

            self.current_block = Some(GeminiBlock::ToolCall {
                id: tool_call_id.clone(),
                sig: thought_sig,
            });

            out.push(Ok(StreamEvent::ToolCallStart { id: tool_call_id.clone(), name }));

            let args_json = serde_json::to_string(&args).unwrap_or_default();
            out.push(Ok(StreamEvent::ToolCallDelta { id: tool_call_id, json_fragment: args_json }));

            self.finish_block(out);
        }
    }

    fn finish_block(&mut self, out: &mut Vec<Result<StreamEvent, ApiError>>) {
        if let Some(block) = self.current_block.take() {
            match block {
                GeminiBlock::Text { sig } => out.push(Ok(StreamEvent::TextEnd { sig })),
                GeminiBlock::Thinking { sig } => out.push(Ok(StreamEvent::ThinkingEnd {
                    replay: sig.map(ri::ThinkingReplay::Signature),
                })),
                GeminiBlock::ToolCall { id, sig } => out.push(Ok(StreamEvent::ToolCallEnd { id, sig })),
            }
        }
    }
}

impl SseInterpreter for GeminiState {
    fn interpret(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>> {
        self.interpret_sse(sse)
    }

    fn finish(&mut self) -> Vec<Result<StreamEvent, ApiError>> {
        if self.done { return Vec::new(); }
        let mut out = Vec::new();
        self.emit_usage(&mut out);
        out.push(Ok(StreamEvent::Done));
        out
    }
}

// -- System instructions --

const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = include_str!("prompts/antigravity_system.md");

/// Prompt injection to override Antigravity's baked-in system identity.
/// Without this, the model follows Google's hardcoded instructions (absolute paths,
/// web-app conventions, Antigravity branding) which conflict with ri's tool interface.
/// Fragile: depends on the model's willingness to honor overrides, which may change
/// between Gemini versions. If Antigravity starts ignoring this, the symptom will be
/// the model using absolute paths and calling itself "Antigravity."
const BRIDGE_PROMPT: &str = include_str!("prompts/antigravity_bridge.md");
