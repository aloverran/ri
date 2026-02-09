use ri::agent::{self, AgentEvent, RunConfig};
use ri::api::{GeminiVariant, Provider, StreamEvent};
use ri::tools::ToolDef;
use ri::types::*;
use std::io::Write;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

use crate::auth::AuthStore;

pub async fn run(
    mut provider: Provider,
    model: Model,
    system_prompt: String,
    tools: Vec<ToolDef>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
) -> eyre::Result<()> {
    let mut messages: Vec<Message> = Vec::new();

    // If there's an initial prompt, run it first
    if let Some(prompt) = initial_prompt {
        print_user_prefix();
        println!("{}", prompt);
        messages.push(Message::user(&prompt));

        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: &provider,
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking: ThinkingLevel::Medium,
            max_tokens: None,
            cwd: &cwd,
        };
        agent::run(&config, &mut messages, &mut |evt| display_event(&evt), cancel).await?;
    }

    // Main REPL loop
    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    // Login state
    let mut login_pending: Option<ri::auth::anthropic::LoginFlow> = None;

    loop {
        if login_pending.is_some() {
            eprint!("\x1b[33mPaste code: \x1b[0m");
        } else {
            eprint!("\x1b[36m> \x1b[0m");
        }
        let _ = std::io::stderr().flush();

        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        // Login code completion
        if let Some(flow) = login_pending.take() {
            match ri::auth::anthropic::complete_login(trimmed, &flow).await {
                Ok(creds) => {
                    let key = creds.access.clone();
                    let mut store = AuthStore::load();
                    store.set("anthropic", creds);
                    let _ = store.save();
                    eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
                    provider = Provider::Anthropic { api_key: key };
                }
                Err(e) => eprintln!("\x1b[31m[login failed: {}]\x1b[0m", e),
            }
            continue;
        }

        // Commands
        match trimmed {
            "/quit" | "/exit" => break,
            "/help" => {
                eprintln!("\x1b[33mCommands:\x1b[0m");
                eprintln!("  /login              - Log in via OAuth (Anthropic)");
                eprintln!("  /login google       - Log in via OAuth (Google Antigravity)");
                eprintln!("  /login gemini       - Log in via OAuth (Google Gemini CLI)");
                eprintln!("  /quit, /exit        - Exit ri");
                continue;
            }
            "/login" | "/login anthropic" => {
                match ri::auth::anthropic::begin_login() {
                    Ok(flow) => {
                        eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
                        eprintln!("\x1b[4m{}\x1b[0m\n", flow.url);
                        login_pending = Some(flow);
                    }
                    Err(e) => eprintln!("\x1b[31m[login error: {}]\x1b[0m", e),
                }
                continue;
            }
            cmd if cmd == "/login google" || cmd == "/login google-antigravity" => {
                if let Some(p) = do_google_login(GeminiVariant::Antigravity).await {
                    provider = p;
                }
                continue;
            }
            cmd if cmd == "/login gemini" || cmd == "/login google-gemini-cli" => {
                if let Some(p) = do_google_login(GeminiVariant::Cli).await {
                    provider = p;
                }
                continue;
            }
            _ => {}
        }

        // Send to agent
        messages.push(Message::user(trimmed));
        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: &provider,
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking: ThinkingLevel::Medium,
            max_tokens: None,
            cwd: &cwd,
        };

        if let Err(e) = agent::run(&config, &mut messages, &mut |evt| display_event(&evt), cancel).await {
            eprintln!("\x1b[31m[error: {}]\x1b[0m", e);
        }
    }

    println!();
    Ok(())
}

fn display_event(evt: &AgentEvent) {
    match evt {
        AgentEvent::StreamEvent(se) => match se {
            StreamEvent::TextStart => {}
            StreamEvent::TextDelta(d) => {
                print!("{}", d);
                let _ = std::io::stdout().flush();
            }
            StreamEvent::TextEnd { .. } => { println!(); }
            StreamEvent::ThinkingStart => { eprint!("\x1b[2m"); }
            StreamEvent::ThinkingDelta(d) => {
                eprint!("{}", d);
                let _ = std::io::stderr().flush();
            }
            StreamEvent::ThinkingEnd { .. } => { eprintln!("\x1b[0m"); }
            StreamEvent::ToolCallStart { name, .. } => {
                eprintln!("\x1b[33m[tool: {}]\x1b[0m", name);
            }
            StreamEvent::Error(msg) => {
                eprintln!("\x1b[31m[error: {}]\x1b[0m", msg);
            }
            _ => {}
        },
        AgentEvent::ToolStart { name, .. } => {
            eprintln!("\x1b[33m[executing: {}]\x1b[0m", name);
        }
        AgentEvent::ToolEnd { output, is_error, .. } => {
            if *is_error {
                eprintln!("\x1b[31m[tool error: {}]\x1b[0m", output);
            } else {
                let display = if output.len() > 200 {
                    let end = output.char_indices()
                        .map(|(i, _)| i)
                        .take_while(|&i| i <= 200)
                        .last()
                        .unwrap_or(0);
                    format!("{}... ({} bytes)", &output[..end], output.len())
                } else {
                    output.clone()
                };
                eprintln!("\x1b[2m[result: {}]\x1b[0m", display);
            }
        }
        _ => {}
    }
}

fn print_user_prefix() {
    eprint!("\x1b[36m> \x1b[0m");
    let _ = std::io::stderr().flush();
}

async fn do_google_login(variant: GeminiVariant) -> Option<Provider> {
    let variant_name = match variant {
        GeminiVariant::Antigravity => "google-antigravity",
        GeminiVariant::Cli => "google-gemini-cli",
    };

    eprintln!("\x1b[33mStarting Google OAuth login...\x1b[0m");

    match ri::auth::google::login(variant, |url| {
        eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
        eprintln!("\x1b[4m{}\x1b[0m\n", url);
        #[cfg(target_os = "macos")]
        { let _ = std::process::Command::new("open").arg(url).spawn(); }
    }).await {
        Ok(creds) => {
            let (token, project_id) = ri::auth::google::build_api_key(&creds);
            if let Some(ref email) = creds.email {
                eprintln!("\x1b[32mLogged in as {}\x1b[0m", email);
            } else {
                eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
            }
            let mut store = AuthStore::load();
            store.set(variant_name, creds);
            let _ = store.save();
            Some(Provider::Gemini { variant, token, project_id })
        }
        Err(e) => {
            eprintln!("\x1b[31m[Google login failed: {}]\x1b[0m", e);
            None
        }
    }
}
