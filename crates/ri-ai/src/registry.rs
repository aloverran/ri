// Provider and model registry.
//
// This module is the public interface for resolving providers and models.
// ri-cli calls into here without knowing about specific providers.

use ri_core::types::{Model, ModelCost};
use ri_core::provider::LlmProvider;
use crate::auth::store::AuthStore;
use crate::{Provider, GeminiVariant};

// -- Model catalog --

fn all_models() -> Vec<(&'static str, Model)> {
    vec![
        ("anthropic", Model {
            id: "claude-sonnet-4-20250514".into(), name: "Claude Sonnet 4".into(),
            reasoning: false, context_window: 200_000, max_tokens: 16_384,
            cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
        }),
        ("anthropic", Model {
            id: "claude-opus-4-6-20250610".into(), name: "Claude Opus 4.6".into(),
            reasoning: true, context_window: 200_000, max_tokens: 32_768,
            cost: ModelCost { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 },
        }),
        ("google-gemini-cli", Model {
            id: "gemini-2.5-pro".into(), name: "Gemini 2.5 Pro".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 1.25, output: 10.0, cache_read: 0.315, cache_write: 0.0 },
        }),
        ("google-gemini-cli", Model {
            id: "gemini-2.5-flash".into(), name: "Gemini 2.5 Flash".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 0.15, output: 0.6, cache_read: 0.0375, cache_write: 0.0 },
        }),
        ("google-antigravity", Model {
            id: "gemini-3-pro".into(), name: "Gemini 3 Pro".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 2.0, output: 6.0, cache_read: 0.5, cache_write: 0.0 },
        }),
        ("google-antigravity", Model {
            id: "gemini-3-flash".into(), name: "Gemini 3 Flash".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 0.5, output: 1.5, cache_read: 0.125, cache_write: 0.0 },
        }),
    ]
}

const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("anthropic", "claude-sonnet-4-20250514"),
    ("google-gemini-cli", "gemini-2.5-pro"),
    ("google-antigravity", "gemini-3-pro"),
];

// Find a model by id. Searches the built-in catalog.
pub fn find_model(model_id: &str) -> Model {
    for (_provider, model) in all_models() {
        if model.id == model_id { return model; }
    }
    // Fallback for unknown model ids.
    Model {
        id: model_id.into(), name: model_id.into(),
        reasoning: false, context_window: 128_000, max_tokens: 16_384,
        cost: ModelCost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
    }
}

// Determine which provider owns a model id.
fn provider_for_model(model_id: &str) -> Option<&'static str> {
    for (provider, model) in all_models() {
        if model.id == model_id { return Some(provider); }
    }
    None
}

// The default model id (first provider's default).
pub fn default_model_id() -> &'static str {
    DEFAULT_MODELS[0].1
}

// -- Provider resolution --

// Resolve a provider for the given model. Handles auth store lookup,
// env var fallback, and token refresh.
pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    let model = find_model(model_id);
    let provider_name = provider_for_model(model_id);
    let provider = build_provider(provider_name).await;
    Ok((Box::new(provider), model))
}

async fn build_provider(provider_name: Option<&str>) -> Provider {
    let mut auth_store = AuthStore::load();

    match provider_name {
        Some("anthropic") | None => {
            let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
            if !key.is_empty() {
                return Provider::Anthropic { api_key: key };
            }

            if let Some(creds) = auth_store.get("anthropic").cloned() {
                if !creds.is_expired() {
                    return Provider::Anthropic { api_key: creds.access };
                }
                if let Ok(refreshed) = crate::auth::anthropic::refresh_token(&creds).await {
                    let key = refreshed.access.clone();
                    auth_store.set("anthropic", refreshed);
                    let _ = auth_store.save();
                    return Provider::Anthropic { api_key: key };
                }
            }

            Provider::Anthropic { api_key: String::new() }
        }

        Some(name @ ("google-gemini-cli" | "google-antigravity")) => {
            let variant = if name == "google-antigravity" {
                GeminiVariant::Antigravity
            } else {
                GeminiVariant::Cli
            };

            if let Some(creds) = auth_store.get(name).cloned() {
                let (token, project_id) = if creds.is_expired() {
                    match crate::auth::google::refresh_token(&creds, variant).await {
                        Ok(refreshed) => {
                            let t = refreshed.access.clone();
                            let p = refreshed.project_id.clone().unwrap_or_default();
                            auth_store.set(name, refreshed);
                            let _ = auth_store.save();
                            (t, p)
                        }
                        Err(e) => {
                            tracing::warn!("Google token refresh failed: {}", e);
                            (String::new(), String::new())
                        }
                    }
                } else {
                    (creds.access.clone(), creds.project_id.clone().unwrap_or_default())
                };

                return Provider::Gemini { variant, token, project_id };
            }

            Provider::Gemini { variant, token: String::new(), project_id: String::new() }
        }

        Some(_) => Provider::Anthropic { api_key: String::new() },
    }
}

// -- Login flows --

pub struct LoginFlowInfo {
    pub name: &'static str,
    pub display: &'static str,
}

pub fn available_logins() -> Vec<LoginFlowInfo> {
    vec![
        LoginFlowInfo { name: "anthropic", display: "Anthropic (OAuth)" },
        LoginFlowInfo { name: "gemini", display: "Google Gemini CLI (OAuth)" },
        LoginFlowInfo { name: "google", display: "Google Antigravity (OAuth)" },
    ]
}

pub enum LoginFlow {
    // User must visit url, get a code, and pass it to complete_paste_login.
    PasteCode { url: String, state: PasteCodeState },
    // Provider opens a local HTTP server. Pass name to run_callback_login.
    // The auth URL is delivered via the on_url callback during run_callback_login.
    LocalCallback { name: String },
}

pub struct PasteCodeState {
    pub(crate) verifier: String,
    pub(crate) state: String,
}

// Start a login flow by name. Returns a LoginFlow the caller drives.
pub fn start_login(name: &str) -> eyre::Result<LoginFlow> {
    match name {
        "anthropic" => {
            let flow = crate::auth::anthropic::begin_login()?;
            Ok(LoginFlow::PasteCode {
                url: flow.url,
                state: PasteCodeState { verifier: flow.verifier, state: flow.state },
            })
        }
        "gemini" | "google-gemini-cli" => {
            Ok(LoginFlow::LocalCallback { name: name.to_string() })
        }
        "google" | "google-antigravity" => {
            Ok(LoginFlow::LocalCallback { name: name.to_string() })
        }
        _ => Err(eyre::eyre!("Unknown login flow: {}", name)),
    }
}

// Complete a paste-code login (Anthropic-style).
pub async fn complete_paste_login(
    state: PasteCodeState,
    code: &str,
) -> eyre::Result<(Box<dyn LlmProvider>, String)> {
    let flow = crate::auth::anthropic::LoginFlow {
        url: String::new(),
        verifier: state.verifier,
        state: state.state,
    };
    let creds = crate::auth::anthropic::complete_login(code, &flow).await?;
    let key = creds.access.clone();
    let mut store = AuthStore::load();
    store.set("anthropic", creds);
    let _ = store.save();
    let provider = Provider::Anthropic { api_key: key };
    Ok((Box::new(provider), "anthropic".into()))
}

// Run a local-callback login (Google-style). Calls on_url when the auth URL is ready.
pub async fn run_callback_login(
    name: &str,
    on_url: impl FnOnce(&str),
) -> eyre::Result<(Box<dyn LlmProvider>, String)> {
    let (variant, store_name) = match name {
        "gemini" | "google-gemini-cli" => (GeminiVariant::Cli, "google-gemini-cli"),
        "google" | "google-antigravity" => (GeminiVariant::Antigravity, "google-antigravity"),
        _ => return Err(eyre::eyre!("Unknown callback login: {}", name)),
    };

    let creds = crate::auth::google::login(variant, on_url).await?;
    let token = creds.access.clone();
    let project_id = creds.project_id.clone().unwrap_or_default();
    let email = creds.email.clone();

    let mut store = AuthStore::load();
    store.set(store_name, creds);
    let _ = store.save();

    if let Some(ref email) = email {
        tracing::info!("Logged in as {}", email);
    }

    let provider = Provider::Gemini { variant, token, project_id };
    Ok((Box::new(provider), store_name.into()))
}
