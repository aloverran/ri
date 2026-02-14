use ri_agent::{self as agent, AgentCallback, AgentEvent, RunConfig};
use ri::{
    AuthMethod, ContentBlock, LlmProvider, Message, Model, Role, SessionFiling, StreamEvent,
    ThinkingLevel, ToolDef,
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
    let session_name = session_name_from_prompt(initial_prompt.as_deref());
    let (mut filing, mut session_ids) = SessionFiling::init(
        &session_name,
        &cwd.display().to_string(),
        &system_prompt,
    )?;

    let config = RunConfig {
        model,
        system_prompt,
        tools,
        thinking,
        max_tokens: None,
        cwd,
        strategy: agent::naive_strategy(),
    };

    if let Some(prompt) = initial_prompt {
        print_user_prefix();
        println!("{}", prompt);

        let user_id = filing.next_id();
        let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(&prompt)]);
        filing.write_message(user_msg)?;
        session_ids.push(user_id);

        let cancel = tokio_util::sync::CancellationToken::new();
        let mut cb = InteractiveCallback;
        agent::run(provider.as_ref(), &config, &mut filing, &mut session_ids, &mut cb, cancel).await?;
    }

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    // State for paste-code login flows (Anthropic).
    let mut awaiting_paste: Option<Box<dyn LlmProvider>> = None;

    loop {
        if awaiting_paste.is_some() {
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

        // Handle paste-code completion.
        if let Some(login_provider) = awaiting_paste.take() {
            match login_provider.complete_login(trimmed).await {
                Ok(()) => {
                    // Re-resolve provider for current model.
                    match ri_ai::registry::resolve(&config.model.id).await {
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
            for p in ri_ai::registry::all_providers() {
                eprintln!("  /login {:<20} - {}", p.id(), p.name());
            }
            eprintln!("  /quit, /exit              - Exit ri");
            continue;
        }

        if trimmed.starts_with("/login") {
            let login_name = trimmed.strip_prefix("/login").unwrap().trim();

            // Find the provider to log in with.
            let login_provider = if login_name.is_empty() {
                // Default: first provider.
                ri_ai::registry::all_providers().into_iter().next()
            } else {
                ri_ai::registry::all_providers().into_iter()
                    .find(|p| p.id() == login_name)
            };

            let Some(login_provider) = login_provider else {
                eprintln!("\x1b[31mUnknown provider: {}\x1b[0m", login_name);
                continue;
            };

            match login_provider.begin_login().await {
                Ok(Some(AuthMethod::PasteCode { url })) => {
                    eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
                    eprintln!("\x1b[4m{}\x1b[0m\n", url);
                    awaiting_paste = Some(login_provider);
                }
                Ok(Some(AuthMethod::LocalCallback { url, port, path })) => {
                    eprintln!("\x1b[33mStarting OAuth login...\x1b[0m");

                    // Run the callback server and complete login.
                    match run_local_callback_login(login_provider, &url, port, &path).await {
                        Ok(()) => {
                            match ri_ai::registry::resolve(&config.model.id).await {
                                Ok((p, _)) => provider = p,
                                Err(e) => { eprintln!("\x1b[31m[resolve error: {}]\x1b[0m", e); continue; }
                            }
                            eprintln!("\x1b[32mLogged in successfully.\x1b[0m");
                        }
                        Err(e) => eprintln!("\x1b[31m[login failed: {}]\x1b[0m", e),
                    }
                }
                Ok(None) => {
                    eprintln!("\x1b[33mNo login needed for this provider.\x1b[0m");
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
        let mut cb = InteractiveCallback;
        if let Err(e) = agent::run(provider.as_ref(), &config, &mut filing, &mut session_ids, &mut cb, cancel).await {
            eprintln!("\x1b[31m[error: {}]\x1b[0m", e);
        }
    }

    println!();
    Ok(())
}

// Run a local OAuth callback server, open browser, wait for code, complete login.
async fn run_local_callback_login(
    provider: Box<dyn LlmProvider>,
    auth_url: &str,
    port: u16,
    expected_path: &str,
) -> eyre::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Open browser
    eprintln!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
    eprintln!("\x1b[4m{}\x1b[0m\n", auth_url);
    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(auth_url).spawn(); }

    // Start local callback server
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await
        .map_err(|e| eyre::eyre!("Failed to bind OAuth callback on port {}: {}", port, e))?;

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(300);

    let code = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(eyre::eyre!("OAuth callback timed out"));
        }

        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| eyre::eyre!("OAuth callback timed out"))?
            .map_err(|e| eyre::eyre!("Accept failed: {}", e))?;

        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        let request_str = loop {
            if buf.len() >= 8192 { break String::from_utf8_lossy(&buf).to_string(); }
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                stream.read(&mut tmp),
            ).await {
                Ok(Ok(0)) => break String::from_utf8_lossy(&buf).to_string(),
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.contains(&b'\n') { break String::from_utf8_lossy(&buf).to_string(); }
                }
                _ => break String::from_utf8_lossy(&buf).to_string(),
            }
        };

        let first_line = request_str.lines().next().unwrap_or("");
        let path_and_query = first_line.strip_prefix("GET ")
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");

        let query_start = path_and_query.find('?').unwrap_or(path_and_query.len());
        let path = &path_and_query[..query_start];
        let query = if query_start < path_and_query.len() { &path_and_query[query_start + 1..] } else { "" };

        if path != expected_path {
            let resp = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        }

        let mut auth_code = None;
        let mut error = None;

        for param in query.split('&') {
            if let Some((key, value)) = param.split_once('=') {
                let value = ri_ai::url::decode(value);
                match key {
                    "code" => auth_code = Some(value),
                    "error" => error = Some(value),
                    _ => {}
                }
            }
        }

        if let Some(err) = error {
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>Failed</h1>";
            let _ = stream.write_all(resp.as_bytes()).await;
            return Err(eyre::eyre!("OAuth error: {}", err));
        }

        let code = auth_code.ok_or_else(|| eyre::eyre!("No authorization code"))?;

        let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<h1>Success</h1><p>You can close this window.</p>";
        let _ = stream.write_all(resp.as_bytes()).await;

        break code;
    };

    provider.complete_login(&code).await?;
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
