// Anthropic provider -- self-contained implementation.
//
// Handles: model catalog, credential management, request building,
// SSE interpretation, and login flow.

use std::collections::HashMap;
use std::path::PathBuf;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::error;

use ri::{
    ApiError, AuthMethod, ContentBlock, EventStream, LlmProvider, Message, Model, ModelCost,
    RequestOptions, Role, StreamEvent, ThinkingLevel, ToolSchema, Usage,
};
use crate::sse::{self, SseEvent, SseInterpreter};
use crate::creds::{self, Credentials};

fn creds_path() -> eyre::Result<PathBuf> {
    Ok(creds::ri_dir()?.join("anthropic_auth.json"))
}

fn load_creds() -> Option<Credentials> {
    creds::load(&creds_path().ok()?)
}

fn save_creds(creds: &Credentials) -> eyre::Result<()> {
    creds::save(&creds_path()?, creds)
}

// -- PKCE + OAuth constants --

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

// -- Provider struct --

pub struct AnthropicProvider {
    state: Mutex<ProviderState>,
}

struct ProviderState {
    api_key: String,
    is_oauth: bool,
    // PKCE verifier + state for in-progress login
    login_verifier: Option<String>,
    login_state: Option<String>,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let mut api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let mut is_oauth = false;

        if api_key.is_empty() {
            if let Some(creds) = load_creds() {
                api_key = creds.access_token.clone();
                is_oauth = api_key.starts_with("sk-ant-oat");
            }
        } else {
            is_oauth = api_key.starts_with("sk-ant-oat");
        }

        Self {
            state: Mutex::new(ProviderState {
                api_key,
                is_oauth,
                login_verifier: None,
                login_state: None,
            }),
        }
    }

    async fn ensure_valid_token(&self) -> eyre::Result<(String, bool)> {
        let mut state = self.state.lock().await;
        if state.api_key.is_empty() {
            return Ok((String::new(), false));
        }

        // If we have stored creds and they're expired, try refresh
        if state.is_oauth {
            if let Some(creds) = load_creds() {
                if creds.is_expired() {
                    match refresh_token(&creds).await {
                        Ok(refreshed) => {
                            state.api_key = refreshed.access_token.clone();
                            state.is_oauth = true;
                            let _ = save_creds(&refreshed);
                        }
                        Err(e) => {
                            tracing::warn!("Anthropic token refresh failed: {}", e);
                        }
                    }
                }
            }
        }

        Ok((state.api_key.clone(), state.is_oauth))
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
                reasoning: true, context_window: 200_000, max_tokens: 128_000,
                cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
            },
            Model {
                id: "claude-opus-4-6".into(), name: "Claude Opus 4.6".into(),
                reasoning: true, context_window: 200_000, max_tokens: 128_000,
                cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 },
            },
            // Extended context variants -- 1M token input via beta header.
            // Same models and pricing, but opts into the context-1m-2025-08-07 beta.
            Model {
                id: "claude-sonnet-4-6-1m".into(), name: "Claude Sonnet 4.6 (1M)".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
            },
            Model {
                id: "claude-opus-4-6-1m".into(), name: "Claude Opus 4.6 (1M)".into(),
                reasoning: true, context_window: 1_000_000, max_tokens: 128_000,
                cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 },
            },
        ]
    }

    fn is_authenticated(&self) -> bool {
        // Best-effort sync check.
        self.state.try_lock().map(|s| !s.api_key.is_empty()).unwrap_or(false)
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

        let mut state = self.state.lock().await;
        state.login_verifier = Some(verifier);
        state.login_state = Some(login_state);
        drop(state);

        Ok(Some(AuthMethod::PasteCode { url: url.to_string() }))
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
            Some((c, s)) => (c, Some(s)),
            None => (code, None),
        };

        if let Some(rs) = returned_state {
            if rs != login_state {
                return Err(eyre::eyre!("OAuth state mismatch"));
            }
        }

        let client = reqwest::Client::new();
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
        let key = creds.access_token.clone();
        save_creds(&creds)?;

        let mut state = self.state.lock().await;
        state.api_key = key;
        state.is_oauth = true;

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
        let mut state = self.state.lock().await;
        state.api_key.clear();
        state.is_oauth = false;
        Ok(())
    }

    async fn stream(&self, mut opts: RequestOptions) -> Result<EventStream, ApiError> {
        let (api_key, is_oauth) = self.ensure_valid_token().await
            .map_err(|e| ApiError::other(format!("{e:#}")))?;

        // Strip the -1m suffix (ri-internal) before sending to the API.
        let extended_context = opts.model.id.ends_with("-1m");
        if extended_context {
            opts.model.id = opts.model.id.strip_suffix("-1m").unwrap().to_string();
        }

        let request = build_request(&api_key, &opts, extended_context);
        let bytes = sse::send(request).await?;
        let state = AnthropicState::new(is_oauth, opts.tools.to_vec());
        Ok(Box::pin(sse::drive_sse_stream(bytes, state)))
    }
}

// -- Token handling --

async fn refresh_token(credentials: &Credentials) -> eyre::Result<Credentials> {
    let client = reqwest::Client::new();
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

fn build_request(api_key: &str, opts: &RequestOptions, extended_context: bool) -> reqwest::RequestBuilder {
    let is_oauth = api_key.starts_with("sk-ant-oat");
    let body = build_body(opts, is_oauth);
    let url = "https://api.anthropic.com/v1/messages";

    tracing::trace!(url, %body, "Anthropic API request");

    let mut builder = reqwest::Client::new()
        .post(url)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");

    let context_1m_beta = if extended_context { ",context-1m-2025-08-07" } else { "" };

    if is_oauth {
        builder = builder
            .header("authorization", format!("Bearer {}", api_key))
            .header("anthropic-beta", format!(
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14{context_1m_beta}"
            ))
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("user-agent", "claude-cli/2.1.2 (external, cli)")
            .header("x-app", "cli");
    } else {
        builder = builder
            .header("x-api-key", api_key)
            .header("anthropic-beta", format!(
                "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14{context_1m_beta}"
            ));
    }

    builder.body(body.to_string())
}

fn build_body(opts: &RequestOptions, is_oauth: bool) -> Value {
    let messages: Vec<Value> = opts.messages.iter()
        .filter(|m| m.role != Role::System)
        .map(|m| convert_message(m))
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

    if is_oauth {
        body["system"] = json!([
            { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
            { "type": "text", "text": opts.system_prompt },
        ]);
    } else {
        body["system"] = json!([{ "type": "text", "text": opts.system_prompt }]);
    }

    if !opts.tools.is_empty() {
        let tools: Vec<Value> = opts.tools.iter().map(|t| {
            let name = if is_oauth { to_claude_code_name(&t.name) } else { t.name.clone() };
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
        body["thinking"] = json!({ "type": "adaptive" });
        let effort = match opts.thinking {
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh => "max",
            ThinkingLevel::Off => unreachable!(),
        };
        body["output_config"] = json!({ "effort": effort });
    }

    body
}

fn convert_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "user",
    };

    let content: Vec<Value> = msg.content.iter()
        .filter(|c| !matches!(c, ContentBlock::Error { .. }))
        .map(convert_content)
        .collect();
    let has_tool_results = msg.content.iter().any(|c| matches!(c, ContentBlock::ToolResult { .. }));
    let effective_role = if has_tool_results { "user" } else { role };

    json!({ "role": effective_role, "content": content })
}

fn convert_content(c: &ContentBlock) -> Value {
    match c {
        ContentBlock::Text { text, .. } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { thinking, sig } => {
            if let Some(s) = sig {
                json!({ "type": "thinking", "thinking": thinking, "signature": s })
            } else {
                json!({ "type": "text", "text": thinking })
            }
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data }
        }),
        ContentBlock::ToolUse { id, name, input, .. } => json!({
            "type": "tool_use", "id": id, "name": name, "input": input
        }),
        ContentBlock::ToolResult { tool_use_id, content, is_error, .. } => {
            let content_json: Vec<Value> = content.iter().map(convert_content).collect();
            json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content_json,
                "is_error": is_error,
            })
        }
        ContentBlock::Error { message } => json!({
            "type": "text",
            "text": format!("[Error: {}]", message)
        }),
        ContentBlock::Unknown(v) => v.clone(),
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
                        AnthropicBlock::Thinking => out.push(Ok(StreamEvent::ThinkingEnd { sig })),
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
                let parsed: Value = serde_json::from_str(&sse.data).unwrap_or_default();
                let error_type = parsed["error"]["type"].as_str().unwrap_or("unknown");
                let error_msg = parsed["error"]["message"].as_str().unwrap_or("Unknown error").to_string();
                match error_type {
                    "rate_limit_error" => {
                        out.push(Err(ApiError::rate_limited(5000, error_msg)));
                    }
                    _ => {
                        out.push(Err(ApiError::other(format!("{error_type}: {error_msg}"))));
                    }
                }
            }

            other => { error!("Unknown SSE event type: [{}]", other); }
        }

        out
    }
}


