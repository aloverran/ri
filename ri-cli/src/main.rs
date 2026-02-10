use clap::Parser;
use color_eyre::eyre::Result;
use ri_core::agent;
use ri_core::types::*;
use ri_store::types::{ContentBlock, Message, Role};
use ri_store::filing::SessionFiling;

mod interactive;
mod print_mode;
mod resources;
mod rpc_mode;

#[derive(Parser)]
#[command(name = "ri", about = "A Rust coding agent")]
struct Cli {
    #[arg(long, default_value = "interactive")]
    mode: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(short, long)]
    prompt: Option<String>,

    #[arg(short = 'C', long)]
    cwd: Option<String>,

    #[arg(long, default_value = "text")]
    output: String,

    #[arg(long)]
    thinking: Option<String>,
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

    let model_id = cli.model
        .or_else(|| res.settings.default_model.clone())
        .unwrap_or_else(|| ri_ai::registry::default_model_id().to_string());

    let (provider, model) = ri_ai::registry::resolve(&model_id).await?;
    let system_prompt = res.build_system_prompt();
    let tools = ri_tools::all_tools();
    let cwd_path = std::path::PathBuf::from(&cwd);

    // Resolve thinking level: CLI flag > settings > default (medium).
    let thinking = resolve_thinking(
        cli.thinking.as_deref(),
        res.settings.default_thinking.as_deref(),
    );

    match cli.mode.as_str() {
        "print" | "json" => {
            let prompt = cli.prompt
                .ok_or_else(|| eyre::eyre!("Print mode requires --prompt (-p)"))?;

            let is_json = cli.mode == "json" || cli.output == "json";

            let sessions_dir = SessionFiling::default_dir()?;
            let mut filing = SessionFiling::new(sessions_dir);
            filing.load_all()?;
            filing.new_session("print", &cwd)?;

            let sys_id = filing.next_id();
            let sys_msg = Message::new(sys_id.clone(), Role::System, vec![ContentBlock::text(&system_prompt)]);
            filing.write_message(sys_msg)?;

            let user_id = filing.next_id();
            let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(&prompt)]);
            filing.write_message(user_msg)?;

            let mut session_ids = vec![sys_id, user_id];

            let config = agent::RunConfig {
                provider: provider.as_ref(),
                model: &model,
                system_prompt: &system_prompt,
                tools: &tools,
                thinking,
                max_tokens: None,
                cwd: &cwd_path,
                strategy: agent::naive_strategy,
            };

            let cancel = tokio_util::sync::CancellationToken::new();
            if is_json {
                let mut cb = print_mode::JsonCallback;
                agent::run(&config, &mut filing, &mut session_ids, &mut cb, cancel).await?;
            } else {
                let mut cb = print_mode::TextCallback;
                agent::run(&config, &mut filing, &mut session_ids, &mut cb, cancel).await?;
            }
            println!();
        }
        "rpc" => {
            rpc_mode::run(provider, model, system_prompt, tools, cwd_path, cli.prompt, thinking).await;
        }
        "interactive" | _ => {
            eprintln!("ri - a Rust coding agent");
            eprintln!("Type /help for commands, /quit to exit.\n");
            interactive::run(provider, model, system_prompt, tools, cwd_path, cli.prompt, thinking).await?;
        }
    }

    Ok(())
}

fn resolve_thinking(cli_flag: Option<&str>, settings: Option<&str>) -> ThinkingLevel {
    let raw = cli_flag.or(settings).unwrap_or("medium");
    match raw {
        "off" => ThinkingLevel::Off,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        other => {
            eprintln!("Unknown thinking level '{}', using medium", other);
            ThinkingLevel::Medium
        }
    }
}
