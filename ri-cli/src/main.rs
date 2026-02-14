use clap::Parser;
use color_eyre::eyre::Result;
use ri::{ContentBlock, Message, Role, SessionStore, ThinkingLevel};

mod agent;
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
    let cwd = cli.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .expect("could not determine current directory")
            .display()
            .to_string()
    });

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
            use futures::StreamExt;

            let prompt = cli.prompt
                .ok_or_else(|| eyre::eyre!("Print mode requires --prompt (-p)"))?;

            let is_json = cli.mode == "json" || cli.output == "json";

            let (mut filing, mut session_ids) = SessionStore::init("print", &cwd_path, &system_prompt)?;

            let user_id = filing.next_id();
            let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(&prompt)]);
            filing.write_message(user_msg)?;
            session_ids.push(user_id);

            let cancel = tokio_util::sync::CancellationToken::new();
            let handler: fn(&agent::AgentEvent) = if is_json {
                print_mode::on_event_json
            } else {
                print_mode::on_event_text
            };

            let events = agent::run(
                provider.as_ref(), &model, &system_prompt, &tools,
                &mut filing, &mut session_ids, &cwd_path, thinking, None, cancel,
            );
            tokio::pin!(events);
            while let Some(evt) = events.next().await {
                handler(&evt);
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
