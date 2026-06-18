// Anthropic provider -- self-contained implementation.
//
// Handles: model catalog, credential management, request building,
// SSE interpretation, and login flow.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::error;

use ri::{
    ApiError, AuthMethod, ContentBlock, EventStream, LlmProvider, Message, Model, ModelCost,
    RequestOptions, Role, StreamEvent, ThinkingLevel, ToolSchema, Usage,
};
use crate::sse::{self, SseEvent, SseInterpreter};
use crate::creds::{self, CredsGuard, Credentials};
use crate::media;

fn creds_path() -> eyre::Result<PathBuf> {
    Ok(creds::ri_dir()?.join("anthropic_auth.json"))
}

fn load_creds() -> Option<Credentials> {
    creds::load(&creds_path().ok()?)
}

/// Read the Anthropic API key. `ANTHROPIC_API_KEY` wins; otherwise the OAuth
/// access token from disk. Returns `(key, is_oauth)` where `is_oauth` says
/// whether we should use the OAuth-specific headers and beta flags.
///
/// Always consults the source of truth (env + disk). There is no in-memory
/// cache to fossilize, so this is the only place that needs updating when
/// credentials change on disk.
fn read_api_key() -> (String, bool) {
    let env = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if !env.is_empty() {
        let is_oauth = env.starts_with("sk-ant-oat");
        return (env, is_oauth);
    }
    if let Some(creds) = load_creds() {
        let is_oauth = creds.access_token.starts_with("sk-ant-oat");
        return (creds.access_token, is_oauth);
    }
    (String::new(), false)
}

// -- PKCE + OAuth constants --

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

/// Hard cap on any OAuth token HTTP call. Refresh typically completes in
/// under a second; the cap exists so a stalled endpoint cannot hold the
/// cross-process refresh lock indefinitely.
const TOKEN_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// -- Provider struct --

pub struct AnthropicProvider {
    /// PKCE verifier and state live here across the `begin_login` ->
    /// `complete_login` HTTP round trip. Everything else -- the current
    /// access token, whether we're OAuth -- is read fresh from disk or env,
    /// so there is nothing else worth caching.
    login: Mutex<LoginInProgress>,
}

#[derive(Default)]
struct LoginInProgress {
    verifier: Option<String>,
    state: Option<String>,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self { login: Mutex::new(LoginInProgress::default()) }
    }

    /// Return a usable Anthropic API key plus whether it's an OAuth token.
    /// If the on-disk OAuth token is past its expiry buffer, refreshes it
    /// under the shared credential lock before returning.
    async fn ensure_valid_token(&self) -> eyre::Result<(String, bool)> {
        let (api_key, is_oauth) = read_api_key();
        if api_key.is_empty() || !is_oauth {
            return Ok((api_key, is_oauth));
        }

        let creds = match load_creds() {
            Some(c) => c,
            None => return Ok((api_key, true)),
        };
        if !creds.is_expired() {
            return Ok((creds.access_token, true));
        }

        // Slow path: token is past its buffer. Serialize with other refreshers
        // via a file lock on `<creds>.json.lock`, re-check disk (a sibling may
        // have refreshed while we waited), then refresh exactly once.
        //
        // If `fresh` exists but is also expired, it means another holder wrote
        // a fresh token that has since expired -- we must refresh with that
        // (newest) refresh token, not the one we originally loaded, because
        // the one we loaded may have been consumed and rotated away.
        let guard = CredsGuard::acquire(creds_path()?).await?;
        let mut current = creds;
        if let Some(fresh) = guard.read() {
            if !fresh.is_expired() {
                return Ok((fresh.access_token, true));
            }
            current = fresh;
        }
        let refreshed = refresh_token(&current).await
            .map_err(|e| eyre::eyre!(
                "Anthropic OAuth refresh failed -- re-login required ({e})"
            ))?;
        guard.write(&refreshed).await?;
        Ok((refreshed.access_token, true))
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &str { "anthropic" }
    fn name(&self) -> &str { "Anthropic" }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "claude-3-5-haiku-20241022".into(), name: "Claude Haiku 3.5".into(),
                reasoning: false, context_window: 200_000, max_tokens: 8_192,
                cost: ModelCost { input: 0.8, output: 4.0, cache_read: 0.08, cache_write: 1.0 },
            },
            Model {
                id: "claude-haiku-4-5-20251001".into(), name: "Claude Haiku 4.5".into(),
                reasoning: true, context_window: 200_000, max_tokens: 64_000,
                cost: ModelCost { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25 },
            },
            Model {
                id: "claude-sonnet-4-20250514".into(), name: "Claude Sonnet 4".into(),
                reasoning: true, context_window: 200_000, max_tokens: 64_000,
                cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
            },
            Model {
                id: "claude-sonnet-4-5-20250929".into(), name: "Claude Sonnet 4.5".into(),
                reasoning: true, context_window: 200_000, max_tokens: 64_000,
                cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
            },
            Model {
                id: "claude-sonnet-4-6".into(), name: "Claude Sonnet 4.6".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
            },
            Model {
                id: "claude-opus-4-6".into(), name: "Claude Opus 4.6".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 },
            },
            Model {
                id: "claude-opus-4-7".into(), name: "Claude Opus 4.7".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 },
            },
            Model {
                id: "claude-opus-4-8".into(), name: "Claude Opus 4.8".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 },
            },
            // Anthropic's most capable widely released model: a tier above Opus,
            // natively 1M-context with always-on adaptive thinking. (Its restricted
            // sibling Claude Mythos 5 is absent from this account's model list, so
            // listing it would only add an unusable, never-resolving entry.)
            Model {
                id: "claude-fable-5".into(), name: "Claude Fable 5".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 10.0, output: 50.0, cache_read: 1.0, cache_write: 12.5 },
            },
        ]
    }

    fn is_authenticated(&self) -> bool {
        !read_api_key().0.is_empty()
    }

    fn can_logout(&self) -> bool {
        // Env-var auth cannot be logged out; only file-based OAuth.
        std::env::var("ANTHROPIC_API_KEY").unwrap_or_default().is_empty()
            && load_creds().is_some()
    }

    async fn begin_login(&self) -> eyre::Result<Option<AuthMethod>> {
        let verifier = crate::creds::generate_verifier();
        let challenge = crate::creds::challenge(&verifier);
        let login_state = crate::creds::generate_verifier();

        let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &login_state);

        let mut login = self.login.lock().await;
        login.verifier = Some(verifier);
        login.state = Some(login_state);

        Ok(Some(AuthMethod::PasteCode { url: url.to_string() }))
    }

    async fn complete_login(&self, code: &str) -> eyre::Result<()> {
        let (verifier, login_state) = {
            let mut login = self.login.lock().await;
            let v = login.verifier.take()
                .ok_or_else(|| eyre::eyre!("No login in progress"))?;
            let s = login.state.take()
                .ok_or_else(|| eyre::eyre!("No login state"))?;
            (v, s)
        };

        let (actual_code, returned_state) = match code.split_once('#') {
            Some((c, s)) => (c, Some(s)),
            None => (code, None),
        };

        if let Some(rs) = returned_state {
            if rs != login_state {
                return Err(eyre::eyre!("OAuth state mismatch"));
            }
        }

        let client = reqwest::Client::builder()
            .timeout(TOKEN_HTTP_TIMEOUT)
            .build()?;
        let response = client
            .post(TOKEN_URL)
            .json(&json!({
                "grant_type": "authorization_code",
                "client_id": CLIENT_ID,
                "code": actual_code,
                "state": returned_state.unwrap_or(&login_state),
                "redirect_uri": REDIRECT_URI,
                "code_verifier": verifier,
            }))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
        }

        let body: Value = response.json().await?;
        let creds = parse_token_response(&body)?;
        let guard = CredsGuard::acquire(creds_path()?).await?;
        guard.write(&creds).await?;
        Ok(())
    }

    async fn logout(&self) -> eyre::Result<()> {
        if !std::env::var("ANTHROPIC_API_KEY").unwrap_or_default().is_empty() {
            return Err(eyre::eyre!(
                "Anthropic auth is via ANTHROPIC_API_KEY env var -- remove it to logout"
            ));
        }
        if let Ok(path) = creds_path() {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError> {
        let (api_key, is_oauth) = self.ensure_valid_token().await
            .map_err(|e| ApiError::other(format!("{e:#}")))?;
        let auth = if is_oauth { Auth::Oauth } else { Auth::ApiKey };

        let resolved = resolve_blobs(&opts).await;
        let request = build_request(&api_key, auth, &opts, &resolved);
        let bytes = sse::send(request, classify_error).await?;
        let state = AnthropicState::new(is_oauth, opts.tools.to_vec());
        Ok(Box::pin(sse::drive_sse_stream(bytes, state)))
    }
}

/// Maximum raw image size Anthropic accepts inline (~4.5 MB/image). Over this
/// is surfaced as a placeholder rather than risking a 413/400.
const ANTHROPIC_IMAGE_LIMIT: u64 = 4_500_000;

/// Maximum raw PDF size Anthropic accepts inline as a document (~30 MB).
const ANTHROPIC_PDF_LIMIT: u64 = 30 * 1024 * 1024;

/// Capability-aware resolution pass: `image/*` under the image cap -> inline
/// base64 (rendered as an image block); `application/pdf` under the PDF cap ->
/// inline base64 (rendered as a document block); audio/video, or anything over
/// a cap, -> placeholder. Anthropic has no upload API, so inline-or-placeholder
/// is the whole story.
async fn resolve_blobs(opts: &RequestOptions) -> media::ResolvedMap {
    let mut map = media::ResolvedMap::new();
    for (mime, hash, size) in media::collect_blobs(&opts.messages) {
        let is_image = mime.starts_with("image/");
        let is_pdf = mime == "application/pdf";
        let over_limit = (is_image && size > ANTHROPIC_IMAGE_LIMIT)
            || (is_pdf && size > ANTHROPIC_PDF_LIMIT);
        let resolved = if !is_image && !is_pdf {
            media::Resolved::Placeholder(format!(
                "[attachment {}, {} - this model can't read it]",
                mime, media::human_size(size)
            ))
        } else if over_limit {
            media::Resolved::Placeholder(format!(
                "[attachment {}, {} - exceeds this model's inline size limit]",
                mime, media::human_size(size)
            ))
        } else {
            match media::read_blob_b64(&opts.blobs, &hash).await {
                Some(b64) => media::Resolved::Inline { media_type: mime.clone(), b64 },
                None => media::Resolved::Placeholder(format!("[missing attachment {}]", mime)),
            }
        };
        map.insert(hash, resolved);
    }
    map
}

// -- Token handling --

async fn refresh_token(credentials: &Credentials) -> eyre::Result<Credentials> {
    let client = reqwest::Client::builder()
        .timeout(TOKEN_HTTP_TIMEOUT)
        .build()?;
    let response = client
        .post(TOKEN_URL)
        .json(&json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": credentials.refresh_token,
        }))
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token refresh failed ({}): {}", status, body));
    }

    let body: Value = response.json().await?;
    let refresh = body["refresh_token"].as_str()
        .unwrap_or(&credentials.refresh_token)
        .to_string();

    let mut access = body["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?
        .to_string();

    if !access.starts_with("sk-ant-oat") {
        access = format!("sk-ant-oat-{}", access);
    }

    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let expires = Credentials::compute_expiry(expires_in);

    Ok(Credentials { access_token: access, refresh_token: refresh, expires, project_id: None, email: None })
}

fn parse_token_response(body: &Value) -> eyre::Result<Credentials> {
    let mut access = body["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?
        .to_string();

    if !access.starts_with("sk-ant-oat") {
        access = format!("sk-ant-oat-{}", access);
    }

    let refresh = body["refresh_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing refresh_token"))?
        .to_string();

    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let expires = Credentials::compute_expiry(expires_in);

    Ok(Credentials { access_token: access, refresh_token: refresh, expires, project_id: None, email: None })
}

// -- Tool name mapping for OAuth (Claude Code compatibility) --

const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read", "Write", "Edit", "Bash", "Grep", "Glob",
    "AskUserQuestion", "EnterPlanMode", "ExitPlanMode",
    "KillShell", "NotebookEdit", "Skill", "Task",
    "TaskOutput", "TodoWrite", "WebFetch", "WebSearch",
];

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS.iter()
        .find(|cc| cc.eq_ignore_ascii_case(name))
        .map(|cc| cc.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, original_tools: &[ToolSchema]) -> String {
    original_tools.iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

// -- Request building --

fn build_request(api_key: &str, auth: Auth, opts: &RequestOptions, resolved: &media::ResolvedMap) -> reqwest::RequestBuilder {
    let body = build_body(opts, auth, resolved);
    let url = "https://api.anthropic.com/v1/messages";

    let beta_header = assemble_betas(&opts.model.id, auth).join(",");
    tracing::debug!(model = %opts.model.id, betas = %beta_header, "Anthropic request betas");
    tracing::trace!(url, %body, "Anthropic API request");

    let builder = reqwest::Client::new()
        .post(url)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("anthropic-beta", beta_header);

    match auth {
        Auth::Oauth => builder
            .header("authorization", format!("Bearer {api_key}"))
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("user-agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .body(body.to_string()),
        Auth::ApiKey => builder
            .header("x-api-key", api_key)
            .header("user-agent", RI_USER_AGENT)
            .body(body.to_string()),
    }
}

/// Which credential a request authenticates with. The OAuth path impersonates
/// Claude Code -- it carries the `claude-code`/`oauth` betas, the "You are
/// Claude Code" system prefix, and Claude Code tool names -- so the generation
/// we present must line up with the beta envelope we send. The api-key path is
/// a plain first-party client and skips all of that.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    ApiKey,
    Oauth,
}

/// User-Agent presented when impersonating Claude Code over OAuth. Pinned to
/// the Claude Code release whose beta envelope `assemble_betas` mirrors; bump
/// the two together so the version and the betas stay one coherent contract.
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.170 (external, cli)";

/// User-Agent for direct api-key requests; ri has no reason to hide here.
const RI_USER_AGENT: &str = concat!("ri/", env!("CARGO_PKG_VERSION"));

// The `anthropic-beta` opt-ins ri knows about: each value is the literal token
// Anthropic parses, the const name is its meaning. Claude Code keeps a 26-entry
// registry, but most of those gate on a backend (bedrock/vertex/foundry) or an
// org feature-flag that ri does not have -- ri only ever talks to the
// first-party API -- so modelling them here would be machinery with nothing
// behind it. We carry exactly the ones a first-party request can earn.
const CLAUDE_CODE: &str = "claude-code-20250219";
const OAUTH_AUTH: &str = "oauth-2025-04-20";
const INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
const PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";
const MID_CONVERSATION_SYSTEM: &str = "mid-conversation-system-2026-04-07";

/// Assemble the `anthropic-beta` opt-ins for one request, mirroring the slice
/// of Claude Code's per-request assembler that actually applies to ri:
/// first-party transport only, gated by model id and credential. The push
/// order follows Claude Code so a wire diff against the real client stays small.
fn assemble_betas(model_id: &str, auth: Auth) -> Vec<String> {
    let mut betas: Vec<String> = Vec::new();

    // `claude-code` tags agentic-coding traffic; Claude Code omits it for Haiku
    // (`!isHaiku`). ri sends it only while impersonating Claude Code (OAuth),
    // paired with the "You are Claude Code" system prefix build_body adds on
    // that same path.
    if auth == Auth::Oauth && !model_id.contains("haiku") {
        betas.push(CLAUDE_CODE.into());
    }
    if auth == Auth::Oauth {
        betas.push(OAUTH_AUTH.into());
    }
    // Interleaved thinking: Claude Code's `tf$` on a first-party backend (ri is
    // always first-party) reduces to "every model except the claude-3 family" --
    // Haiku 4.5 included, verified against the live API. The `claude-haiku-4-5`
    // carve-out in `tf$` only fires on non-first-party backends (bedrock, vertex,
    // gateway), which ri never touches. Behind Claude Code's own kill-switch so a
    // runaway turn can be reined in without a rebuild.
    if !model_id.contains("claude-3-") && !env_flag("DISABLE_INTERLEAVED_THINKING") {
        betas.push(INTERLEAVED_THINKING.into());
    }
    // Claude Code sends this on every first-party request, and ri is always
    // first-party. ri's body-root cache_control is honored without it, but it
    // keeps the envelope aligned with the client we impersonate.
    betas.push(PROMPT_CACHING_SCOPE.into());
    // Mid-stream system blocks: Claude Code opts Opus 4.8 and Fable 5 specifically
    // into this (not the wider Opus line). ri hoists all system content to the top
    // of the request rather than weaving it in, so this is inert today -- but it
    // mirrors the envelope these models were aligned with, and would switch on for
    // free the day ri starts weaving system blocks into the message stream.
    if model_id.contains("opus-4-8") || model_id.contains("fable") {
        betas.push(MID_CONVERSATION_SYSTEM.into());
    }
    // Manual escape hatch, appended verbatim (no dedup, matching Claude Code's
    // ANTHROPIC_BETAS).
    if let Ok(extra) = std::env::var("ANTHROPIC_BETAS") {
        betas.extend(extra.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from));
    }
    betas
}

/// Whether an env kill-switch is set to a meaningful "on" value. Empty, `0`, and
/// `false` (any case) all read as off, so an accidental bare `VAR=` does not
/// silently flip behavior.
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn build_body(opts: &RequestOptions, auth: Auth, resolved: &media::ResolvedMap) -> Value {
    let oauth = auth == Auth::Oauth;

    // Only emit structured tool protocol for complete call+result pairs.
    // Orphaned tool blocks from cross-provider contexts are filtered out.
    let complete = ri::complete_tool_pairs(&opts.messages);

    let messages: Vec<Value> = opts.messages.iter()
        .filter(|m| m.role != Role::System)
        .map(|m| convert_message(m, &complete, resolved))
        .collect();

    let max_tokens = opts.max_tokens
        .unwrap_or_else(|| (opts.model.max_tokens / 3).max(4096));

    let mut body = json!({
        "model": opts.model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        "cache_control": { "type": "ephemeral" },
    });

    if oauth {
        let mut system_blocks = vec![
            json!({ "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." }),
        ];
        if !opts.system_prompt.trim().is_empty() {
            system_blocks.push(json!({ "type": "text", "text": opts.system_prompt }));
        }
        body["system"] = json!(system_blocks);
    } else if !opts.system_prompt.trim().is_empty() {
        body["system"] = json!([{ "type": "text", "text": opts.system_prompt }]);
    }

    if !opts.tools.is_empty() {
        let tools: Vec<Value> = opts.tools.iter().map(|t| {
            let name = if oauth { to_claude_code_name(&t.name) } else { t.name.clone() };
            json!({
                "name": name,
                "description": t.description,
                "input_schema": {
                    "type": "object",
                    "properties": t.parameters.get("properties").unwrap_or(&json!({})),
                    "required": t.parameters.get("required").unwrap_or(&json!([]))
                }
            })
        }).collect();
        body["tools"] = json!(tools);
    }

    if opts.thinking != ThinkingLevel::Off && opts.model.reasoning {
        apply_thinking(&mut body, opts.thinking, thinking_mode(&opts.model.id));
    }

    body
}

/// Which thinking-config shape a model accepts. Anthropic split the API
/// in February 2026: newer hybrid-reasoning models (Opus 4.6+, Sonnet 4.6+)
/// take an adaptive mode with an effort suggestion and decide for themselves
/// how much to think; older ones still require a hard `budget_tokens` ceiling.
enum ThinkingMode {
    Adaptive,
    Budget,
}

fn thinking_mode(model_id: &str) -> ThinkingMode {
    // Claude Code's `gH8`: the hybrid-reasoning generation takes adaptive
    // thinking; everything older requires a hard `budget_tokens` ceiling.
    // Substring matching so a dated id (`claude-opus-4-8-2026...`) classifies the
    // same as its canonical form, and anything unknown stays on budget.
    let adaptive = ["opus-4-6", "opus-4-7", "opus-4-8", "sonnet-4-6", "fable"]
        .iter()
        .any(|family| model_id.contains(family));
    if adaptive { ThinkingMode::Adaptive } else { ThinkingMode::Budget }
}

fn apply_thinking(body: &mut Value, level: ThinkingLevel, mode: ThinkingMode) {
    match mode {
        ThinkingMode::Adaptive => {
            let effort = match level {
                ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High => "high",
                ThinkingLevel::XHigh => "max",
                ThinkingLevel::Off => unreachable!("guarded by caller"),
            };
            // `display: "summarized"` opts in to the server-side reasoning
            // summary. Required for Opus 4.7 thinking to stream at all;
            // older adaptive models accept it verbatim.
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            body["output_config"] = json!({ "effort": effort });
        }
        ThinkingMode::Budget => {
            let budget = match level {
                ThinkingLevel::Low => 1024,
                ThinkingLevel::Medium => 4096,
                ThinkingLevel::High => 16384,
                ThinkingLevel::XHigh => 32768,
                ThinkingLevel::Off => unreachable!("guarded by caller"),
            };
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
    }
}

fn convert_message(msg: &Message, complete: &std::collections::HashSet<&str>, resolved: &media::ResolvedMap) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        // build_body hoists every Role::System into the top-level `system` field
        // and filters it out before this map, so reaching here means that
        // contract broke upstream. Surface it loudly rather than silently
        // demoting a system message to a user turn (the original bug here).
        Role::System => unreachable!("Role::System is hoisted into the top-level system field by build_body and filtered before convert_message"),
    };

    // Convert blocks, downgrading orphaned tool blocks to text.
    let content: Vec<Value> = msg.content.iter()
        .filter(|c| !matches!(c, ContentBlock::Error { .. }))
        .filter_map(|c| match c {
            ContentBlock::ToolUse { id, .. } if !complete.contains(id.as_str()) => {
                c.tool_as_text().map(|t| json!({ "type": "text", "text": t }))
            }
            ContentBlock::ToolResult { tool_use_id, .. } if !complete.contains(tool_use_id.as_str()) => {
                c.tool_as_text().map(|t| json!({ "type": "text", "text": t }))
            }
            _ => Some(convert_content(c, resolved)),
        })
        .collect();
    let has_tool_results = msg.content.iter().any(|c| {
        matches!(c, ContentBlock::ToolResult { tool_use_id, .. } if complete.contains(tool_use_id.as_str()))
    });
    let effective_role = if has_tool_results { "user" } else { role };

    json!({ "role": effective_role, "content": content })
}

fn convert_content(c: &ContentBlock, resolved: &media::ResolvedMap) -> Value {
    match c {
        ContentBlock::Text { text, .. } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { thinking, replay } => {
            if let Some(ri::ThinkingReplay::Signature(s)) = replay {
                json!({ "type": "thinking", "thinking": thinking, "signature": s })
            } else {
                // No signature or encrypted blob from another provider -- fall back to text.
                json!({ "type": "text", "text": thinking })
            }
        }
        ContentBlock::Blob { hash, .. } => match resolved.get(hash) {
            // PDF inline -> document block; everything else inline -> image block.
            Some(media::Resolved::Inline { media_type, b64 }) if media_type == "application/pdf" => json!({
                "type": "document",
                "source": { "type": "base64", "media_type": "application/pdf", "data": b64 }
            }),
            Some(media::Resolved::Inline { media_type, b64 }) => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": b64 }
            }),
            // FileUri is not an Anthropic concept; surface as text. Placeholder
            // and a missing entry both degrade to descriptive text -- never a drop.
            Some(media::Resolved::FileUri { .. }) | None => {
                json!({ "type": "text", "text": c.as_text_or_placeholder() })
            }
            Some(media::Resolved::Placeholder(t)) => json!({ "type": "text", "text": t }),
        },
        ContentBlock::ToolUse { id, name, input, .. } => json!({
            "type": "tool_use", "id": id, "name": name, "input": input
        }),
        ContentBlock::ToolResult { tool_use_id, content, is_error, .. } => {
            let content_json: Vec<Value> = content.iter().map(|b| convert_content(b, resolved)).collect();
            json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content_json,
                "is_error": is_error,
            })
        }
        ContentBlock::Error { message, .. } => json!({
            "type": "text",
            "text": format!("[Error: {}]", message)
        }),
        ContentBlock::Unknown(v) => v.clone(),
    }
}

/// Map an Anthropic error response to an `ApiError`. Anthropic's transient
/// failures are 429 (`rate_limit_error`), 529 (`overloaded_error`), and 5xx
/// server errors; its retry delay arrives in the `retry-after` header (passed
/// here as `retry_after_ms`). A permanent quota is a 402, not a 429, so every
/// rate limit here is a transient throttle worth retrying. Used for both the
/// HTTP response (status set) and a mid-stream `error` event (status 0, where
/// `error.type` carries the verdict).
fn classify_error(status: u16, body: &str, retry_after_ms: Option<u64>) -> ApiError {
    let error_type = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|p| p["error"]["type"].as_str().map(String::from));

    let retryable = status == 429
        || status == 529
        || (500..=599).contains(&status)
        || matches!(
            error_type.as_deref(),
            Some("rate_limit_error" | "overloaded_error" | "api_error")
        );

    let err = sse::HttpApiError { status, body: body.to_string() };
    if retryable {
        ApiError::retryable(retry_after_ms.unwrap_or(0), err)
    } else {
        ApiError::other(err)
    }
}

// -- SSE interpretation --

#[derive(Debug)]
enum AnthropicBlock {
    Text,
    Thinking,
    ToolUse { id: String },
}

struct AnthropicState {
    blocks: HashMap<usize, AnthropicBlock>,
    signatures: HashMap<usize, String>,
    usage: Usage,
    raw_usage: serde_json::Map<String, Value>,
    is_oauth: bool,
    original_tools: Vec<ToolSchema>,
}

impl AnthropicState {
    fn new(is_oauth: bool, original_tools: Vec<ToolSchema>) -> Self {
        Self {
            blocks: HashMap::new(),
            signatures: HashMap::new(),
            usage: Usage::default(),
            raw_usage: serde_json::Map::new(),
            is_oauth,
            original_tools,
        }
    }

    /// Remap Claude Code tool names back to our original names when using OAuth.
    fn remap_tool_name(&self, name: String) -> String {
        if !self.is_oauth { return name; }
        from_claude_code_name(&name, &self.original_tools)
    }
}

impl SseInterpreter for AnthropicState {
    fn interpret(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>> {
        let mut out = Vec::new();

        match sse.event_type.as_str() {
            "content_block_start" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::other(format!("stream parse error (content_block_start): {e}"))));
                        return out;
                    }
                };
                let block_type = parsed["content_block"]["type"].as_str().unwrap_or("");
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;

                match block_type {
                    "text" => {
                        self.blocks.insert(index, AnthropicBlock::Text);
                        out.push(Ok(StreamEvent::TextStart));
                    }
                    "thinking" => {
                        self.blocks.insert(index, AnthropicBlock::Thinking);
                        out.push(Ok(StreamEvent::ThinkingStart));
                    }
                    "tool_use" => {
                        let id = parsed["content_block"]["id"].as_str().unwrap_or("").to_string();
                        let raw_name = parsed["content_block"]["name"].as_str().unwrap_or("").to_string();
                        let name = self.remap_tool_name(raw_name);
                        self.blocks.insert(index, AnthropicBlock::ToolUse { id: id.clone() });
                        out.push(Ok(StreamEvent::ToolCallStart { id, name }));
                    }
                    other => { error!("Unknown content block type: [{}]", other); }
                }
            }

            "content_block_delta" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::other(format!("stream parse error (content_block_delta): {e}"))));
                        return out;
                    }
                };
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                let delta_type = parsed["delta"]["type"].as_str().unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = parsed["delta"]["text"].as_str().unwrap_or("").to_string();
                        out.push(Ok(StreamEvent::TextDelta(text)));
                    }
                    "thinking_delta" => {
                        let text = parsed["delta"]["thinking"].as_str().unwrap_or("").to_string();
                        out.push(Ok(StreamEvent::ThinkingDelta(text)));
                    }
                    "input_json_delta" => {
                        let partial = parsed["delta"]["partial_json"].as_str().unwrap_or("").to_string();
                        if let Some(AnthropicBlock::ToolUse { id }) = self.blocks.get(&index) {
                            out.push(Ok(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                json_fragment: partial,
                            }));
                        }
                    }
                    "signature_delta" => {
                        let sig = parsed["delta"]["signature"].as_str().unwrap_or("");
                        self.signatures.entry(index).or_default().push_str(sig);
                    }
                    other => { error!("Unknown delta type: [{}]", other); }
                }
            }

            "content_block_stop" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::other(format!("stream parse error (content_block_stop): {e}"))));
                        return out;
                    }
                };
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                let sig = self.signatures.remove(&index).filter(|s| !s.is_empty());
                if let Some(block) = self.blocks.get(&index) {
                    match block {
                        AnthropicBlock::Text => out.push(Ok(StreamEvent::TextEnd { sig: sig.clone() })),
                        AnthropicBlock::Thinking => out.push(Ok(StreamEvent::ThinkingEnd {
                            replay: sig.map(ri::ThinkingReplay::Signature),
                        })),
                        AnthropicBlock::ToolUse { id } => {
                            out.push(Ok(StreamEvent::ToolCallEnd { id: id.clone(), sig }));
                        }
                    }
                }
            }

            "message_start" => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&sse.data) {
                    if let Some(usage) = parsed.get("message").and_then(|m| m.get("usage")) {
                        // Anthropic reports input_tokens as non-cached only.
                        // Normalize to total prompt context (matching Gemini's promptTokenCount).
                        let base = usage["input_tokens"].as_u64().unwrap_or(0);
                        let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                        let cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                        self.usage.input_tokens = base + cache_read + cache_write;
                        self.usage.cache_read_tokens = cache_read;
                        self.usage.cache_write_tokens = cache_write;
                        // Merge all fields into raw_usage for debug display.
                        if let Some(obj) = usage.as_object() {
                            for (k, v) in obj {
                                self.raw_usage.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }

            "message_delta" => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&sse.data) {
                    if let Some(usage) = parsed.get("usage") {
                        if let Some(n) = usage["output_tokens"].as_u64() { self.usage.output_tokens = n; }
                        if let Some(obj) = usage.as_object() {
                            for (k, v) in obj {
                                self.raw_usage.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }

            "ping" => {}
            "message_stop" => {
                if !self.raw_usage.is_empty() {
                    self.usage.extras = Some(Value::Object(self.raw_usage.clone()));
                }
                let u = &self.usage;
                if u.input_tokens > 0 || u.output_tokens > 0 {
                    out.push(Ok(StreamEvent::Usage(u.clone())));
                }
                out.push(Ok(StreamEvent::Done));
            }

            "error" => {
                // A mid-stream error event -- classify it with the same shared
                // policy as an HTTP error so an `overloaded_error` or
                // `rate_limit_error` here rides out like its status-code twin.
                out.push(Err(classify_error(0, &sse.data, None)));
            }

            other => { error!("Unknown SSE event type: [{}]", other); }
        }

        out
    }
}
