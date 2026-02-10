use clap::Parser;
use color_eyre::eyre::Result;
use ri_ai::{GeminiVariant, Provider};
use ri_core::types::*;
use ri_store::types::Message;

mod auth;
mod interactive;
mod print_mode;
mod resources;
mod rpc_mode;

#[derive(Parser)]
#[command(name = "ri", about = "A Rust coding agent")]
struct Cli {
    #[arg(long, default_value = "interactive")]
    mode: String,

    #[arg(long, default_value = "anthropic")]
    provider: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(short, long)]
    prompt: Option<String>,

    #[arg(short = 'C', long)]
    cwd: Option<String>,

    #[arg(long, default_value = "text")]
    output: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let cwd = cli.cwd.unwrap_or_else(|| std::env::current_dir().unwrap().display().to_string());

    tracing::info!("ri starting in {}", cwd);

    let res = resources::ResourceLoader::load(std::path::Path::new(&cwd));

    let provider_name = res.settings.default_provider
        .as_deref()
        .unwrap_or(&cli.provider);

    let model_id = cli.model
        .or_else(|| res.settings.default_model.clone())
        .unwrap_or_else(|| default_model_id(provider_name).to_string());

    let model = find_model(provider_name, &model_id, &res);
    let provider = build_provider(provider_name, &res).await;
    let system_prompt = res.build_system_prompt(None);
    let tools = ri_tools::all_tools();
    let cwd_path = std::path::PathBuf::from(&cwd);

    match cli.mode.as_str() {
        "print" | "json" => {
            let prompt = cli.prompt
                .ok_or_else(|| eyre::eyre!("Print mode requires --prompt (-p)"))?;

            let is_json = cli.mode == "json" || cli.output == "json";
            let mut messages = vec![Message::user(prompt)];

            let config = ri_core::agent::RunConfig {
                provider: &provider,
                model: &model,
                system_prompt: &system_prompt,
                tools: &tools,
                thinking: ThinkingLevel::Medium,
                max_tokens: None,
                cwd: &cwd_path,
            };

            let cancel = tokio_util::sync::CancellationToken::new();
            if is_json {
                let mut cb = print_mode::JsonCallback::new();
                ri_core::agent::run(&config, &mut messages, &mut cb, cancel).await?;
            } else {
                let mut cb = print_mode::TextCallback::new();
                ri_core::agent::run(&config, &mut messages, &mut cb, cancel).await?;
            }
            println!();
        }
        "rpc" => {
            rpc_mode::run(provider, model, system_prompt, tools, cwd_path, cli.prompt).await;
        }
        "interactive" | _ => {
            eprintln!("ri - a Rust coding agent");
            eprintln!("Type /help for commands, /quit to exit.\n");
            interactive::run(provider, model, system_prompt, tools, cwd_path, cli.prompt).await?;
        }
    }

    Ok(())
}

fn default_model_id(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-sonnet-4-20250514",
        "google-gemini-cli" => "gemini-2.5-pro",
        "google-antigravity" => "gemini-3-pro",
        _ => "claude-sonnet-4-20250514",
    }
}

fn find_model(provider: &str, model_id: &str, res: &resources::ResourceLoader) -> Model {
    for m in res.custom_models() {
        if m.id == model_id { return m; }
    }

    match (provider, model_id) {
        ("anthropic", "claude-sonnet-4-20250514") => Model {
            id: "claude-sonnet-4-20250514".into(), name: "Claude Sonnet 4".into(),
            reasoning: false, context_window: 200_000, max_tokens: 16_384,
            cost: ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
        },
        ("anthropic", id) if id.contains("opus-4-6") || id.contains("opus-4.6") => Model {
            id: id.into(), name: "Claude Opus 4.6".into(),
            reasoning: true, context_window: 200_000, max_tokens: 32_768,
            cost: ModelCost { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 },
        },
        ("google-gemini-cli", "gemini-2.5-pro") => Model {
            id: "gemini-2.5-pro".into(), name: "Gemini 2.5 Pro".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 1.25, output: 10.0, cache_read: 0.315, cache_write: 0.0 },
        },
        ("google-gemini-cli", "gemini-2.5-flash") => Model {
            id: "gemini-2.5-flash".into(), name: "Gemini 2.5 Flash".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 0.15, output: 0.6, cache_read: 0.0375, cache_write: 0.0 },
        },
        ("google-antigravity", "gemini-3-pro") => Model {
            id: "gemini-3-pro".into(), name: "Gemini 3 Pro".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 2.0, output: 6.0, cache_read: 0.5, cache_write: 0.0 },
        },
        ("google-antigravity", "gemini-3-flash") => Model {
            id: "gemini-3-flash".into(), name: "Gemini 3 Flash".into(),
            reasoning: true, context_window: 1_048_576, max_tokens: 65_536,
            cost: ModelCost { input: 0.5, output: 1.5, cache_read: 0.125, cache_write: 0.0 },
        },
        _ => {
            Model {
                id: model_id.into(), name: model_id.into(),
                reasoning: false, context_window: 128_000, max_tokens: 16_384,
                cost: ModelCost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            }
        }
    }
}

async fn build_provider(provider_name: &str, res: &resources::ResourceLoader) -> Provider {
    let mut auth_store = auth::AuthStore::load();

    match provider_name {
        "anthropic" => {
            let key = res.provider_api_key("anthropic")
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .unwrap_or_default();

            if !key.is_empty() {
                return Provider::Anthropic { api_key: key };
            }

            if let Some(creds) = auth_store.get("anthropic").cloned() {
                if !creds.is_expired() {
                    return Provider::Anthropic { api_key: creds.access };
                }
                if let Ok(refreshed) = ri_ai::auth::anthropic::refresh_token(&creds).await {
                    let key = refreshed.access.clone();
                    auth_store.set("anthropic", refreshed);
                    let _ = auth_store.save();
                    return Provider::Anthropic { api_key: key };
                }
            }

            Provider::Anthropic { api_key: String::new() }
        }

        name @ ("google-gemini-cli" | "google-antigravity") => {
            let variant = if name == "google-antigravity" {
                GeminiVariant::Antigravity
            } else {
                GeminiVariant::Cli
            };

            if let Some(creds) = auth_store.get(name).cloned() {
                let (token, project_id) = if creds.is_expired() {
                    match ri_ai::auth::google::refresh_token(&creds, variant).await {
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

        _ => Provider::Anthropic { api_key: String::new() },
    }
}
