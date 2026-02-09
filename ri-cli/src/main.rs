// ri - Rust implementation of pi coding agent.
//
// Entry point: parse args, set up model registry, create agent, run mode.

use clap::Parser;
use color_eyre::eyre::Result;
use ri_ai::oauth::OAuthProvider;

mod auth;
mod interactive;
mod print_mode;
mod resources;
mod rpc_mode;
mod session;

#[derive(Parser)]
#[command(name = "ri", about = "A Rust coding agent")]
struct Cli {
    /// Run mode
    #[arg(long, default_value = "interactive")]
    mode: String,

    /// LLM provider
    #[arg(long, default_value = "anthropic")]
    provider: String,

    /// Model ID
    #[arg(long)]
    model: Option<String>,

    /// Initial prompt (non-interactive)
    #[arg(short, long)]
    prompt: Option<String>,

    /// Working directory
    #[arg(short = 'C', long)]
    cwd: Option<String>,

    /// Disable session persistence
    #[arg(long)]
    no_session: bool,

    /// Output format for print mode: text or json
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

    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap().display().to_string());

    tracing::info!("ri starting in {}", cwd);

    // Load resources (context files, skills, prompts, settings, models.json)
    let res = resources::ResourceLoader::load(std::path::Path::new(&cwd));

    // Set up model registry
    let mut registry = ri_ai::registry::ModelRegistry::new();

    // Register Anthropic provider
    let anthropic = std::sync::Arc::new(ri_ai::anthropic::AnthropicProvider::new());
    registry.register_provider("anthropic", anthropic);

    // Add default models
    registry.add_model(ri_core::types::Model {
        id: "claude-sonnet-4-20250514".to_string(),
        name: "Claude Sonnet 4".to_string(),
        api: ri_core::types::ApiType::AnthropicMessages,
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning: false,
        input: vec![
            ri_core::types::InputModality::Text,
            ri_core::types::InputModality::Image,
        ],
        cost: ri_core::types::ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        context_window: 200_000,
        max_tokens: 16_384,
    });

    // Register custom models from models.json
    for model in res.custom_models() {
        registry.add_model(model);
    }

    // Create tools
    let tools = ri_tools::all_tools(&cwd);

    // Create event channel
    let (event_tx, event_rx) = ri_core::event::event_channel(256);

    // Resolve model -- use settings defaults, then CLI overrides
    let provider_name = res.settings.default_provider
        .as_deref()
        .unwrap_or(&cli.provider);
    let model_id = cli.model
        .or_else(|| res.settings.default_model.clone())
        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
    let model = registry
        .find(provider_name, &model_id)
        .cloned()
        .ok_or_else(|| eyre::eyre!("Model not found: {}:{}", provider_name, model_id))?;

    // Resolve API key: models.json -> env var -> OAuth credentials
    let mut api_key = match res.provider_api_key(provider_name) {
        Some(key_spec) => registry.resolve_api_key(&key_spec).await
            .unwrap_or_default(),
        None => std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
    };

    // Fall back to OAuth credentials from ~/.ri/auth.json
    if api_key.is_empty() {
        let mut auth_store = auth::AuthStore::load();
        if let Some(creds) = auth_store.get(provider_name).cloned() {
            // Detect stale OAuth tokens (sk-ant-oat...) from before the create_api_key
            // fix. These won't work with the Messages API -- user needs to /login again.
            if creds.access.starts_with("sk-ant-oat") {
                tracing::warn!("Saved credentials contain an OAuth token, not an API key. Run /login to re-authenticate.");
            } else if auth::AuthStore::is_expired(&creds) {
                let oauth = ri_ai::oauth::anthropic_oauth::AnthropicOAuth::new();
                match oauth.refresh_token(&creds).await {
                    Ok(new_creds) => {
                        api_key = new_creds.access.clone();
                        auth_store.set(provider_name, new_creds);
                        let _ = auth_store.save();
                        tracing::info!("API key refreshed via OAuth");
                    }
                    Err(e) => {
                        tracing::warn!("OAuth token refresh failed: {e}");
                    }
                }
            } else {
                api_key = creds.access.clone();
                tracing::info!("Using saved API key");
            }
        }
    }

    let provider = registry
        .get_provider(provider_name)
        .ok_or_else(|| eyre::eyre!("Provider not found: {}", provider_name))?;

    // Build system prompt from resources (tool descriptions come via API tools param)
    let system_prompt = res.build_system_prompt(None);

    // Create agent
    let config = ri_core::agent::AgentConfig {
        model,
        thinking_level: ri_core::types::ThinkingLevel::Medium,
        system_prompt,
        api_key,
    };

    let mut agent = ri_core::agent::Agent::new(config, provider, tools, event_tx);

    match cli.mode.as_str() {
        "print" | "json" => {
            let prompt = cli.prompt
                .ok_or_else(|| eyre::eyre!("Print mode requires --prompt (-p)"))?;

            let is_json = cli.mode == "json" || cli.output == "json";
            let display_task = if is_json {
                tokio::spawn(print_mode::run_json(event_rx))
            } else {
                tokio::spawn(print_mode::run_text(event_rx))
            };

            agent
                .prompt(ri_core::types::AgentMessage::user(prompt))
                .await?;

            let _ = display_task.await;
        }
        "rpc" => {
            let steering_tx = agent.steering_handle();
            let follow_up_tx = agent.follow_up_handle();
            let cancel = agent.cancel_token();

            if let Some(prompt) = cli.prompt {
                // Run agent with initial prompt in background
                tokio::spawn(async move {
                    let _ = agent
                        .prompt(ri_core::types::AgentMessage::user(prompt))
                        .await;
                });

                rpc_mode::run(event_rx, steering_tx, follow_up_tx, cancel).await;
            } else {
                // No initial prompt -- wait for prompt command via RPC
                rpc_mode::run(event_rx, steering_tx, follow_up_tx, cancel).await;
            }
        }
        "interactive" | _ => {
            eprintln!("ri - a Rust coding agent");
            eprintln!("Type /help for commands, /quit to exit.\n");
            interactive::run(agent, event_rx, cli.prompt).await?;
        }
    }

    Ok(())
}

