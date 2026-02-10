use ri_agent::{self as agent, AgentCallback, AgentEvent, RunConfig};
use ri::{
    ContentBlock, LlmProvider, Message, Model, Role, SessionFiling, StreamEvent, ThinkingLevel,
    ToolDef,
};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

struct InteractiveCallback;

impl AgentCallback for InteractiveCallback {
    fn on_event(&mut self, evt: AgentEvent) {
        display_event(&evt);
    }
}

pub async fn run(
    mut provider: Box<dyn LlmProvider>,
    model: Model,
    system_prompt: String,
    tools: Vec<ToolDef>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
    thinking: ThinkingLevel,
) -> eyre::Result<()> {
    let sessions_dir = SessionFiling::default_dir()?;
    let mut filing = SessionFiling::new(sessions_dir);
    filing.load_all()?;

    let session_name = session_name_from_prompt(initial_prompt.as_deref());
    filing.new_session(&session_name, &cwd.display().to_string())?;

    let sys_id = filing.next_id();
    let sys_msg = Message::new(sys_id.clone(), Role::System, vec![ContentBlock::text(&system_prompt)]);
    filing.write_message(sys_msg)?;
    let mut session_ids = vec![sys_id];

    if let Some(prompt) = initial_prompt {
        print_user_prefix();
        println!("{}", prompt);

        let user_id = filing.next_id();
        let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(&prompt)]);
        filing.write_message(user_msg)?;
        session_ids.push(user_id);

        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: provider.as_ref(),
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking,
            max_tokens: None,
            cwd: &cwd,
            strategy: agent::naive_strategy,
        };
        let mut cb = InteractiveCallback;
        agent::run(&config, &mut filing, &mut session_ids, &mut cb, cancel).await?;
    }

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    let mut paste_login: Option<ri_ai::registry::PasteCodeState> = None;

    loop {
        if paste_login.is_some() {
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

        if let Some(state) = paste_login.take() {
            match ri_ai::registry::complete_paste_login(state, trimmed).await {
                Ok(_) => {
                    // Re-resolve provider for current model to stay consistent.
                    match ri_ai::registry::resolve(&model.id).await {
                        Ok((p, _)) => provider = p,
                        Err(e) => { eprintln!("\x1b[31m[resolve error: {}]\x1b[0m", e); continue; }
                    }
                    eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
                }
                Err(e) => eprintln!("\x1b[31m[login failed: {}]\x1b[0m", e),
            }
            continue;
        }

        if trimmed == "/quit" || trimmed == "/exit" {
            break;
        }

        if trimmed == "/help" {
            eprintln!("\x1b[33mCommands:\x1b[0m");
            for info in ri_ai::registry::available_logins() {
                eprintln!("  /login {:<14} - {}", info.name, info.display);
            }
            eprintln!("  /quit, /exit        - Exit ri");
            continue;
        }

        if trimmed.starts_with("/login") {
            let login_name = trimmed.strip_prefix("/login").unwrap().trim();
            let login_name = if login_name.is_empty() {
                let flows = ri_ai::registry::available_logins();
                if flows.is_empty() {
                    eprintln!("\x1b[31mNo login flows available.\x1b[0m");
                    continue;
                }
                flows[0].name
            } else {
                login_name
            };

            match ri_ai::registry::start_login(login_name) {
                Ok(ri_ai::registry::LoginFlow::PasteCode { url, state }) => {
                    eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
                    eprintln!("\x1b[4m{}\x1b[0m\n", url);
                    paste_login = Some(state);
                }
                Ok(ri_ai::registry::LoginFlow::LocalCallback { name }) => {
                    eprintln!("\x1b[33mStarting OAuth login...\x1b[0m");
                    match ri_ai::registry::run_callback_login(&name, |url| {
                        eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
                        eprintln!("\x1b[4m{}\x1b[0m\n", url);
                        #[cfg(target_os = "macos")]
                        { let _ = std::process::Command::new("open").arg(url).spawn(); }
                    }).await {
                        Ok(_) => {
                            // Re-resolve provider for current model to stay consistent.
                            match ri_ai::registry::resolve(&model.id).await {
                                Ok((p, _)) => provider = p,
                                Err(e) => { eprintln!("\x1b[31m[resolve error: {}]\x1b[0m", e); continue; }
                            }
                            eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
                        }
                        Err(e) => eprintln!("\x1b[31m[login failed: {}]\x1b[0m", e),
                    }
                }
                Err(e) => eprintln!("\x1b[31m[login error: {}]\x1b[0m", e),
            }
            continue;
        }

        let user_id = filing.next_id();
        let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(trimmed)]);
        filing.write_message(user_msg)?;
        session_ids.push(user_id);

        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: provider.as_ref(),
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking,
            max_tokens: None,
            cwd: &cwd,
            strategy: agent::naive_strategy,
        };

        let mut cb = InteractiveCallback;
        if let Err(e) = agent::run(&config, &mut filing, &mut session_ids, &mut cb, cancel).await {
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

fn session_name_from_prompt(prompt: Option<&str>) -> String {
    match prompt {
        Some(p) => {
            let words: String = p.split_whitespace()
                .take(5)
                .collect::<Vec<_>>()
                .join("-");
            if words.is_empty() { "session".to_string() } else { words }
        }
        None => "interactive".to_string(),
    }
}
