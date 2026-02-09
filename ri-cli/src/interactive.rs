// Interactive mode: REPL-style agent interaction.
//
// Reads user input from stdin, sends to agent, streams response to terminal.
// Uses basic line-oriented I/O (no raw mode / ratatui for v1).
// The agent runs in the background and events are printed as they arrive.

use ri_ai::oauth::{LoginState, OAuthProvider};
use ri_core::event::{AgentEvent, AssistantStreamEvent, EventReceiver};
use ri_core::types::AgentMessage;
use std::io::Write;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::auth::AuthStore;

/// Run interactive mode.
///
/// The agent is owned by the caller. We take handles for steering/follow-up
/// and run the display + input loops concurrently.
pub async fn run(
    mut agent: ri_core::agent::Agent,
    event_rx: EventReceiver,
    initial_prompt: Option<String>,
) -> eyre::Result<()> {
    let cancel = agent.cancel_token();

    // Channel for user input -> agent prompt
    let (input_tx, mut input_rx) = mpsc::channel::<String>(16);

    // Spawn display task (consumes agent events, prints to terminal)
    let display_cancel = cancel.clone();
    let display_task = tokio::spawn(display_loop(event_rx, display_cancel));

    // Spawn input task (reads lines from stdin)
    let input_cancel = cancel.clone();
    tokio::spawn(input_loop(input_tx, input_cancel));

    // If there's an initial prompt, run it first
    if let Some(prompt) = initial_prompt {
        print_user_prefix();
        println!("{prompt}");
        agent.prompt(AgentMessage::user(prompt)).await?;
    }

    // Login state: when Some, the next input line is treated as the OAuth code.
    let mut login_pending: Option<LoginState> = None;

    // Main loop: wait for user input, then run agent
    loop {
        if login_pending.is_some() {
            eprint!("\x1b[33mPaste code: \x1b[0m");
            let _ = std::io::stderr().flush();
        } else {
            print_prompt();
        }

        tokio::select! {
            input = input_rx.recv() => {
                match input {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        // If we're waiting for a login code, complete the flow
                        if let Some(state) = login_pending.take() {
                            if let Some(key) = complete_login(trimmed, &state).await {
                                agent.config.api_key = key;
                            }
                            continue;
                        }

                        // Handle special commands
                        if trimmed == "/quit" || trimmed == "/exit" {
                            break;
                        }

                        if trimmed == "/help" {
                            print_help();
                            continue;
                        }

                        if trimmed == "/login" {
                            login_pending = begin_login();
                            continue;
                        }

                        // Send to agent. Errors are printed but don't kill the REPL.
                        if let Err(e) = agent.prompt(AgentMessage::user(trimmed.to_string())).await {
                            eprintln!("\x1b[31m[error: {e}]\x1b[0m");
                        }
                    }
                    None => break, // Input channel closed (stdin EOF)
                }
            }
            _ = cancel.cancelled() => break,
        }
    }

    // Clean up
    cancel.cancel();
    let _ = display_task.await;

    println!();
    Ok(())
}

fn print_prompt() {
    eprint!("\x1b[36m> \x1b[0m");
    let _ = std::io::stderr().flush();
}

fn print_user_prefix() {
    eprint!("\x1b[36m> \x1b[0m");
    let _ = std::io::stderr().flush();
}

fn print_help() {
    eprintln!("\x1b[33mCommands:\x1b[0m");
    eprintln!("  /login        - Log in via OAuth (Anthropic)");
    eprintln!("  /quit, /exit  - Exit ri");
    eprintln!("  /help         - Show this help");
    eprintln!();
}

fn begin_login() -> Option<LoginState> {
    let provider = ri_ai::oauth::anthropic_oauth::AnthropicOAuth::new();
    match provider.begin_login() {
        Ok((result, state)) => {
            eprintln!();
            eprintln!("\x1b[33mVisit this URL to authorize:\x1b[0m");
            eprintln!("\x1b[4m{}\x1b[0m", result.url);
            if let Some(instructions) = result.instructions {
                eprintln!("\x1b[2m{instructions}\x1b[0m");
            }
            eprintln!();
            Some(state)
        }
        Err(e) => {
            eprintln!("\x1b[31m[login error: {e}]\x1b[0m");
            None
        }
    }
}

async fn complete_login(code: &str, state: &LoginState) -> Option<String> {
    let provider = ri_ai::oauth::anthropic_oauth::AnthropicOAuth::new();
    match provider.complete_login(code, state).await {
        Ok(creds) => {
            let key = creds.access.clone();
            let mut store = AuthStore::load();
            store.set("anthropic", creds);
            match store.save() {
                Ok(()) => {
                    eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
                    eprintln!("\x1b[2mCredentials saved to {}\x1b[0m", AuthStore::path().display());
                }
                Err(e) => {
                    eprintln!("\x1b[31m[failed to save credentials: {e}]\x1b[0m");
                }
            }
            Some(key)
        }
        Err(e) => {
            eprintln!("\x1b[31m[login failed: {e}]\x1b[0m");
            None
        }
    }
}

async fn input_loop(tx: mpsc::Sender<String>, cancel: tokio_util::sync::CancellationToken) {
    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            line = lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        if tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(_) => break,
                }
            }
        }
    }
}

async fn display_loop(
    mut rx: EventReceiver,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut in_text = false;
    let mut in_thinking = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            event = rx.recv() => {
                match event {
                    Ok(evt) => {
                        match evt {
                            AgentEvent::MessageUpdate(ref stream_evt) => match stream_evt {
                                AssistantStreamEvent::TextStart => {
                                    in_text = true;
                                }
                                AssistantStreamEvent::TextDelta { delta } => {
                                    print!("{delta}");
                                    let _ = std::io::stdout().flush();
                                }
                                AssistantStreamEvent::TextEnd => {
                                    if in_text {
                                        println!();
                                        in_text = false;
                                    }
                                }
                                AssistantStreamEvent::ThinkingStart => {
                                    in_thinking = true;
                                    eprint!("\x1b[2m"); // dim
                                }
                                AssistantStreamEvent::ThinkingDelta { delta } => {
                                    eprint!("{delta}");
                                    let _ = std::io::stderr().flush();
                                }
                                AssistantStreamEvent::ThinkingEnd => {
                                    if in_thinking {
                                        eprint!("\x1b[0m"); // reset
                                        eprintln!();
                                        in_thinking = false;
                                    }
                                }
                                AssistantStreamEvent::ToolCallStart { name, .. } => {
                                    eprintln!("\x1b[33m[tool: {name}]\x1b[0m");
                                }
                                AssistantStreamEvent::Error { message } => {
                                    eprintln!("\x1b[31m[error: {message}]\x1b[0m");
                                }
                                _ => {}
                            },
                            AgentEvent::ToolExecutionStart { tool_call } => {
                                eprintln!(
                                    "\x1b[33m[executing: {}]\x1b[0m",
                                    tool_call.name
                                );
                            }
                            AgentEvent::ToolExecutionEnd {
                                result,
                                is_error,
                                ..
                            } => {
                                for block in &result {
                                    if let ri_core::types::ContentBlock::Text { text } = block {
                                        if is_error {
                                            eprintln!("\x1b[31m[tool error: {}]\x1b[0m", text);
                                        } else {
                                            // Truncate long tool output (char-safe)
                                            let display = if text.len() > 200 {
                                                let end = text
                                                    .char_indices()
                                                    .map(|(i, _)| i)
                                                    .take_while(|&i| i <= 200)
                                                    .last()
                                                    .unwrap_or(0);
                                                format!(
                                                    "{}... ({} bytes)",
                                                    &text[..end],
                                                    text.len()
                                                )
                                            } else {
                                                text.clone()
                                            };
                                            eprintln!(
                                                "\x1b[2m[result: {}]\x1b[0m",
                                                display
                                            );
                                        }
                                    }
                                }
                            }
                            AgentEvent::AgentEnd => {
                                // Agent finished this turn. The main loop will
                                // print a new prompt.
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("\x1b[33m[skipped {n} events]\x1b[0m");
                    }
                }
            }
        }
    }
}
