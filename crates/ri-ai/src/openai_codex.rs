// OpenAI Codex provider -- ChatGPT subscription access to GPT-5.x models.
//
// Uses the OpenAI Responses API via `chatgpt.com/backend-api/codex/responses`.
// Authentication is OAuth2 PKCE via auth.openai.com with a local HTTP callback.
// The access token is a JWT embedding a `chatgpt_account_id` claim required
// as a header on every request.
//
// The Responses API differs from Chat Completions:
//   - System prompt goes in `instructions`, not in messages
//   - Messages use `input` array with typed items (input_text, function_call, etc.)
//   - Reasoning is encrypted; the full item JSON is stored as a "signature"
//     and replayed verbatim on future requests
//   - Tool call IDs are compound: `call_id|item_id`
//   - SSE events carry their type inside the JSON `data` payload, not in `event:`

use std::path::PathBuf;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use ri::{
    ApiError, AuthMethod, ContentBlock, EventStream, LlmProvider, Message, Model, ModelCost,
    RequestOptions, Role, StreamEvent, ThinkingLevel, Usage,
};
use crate::sse::{self, SseEvent, SseInterpreter};
use crate::creds::{self, Credentials};

// -- Constants --

const BASE_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";

const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";

/// JWT claim path where OpenAI stores account metadata.
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 1000;

// -- Credential storage --

fn creds_path() -> eyre::Result<PathBuf> {
    Ok(creds::ri_dir()?.join("openai_codex_auth.json"))
}

fn load_creds() -> Option<Credentials> {
    creds::load(&creds_path().ok()?)
}

fn save_creds(c: &Credentials) -> eyre::Result<()> {
    creds::save(&creds_path()?, c)
}

// -- Provider --

pub struct OpenAICodexProvider {
    state: Mutex<ProviderState>,
}

struct ProviderState {
    access_token: String,
    account_id: String,
    // PKCE for in-progress login
    login_verifier: Option<String>,
    login_state: Option<String>,
}

impl OpenAICodexProvider {
    pub fn new() -> Self {
        let (access_token, account_id) = if let Some(creds) = load_creds() {
            let account_id = extract_account_id(&creds.access_token).unwrap_or_default();
            (creds.access_token, account_id)
        } else {
            (String::new(), String::new())
        };

        Self {
            state: Mutex::new(ProviderState {
                access_token,
                account_id,
                login_verifier: None,
                login_state: None,
            }),
        }
    }

    /// Ensure we have a valid (non-expired) token, refreshing if needed.
    async fn ensure_valid_token(&self) -> eyre::Result<(String, String)> {
        let mut state = self.state.lock().await;
        if state.access_token.is_empty() {
            return Ok((String::new(), String::new()));
        }

        if let Some(creds) = load_creds() {
            if creds.is_expired() {
                match refresh_token(&creds).await {
                    Ok(refreshed) => {
                        state.access_token = refreshed.access_token.clone();
                        state.account_id = extract_account_id(&refreshed.access_token)
                            .unwrap_or_default();
                        let _ = save_creds(&refreshed);
                    }
                    Err(e) => {
                        tracing::warn!("OpenAI Codex token refresh failed: {}", e);
                    }
                }
            }
        }

        Ok((state.access_token.clone(), state.account_id.clone()))
    }
}

#[async_trait]
impl LlmProvider for OpenAICodexProvider {
    fn id(&self) -> &str { "openai-codex" }
    fn name(&self) -> &str { "ChatGPT Codex" }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "gpt-5.2".into(), name: "GPT-5.2".into(),
                reasoning: true, context_window: 400_000, max_tokens: 128_000,
                cost: ModelCost { input: 1.75, output: 14.0, cache_read: 0.175, cache_write: 0.0 },
            },
            Model {
                id: "gpt-5.2-codex".into(), name: "GPT-5.2 Codex".into(),
                reasoning: true, context_window: 400_000, max_tokens: 128_000,
                cost: ModelCost { input: 1.75, output: 14.0, cache_read: 0.175, cache_write: 0.0 },
            },
            Model {
                id: "gpt-5.3-codex".into(), name: "GPT-5.3 Codex".into(),
                reasoning: true, context_window: 400_000, max_tokens: 128_000,
                cost: ModelCost { input: 1.75, output: 14.0, cache_read: 0.175, cache_write: 0.0 },
            },
        ]
    }

    fn is_authenticated(&self) -> bool {
        self.state.try_lock().map(|s| !s.access_token.is_empty()).unwrap_or(false)
    }

    fn can_logout(&self) -> bool {
        load_creds().is_some()
    }

    async fn begin_login(&self) -> eyre::Result<Option<AuthMethod>> {
        let verifier = creds::generate_verifier();
        let challenge = creds::challenge(&verifier);
        let login_state = creds::generate_verifier();

        let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &login_state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "ri");

        let mut state = self.state.lock().await;
        state.login_verifier = Some(verifier);
        state.login_state = Some(login_state);
        drop(state);

        Ok(Some(AuthMethod::LocalCallback {
            url: url.to_string(),
            port: CALLBACK_PORT,
            path: CALLBACK_PATH.to_string(),
        }))
    }

    async fn complete_login(&self, code: &str) -> eyre::Result<()> {
        let (verifier, login_state) = {
            let mut state = self.state.lock().await;
            let v = state.login_verifier.take()
                .ok_or_else(|| eyre::eyre!("No login in progress"))?;
            let s = state.login_state.take()
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

        let creds = exchange_code(&actual_code, &verifier).await?;
        let account_id = extract_account_id(&creds.access_token)
            .ok_or_else(|| eyre::eyre!("Failed to extract account ID from token"))?;

        save_creds(&creds)?;

        let mut state = self.state.lock().await;
        state.access_token = creds.access_token;
        state.account_id = account_id;

        Ok(())
    }

    async fn logout(&self) -> eyre::Result<()> {
        if let Ok(path) = creds_path() {
            let _ = std::fs::remove_file(&path);
        }
        let mut state = self.state.lock().await;
        state.access_token.clear();
        state.account_id.clear();
        Ok(())
    }

    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError> {
        let (token, account_id) = self.ensure_valid_token().await
            .map_err(|e| ApiError::other(format!("{e:#}")))?;

        if token.is_empty() {
            return Err(ApiError::other("Not authenticated with OpenAI Codex"));
        }

        let prompt_cache_key = derive_prompt_cache_key(&opts.messages);
        let body = build_body(&opts, prompt_cache_key.as_deref());
        let response = send_with_retry(
            &token,
            &account_id,
            &body,
            prompt_cache_key.as_deref(),
        ).await?;

        let bytes = response.bytes_stream();
        let interpreter = CodexState::new();
        Ok(Box::pin(sse::drive_sse_stream(bytes, interpreter)))
    }
}

// -- Token exchange & refresh --

/// Exchange an authorization code for tokens. Uses form-urlencoded (not JSON).
async fn exchange_code(code: &str, verifier: &str) -> eyre::Result<Credentials> {
    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
    }

    let body: Value = response.json().await?;
    parse_token_response(&body)
}

async fn refresh_token(credentials: &Credentials) -> eyre::Result<Credentials> {
    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", credentials.refresh_token.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token refresh failed ({}): {}", status, body));
    }

    let body: Value = response.json().await?;
    parse_token_response(&body)
}

fn parse_token_response(body: &Value) -> eyre::Result<Credentials> {
    let access = body["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token in response"))?
        .to_string();

    let refresh = body["refresh_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing refresh_token in response"))?
        .to_string();

    let expires_in = body["expires_in"].as_u64()
        .ok_or_else(|| eyre::eyre!("Missing expires_in in response"))?;

    Ok(Credentials {
        access_token: access,
        refresh_token: refresh,
        expires: Credentials::compute_expiry(expires_in),
        project_id: None,
        email: None,
    })
}

// -- JWT account ID extraction --

fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 { return None; }

    // JWT payload is base64url-encoded. The spec says no padding, but some
    // issuers include it. Strip trailing '=' to handle both cases.
    use base64::Engine;
    let payload = parts[1].trim_end_matches('=');
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload).ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;

    payload.get(JWT_CLAIM_PATH)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|id| id.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

// -- HTTP request with retry --

fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

async fn send_with_retry(
    token: &str,
    account_id: &str,
    body: &Value,
    session_id: Option<&str>,
) -> Result<reqwest::Response, ApiError> {
    let client = reqwest::Client::new();
    let body_str = body.to_string();

    tracing::trace!("Codex request body: {}", body_str);

    let mut last_error = None;
    for attempt in 0..=MAX_RETRIES {
        let mut request = client
            .post(BASE_URL)
            .header("authorization", format!("Bearer {}", token))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "ri")
            .header("user-agent", "ri")
            .header("accept", "text/event-stream")
            .header("content-type", "application/json");

        if let Some(id) = session_id.filter(|s| !s.is_empty()) {
            request = request.header("session_id", id);
        }

        let result = request
            .body(body_str.clone())
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                if status < 400 {
                    return Ok(response);
                }

                let error_text = response.text().await.unwrap_or_default();

                if attempt < MAX_RETRIES && is_retryable(status) {
                    let delay = BASE_DELAY_MS * 2u64.pow(attempt);
                    tracing::info!("Codex returned {}, retrying in {}ms (attempt {})", status, delay, attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }

                return Err(parse_codex_error(status, &error_text));
            }
            Err(e) => {
                let detail = e.to_string();
                last_error = Some(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                if attempt < MAX_RETRIES {
                    let delay = BASE_DELAY_MS * 2u64.pow(attempt);
                    tracing::info!("Codex network error, retrying in {}ms (attempt {}): {detail}", delay, attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
            }
        }
    }

    Err(ApiError::other(last_error.unwrap_or_else(|| "Failed after retries".into())))
}

fn parse_codex_error(status: u16, body: &str) -> ApiError {
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        if let Some(err) = parsed.get("error") {
            let code = err["code"].as_str()
                .or_else(|| err["type"].as_str())
                .unwrap_or("");

            // Usage limit / rate limit errors
            if code.contains("usage_limit") || code.contains("rate_limit") || status == 429 {
                let message = err["message"].as_str().unwrap_or(body);
                return ApiError::rate_limited(60_000, format!("HTTP {status}: {message}"));
            }

            let message = err["message"].as_str().unwrap_or(body).to_string();
            return ApiError::other(format!("HTTP {status}: {message}"));
        }
    }

    if status == 429 {
        return ApiError::rate_limited(60_000, format!("HTTP {status}: {body}"));
    }

    ApiError::other(format!("HTTP {status}: {body}"))
}

// -- Request body --

fn build_body(opts: &RequestOptions, prompt_cache_key: Option<&str>) -> Value {
    let messages = build_input_messages(&opts.messages);

    let mut body = json!({
        "model": opts.model.id,
        "store": false,
        "stream": true,
        "instructions": opts.system_prompt,
        "input": messages,
        "text": { "verbosity": "medium" },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });

    if !opts.tools.is_empty() {
        let tools: Vec<Value> = opts.tools.iter().map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "strict": Value::Null,
            })
        }).collect();
        body["tools"] = json!(tools);
    }

    if let Some(key) = prompt_cache_key.filter(|s| !s.is_empty()) {
        body["prompt_cache_key"] = json!(key);
    }

    if opts.thinking != ThinkingLevel::Off && opts.model.reasoning {
        let effort = match opts.thinking {
            ThinkingLevel::Off => unreachable!(),
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh => "xhigh",
        };
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }

    body
}

/// Derive a stable cache key for a conversation from the earliest non-system message.
///
/// Codex prompt caching benefits from a stable `prompt_cache_key` and matching
/// `session_id` header across turns. We use the first non-system message ID as a
/// deterministic per-conversation anchor.
fn derive_prompt_cache_key(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|m| m.role != Role::System)
        .or_else(|| messages.first())
        .map(|m| m.id.to_string())
}

// -- Message conversion (ri messages -> Responses API input) --

fn build_input_messages(messages: &[Message]) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    let mut text_block_counter = 0usize;

    for msg in messages {
        if msg.role == Role::System { continue; }

        match msg.role {
            Role::User => {
                let content: Vec<Value> = msg.content.iter().filter_map(|block| {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            Some(json!({ "type": "input_text", "text": text }))
                        }
                        ContentBlock::Image { media_type, data } => {
                            Some(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", media_type, data),
                            }))
                        }
                        _ => None,
                    }
                }).collect();

                if !content.is_empty() {
                    input.push(json!({ "role": "user", "content": content }));
                }
            }

            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Thinking { sig, .. } => {
                            // Reasoning blocks: replay the stored signature (encrypted item JSON).
                            // If no signature, skip -- we can't fabricate encrypted reasoning.
                            if let Some(sig) = sig {
                                if let Ok(item) = serde_json::from_str::<Value>(sig) {
                                    input.push(item);
                                }
                            }
                        }
                        ContentBlock::Text { text, .. } => {
                            let msg_id = format!("msg_{}", text_block_counter);
                            text_block_counter += 1;
                            input.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                                "status": "completed",
                                "id": msg_id,
                            }));
                        }
                        ContentBlock::ToolUse { id, name, input: args, .. } => {
                            let (call_id, item_id) = split_compound_id(id);
                            input.push(json!({
                                "type": "function_call",
                                "id": item_id,
                                "call_id": call_id,
                                "name": name,
                                "arguments": serde_json::to_string(args).unwrap_or_default(),
                            }));
                        }
                        _ => {}
                    }
                }
            }

            Role::System => {} // Filtered above, but satisfy the match.
        }

        // Tool results: these live in user messages in ri's model (role=User
        // with ToolResult blocks), but need to become function_call_output items.
        if msg.role == Role::User || msg.role == Role::Assistant {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
                    let text: String = content.iter().filter_map(|b| {
                        if let ContentBlock::Text { text, .. } = b { Some(text.as_str()) } else { None }
                    }).collect::<Vec<_>>().join("\n");

                    let (call_id, _) = split_compound_id(tool_use_id);
                    // Codex has no is_error field on function_call_output; errors
                    // are already expressed in the text content itself.
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": if text.is_empty() { "(empty)".to_string() } else { text },
                    }));
                }
            }
        }
    }

    input
}

/// Split a compound tool call ID (`call_id|item_id`) into its parts.
/// If not compound, treats the whole string as call_id with no item_id.
fn split_compound_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once('|') {
        Some((call, item)) => (call, Some(item)),
        None => (id, None),
    }
}

// -- SSE interpretation --
//
// The Codex/Responses API sends SSE where the event type is inside the JSON
// payload's `type` field, not in the SSE `event:` line. Our SseInterpreter
// receives these with an empty event_type and must dispatch on the parsed JSON.

struct CodexState {
    /// Currently open block type for tracking start/end boundaries.
    current: Option<CodexBlock>,
    /// Accumulated reasoning summary text for the current thinking block.
    thinking_buf: String,
    /// The full reasoning item JSON, stored for replay as a signature.
    reasoning_item: Option<Value>,
    /// Current function_call partial JSON accumulator.
    tool_json_buf: String,
    /// Usage from response.completed event.
    usage: Usage,
    done: bool,
}

#[derive(Debug)]
enum CodexBlock {
    Reasoning,
    Message,
    FunctionCall { call_id: String, item_id: String },
}

impl CodexState {
    fn new() -> Self {
        Self {
            current: None,
            thinking_buf: String::new(),
            reasoning_item: None,
            tool_json_buf: String::new(),
            usage: Usage::default(),
            done: false,
        }
    }

    fn interpret_event(&mut self, data: &str) -> Vec<Result<StreamEvent, ApiError>> {
        if data.is_empty() || data == "[DONE]" {
            return self.finalize();
        }

        let event: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(), // Silently skip unparseable lines
        };

        let event_type = match event["type"].as_str() {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut out = Vec::new();

        match event_type {
            // -- Error events --
            "error" => {
                let code = event["code"].as_str().unwrap_or("");
                let message = event["message"].as_str().unwrap_or("");
                let msg = if !message.is_empty() { message } else { code };
                out.push(Err(ApiError::other(format!("Codex error: {msg}"))));
            }

            "response.failed" => {
                let msg = event.pointer("/response/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Codex response failed");
                out.push(Err(ApiError::other(msg)));
            }

            // -- Item lifecycle --
            "response.output_item.added" => {
                let item = &event["item"];
                let item_type = item["type"].as_str().unwrap_or("");

                match item_type {
                    "reasoning" => {
                        self.finish_current(&mut out);
                        self.current = Some(CodexBlock::Reasoning);
                        self.thinking_buf.clear();
                        self.reasoning_item = None;
                        out.push(Ok(StreamEvent::ThinkingStart));
                    }
                    "message" => {
                        self.finish_current(&mut out);
                        self.current = Some(CodexBlock::Message);
                        out.push(Ok(StreamEvent::TextStart));
                    }
                    "function_call" => {
                        self.finish_current(&mut out);
                        let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let compound_id = format!("{}|{}", call_id, item_id);

                        self.tool_json_buf.clear();
                        // Seed with any arguments already present in the added event
                        if let Some(args) = item["arguments"].as_str() {
                            self.tool_json_buf.push_str(args);
                        }

                        self.current = Some(CodexBlock::FunctionCall {
                            call_id, item_id,
                        });
                        out.push(Ok(StreamEvent::ToolCallStart { id: compound_id, name }));
                    }
                    _ => {}
                }
            }

            // -- Reasoning summary streaming --
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.thinking_buf.push_str(delta);
                    out.push(Ok(StreamEvent::ThinkingDelta(delta.to_string())));
                }
            }
            "response.reasoning_summary_part.done" => {
                // Part boundary -- add separator so multiple summary parts are readable.
                self.thinking_buf.push_str("\n\n");
                out.push(Ok(StreamEvent::ThinkingDelta("\n\n".to_string())));
            }
            "response.reasoning_summary_part.added" => {
                // No action needed; delta events carry the content.
            }

            // -- Text streaming --
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    out.push(Ok(StreamEvent::TextDelta(delta.to_string())));
                }
            }
            "response.refusal.delta" => {
                // Model refused; surface as text so the user sees it.
                if let Some(delta) = event["delta"].as_str() {
                    out.push(Ok(StreamEvent::TextDelta(delta.to_string())));
                }
            }
            "response.content_part.added" => {
                // Track content parts within a message item (output_text, refusal).
                // No stream event needed; deltas carry the data.
            }

            // -- Function call arguments --
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.tool_json_buf.push_str(delta);
                    if let Some(CodexBlock::FunctionCall { call_id, item_id, .. }) = &self.current {
                        let compound_id = format!("{}|{}", call_id, item_id);
                        out.push(Ok(StreamEvent::ToolCallDelta {
                            id: compound_id,
                            json_fragment: delta.to_string(),
                        }));
                    }
                }
            }
            "response.function_call_arguments.done" => {
                // Final arguments; overwrite buffer with the complete version.
                if let Some(args) = event["arguments"].as_str() {
                    self.tool_json_buf = args.to_string();
                }
            }

            // -- Item done --
            "response.output_item.done" => {
                let item = &event["item"];
                let item_type = item["type"].as_str().unwrap_or("");

                match item_type {
                    "reasoning" => {
                        // Store the full item JSON as the signature for replay.
                        self.reasoning_item = Some(item.clone());
                        let sig = serde_json::to_string(item).ok();
                        // Only emit ThinkingEnd if we haven't already (via finish_current).
                        if matches!(&self.current, Some(CodexBlock::Reasoning)) {
                            out.push(Ok(StreamEvent::ThinkingEnd { sig }));
                            self.current = None;
                        }
                    }
                    "message" => {
                        let sig = item["id"].as_str().map(|s| s.to_string());
                        if matches!(&self.current, Some(CodexBlock::Message)) {
                            out.push(Ok(StreamEvent::TextEnd { sig }));
                            self.current = None;
                        }
                    }
                    "function_call" => {
                        if let Some(CodexBlock::FunctionCall { call_id, item_id }) = &self.current {
                            let compound_id = format!("{}|{}", call_id, item_id);
                            out.push(Ok(StreamEvent::ToolCallEnd { id: compound_id, sig: None }));
                            self.current = None;
                        }
                    }
                    _ => {}
                }
            }

            // -- Response complete --
            "response.done" | "response.completed" => {
                // Guard: the API may send both event types for the same response.
                if self.done { return out; }

                if let Some(response) = event.get("response") {
                    if let Some(usage) = response.get("usage") {
                        let cached = usage.pointer("/input_tokens_details/cached_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let input_total = usage["input_tokens"].as_u64().unwrap_or(0);

                        self.usage = Usage {
                            // OpenAI's input_tokens already includes cached tokens,
                            // matching ri's convention (input_tokens = total prompt context).
                            input_tokens: input_total,
                            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                            cache_read_tokens: cached,
                            cache_write_tokens: 0,
                            extras: Some(usage.clone()),
                        };
                    }
                }

                self.finish_current(&mut out);
                let u = &self.usage;
                if u.input_tokens > 0 || u.output_tokens > 0 {
                    out.push(Ok(StreamEvent::Usage(u.clone())));
                }
                out.push(Ok(StreamEvent::Done));
                self.done = true;
            }

            _ => {
                // Unknown event types are silently ignored.
                // The Responses API may add new event types over time.
            }
        }

        out
    }

    fn finish_current(&mut self, out: &mut Vec<Result<StreamEvent, ApiError>>) {
        match self.current.take() {
            Some(CodexBlock::Reasoning) => {
                let sig = self.reasoning_item.take()
                    .and_then(|item| serde_json::to_string(&item).ok());
                out.push(Ok(StreamEvent::ThinkingEnd { sig }));
            }
            Some(CodexBlock::Message) => {
                out.push(Ok(StreamEvent::TextEnd { sig: None }));
            }
            Some(CodexBlock::FunctionCall { call_id, item_id, .. }) => {
                let compound_id = format!("{}|{}", call_id, item_id);
                out.push(Ok(StreamEvent::ToolCallEnd { id: compound_id, sig: None }));
            }
            None => {}
        }
    }

    fn finalize(&mut self) -> Vec<Result<StreamEvent, ApiError>> {
        if self.done { return Vec::new(); }
        let mut out = Vec::new();
        self.finish_current(&mut out);
        let u = &self.usage;
        if u.input_tokens > 0 || u.output_tokens > 0 {
            out.push(Ok(StreamEvent::Usage(u.clone())));
        }
        out.push(Ok(StreamEvent::Done));
        self.done = true;
        out
    }
}

impl SseInterpreter for CodexState {
    fn interpret(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>> {
        // Codex SSE uses the JSON data payload for event dispatch, not the event: line.
        self.interpret_event(&sse.data)
    }

    fn finish(&mut self) -> Vec<Result<StreamEvent, ApiError>> {
        self.finalize()
    }
}
