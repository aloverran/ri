use clap::Parser;
use color_eyre::eyre::Result;
use ri_core::types::*;
use ri_store::types::Message;

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
                provider: provider.as_ref(),
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
