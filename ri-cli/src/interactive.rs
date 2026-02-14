use crate::agent::{self, AgentEvent};
use ri::{
    AuthMethod, LlmProvider, Model, SessionStore,
    StreamEvent, ThinkingLevel, Tool,
};
use std::borrow::Cow;
use std::io;
use std::path::PathBuf;

use crossterm::style::Color;
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};
use reedline::{
    FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, Signal,
};

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

struct RiPrompt;

impl Prompt for RiPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("ri")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(".. ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "(failed) ",
        };
        Cow::Owned(format!("{}search: {} ", prefix, history_search.term))
    }

    fn get_prompt_color(&self) -> Color {
        Color::Cyan
    }

    fn get_indicator_color(&self) -> Color {
        Color::Cyan
    }
}

// ---------------------------------------------------------------------------
// TUI renderer -- drives the inline viewport during agent streaming
// ---------------------------------------------------------------------------

/// Height of the inline viewport: 1 border line + 1 content line.
const VIEWPORT_HEIGHT: u16 = 2;

/// Rendering phase for the status viewport.
enum Phase {
    Waiting,
    Thinking,
    Responding,
    Tool { name: String },
    Idle,
}

/// Renders agent events into a ratatui inline viewport. Completed content
/// (thinking lines, rendered markdown, tool results) is pushed above the
/// viewport via `insert_before` so it lives in terminal scrollback.
/// The viewport itself is a small 2-line status bar showing current activity.
struct TuiRenderer {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    phase: Phase,
    /// Accumulates response text during streaming; rendered as markdown on TextEnd.
    text_buf: String,
    /// Accumulates the current incomplete thinking line for streaming to scrollback.
    thinking_line: String,
    tick: usize,
}

impl TuiRenderer {
    fn new() -> eyre::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )?;

        let mut renderer = Self {
            terminal,
            phase: Phase::Waiting,
            text_buf: String::new(),
            thinking_line: String::new(),
            tick: 0,
        };

        // Draw immediately so the viewport is never blank.
        renderer.render_viewport();

        Ok(renderer)
    }

    /// Push a single styled line into terminal scrollback above the viewport.
    fn emit_line(&mut self, line: Line<'static>) {
        let _ = self.terminal.insert_before(1, |buf| {
            Paragraph::new(line).render(buf.area, buf);
        });
    }

    /// Render markdown text and push it into scrollback above the viewport.
    fn emit_markdown(&mut self, md: &str) {
        if md.trim().is_empty() {
            return;
        }
        let text = tui_markdown::from_str(md);
        if text.lines.is_empty() {
            return;
        }

        let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
        let para = Paragraph::new(text).wrap(Wrap { trim: false });
        let height = para.line_count(width);

        if height == 0 {
            return;
        }

        let height = height.min(u16::MAX as usize) as u16;
        let _ = self.terminal.insert_before(height, |buf| {
            (&para).render(buf.area, buf);
        });
    }

    /// Flush any complete thinking lines (ending with '\n') into scrollback as dim text.
    fn flush_thinking_lines(&mut self) {
        while let Some(pos) = self.thinking_line.find('\n') {
            let line_text: String = self.thinking_line.drain(..=pos).collect();
            let trimmed = line_text.trim_end();
            if !trimmed.is_empty() {
                self.emit_line(Line::styled(
                    trimmed.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }
    }

    /// Draw the 2-line status viewport (border + content).
    fn render_viewport(&mut self) {
        self.tick += 1;
        let spinner = spinner_frame(self.tick);

        let (title, detail) = match &self.phase {
            Phase::Waiting => ("waiting", "sending request...".to_string()),
            Phase::Thinking => ("thinking", tail_line(&self.thinking_line)),
            Phase::Responding => ("responding", tail_line(&self.text_buf)),
            Phase::Tool { name } => ("tool", format!("executing: {}", name)),
            Phase::Idle => ("idle", String::new()),
        };

        let title_str = format!(" {} {} ", spinner, title);

        let _ = self.terminal.draw(|frame| {
            let area = frame.area();
            let block = Block::default()
                .borders(Borders::TOP)
                .title(title_str)
                .style(Style::default().add_modifier(Modifier::DIM));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            let para = Paragraph::new(detail)
                .style(Style::default().add_modifier(Modifier::DIM))
                .wrap(Wrap { trim: false });
            frame.render_widget(para, inner);
        });
    }

    fn handle(&mut self, evt: &AgentEvent) {
        match evt {
            AgentEvent::Stream(se) => match se {
                StreamEvent::ThinkingStart => {
                    self.thinking_line.clear();
                    self.phase = Phase::Thinking;
                    self.render_viewport();
                }
                StreamEvent::ThinkingDelta(d) => {
                    self.thinking_line.push_str(d);
                    self.flush_thinking_lines();
                    self.render_viewport();
                }
                StreamEvent::ThinkingEnd { .. } => {
                    // Flush any remaining incomplete thinking line.
                    if !self.thinking_line.is_empty() {
                        let remaining = std::mem::take(&mut self.thinking_line);
                        self.emit_line(Line::styled(
                            remaining,
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                    self.phase = Phase::Idle;
                }

                StreamEvent::TextStart => {
                    self.text_buf.clear();
                    self.phase = Phase::Responding;
                    self.render_viewport();
                }
                StreamEvent::TextDelta(d) => {
                    self.text_buf.push_str(d);
                    self.render_viewport();
                }
                StreamEvent::TextEnd { .. } => {
                    let text = std::mem::take(&mut self.text_buf);
                    self.emit_markdown(&text);
                    self.phase = Phase::Idle;
                }

                StreamEvent::ToolCallStart { name, .. } => {
                    self.emit_line(Line::from(vec![
                        Span::styled("tool: ", Style::default().fg(ratatui::style::Color::Yellow)),
                        Span::raw(name.clone()),
                    ]));
                }

                StreamEvent::Error(msg) => {
                    self.emit_line(Line::styled(
                        format!("error: {}", msg),
                        Style::default().fg(ratatui::style::Color::Red),
                    ));
                }

                StreamEvent::Usage(u) => {
                    let usage_str = format!(
                        "tokens: {} in / {} out / {} cached",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens
                    );
                    self.emit_line(Line::styled(
                        usage_str,
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }

                _ => {}
            },

            AgentEvent::ToolStart { name, .. } => {
                self.phase = Phase::Tool { name: name.clone() };
                self.render_viewport();
            }

            AgentEvent::ToolEnd { output, is_error, .. } => {
                if *is_error {
                    self.emit_line(Line::styled(
                        format!("tool error: {}", truncate(output, 200)),
                        Style::default().fg(ratatui::style::Color::Red),
                    ));
                } else {
                    self.emit_line(Line::styled(
                        format!("result: {}", truncate(output, 200)),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                self.phase = Phase::Idle;
                self.render_viewport();
            }

            AgentEvent::Error(msg) => {
                self.emit_line(Line::styled(
                    format!("error: {}", msg),
                    Style::default().fg(ratatui::style::Color::Red),
                ));
            }

            AgentEvent::MessageComplete(_) => {}
        }
    }

    /// Clear the viewport and restore normal terminal mode.
    fn teardown(self) {
        drop(self.terminal);
        let _ = crossterm::terminal::disable_raw_mode();
        // Newline so the next reedline prompt starts on a fresh line.
        println!();
    }
}

// ---------------------------------------------------------------------------
// Main interactive loop
// ---------------------------------------------------------------------------

pub async fn run(
    mut provider: Box<dyn LlmProvider>,
    model: Model,
    system_prompt: String,
    tools: Vec<Box<dyn Tool>>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
    thinking: ThinkingLevel,
) -> eyre::Result<()> {
    let session_name = session_name_from_prompt(initial_prompt.as_deref());
    let (mut store, mut message_ids) = SessionStore::init(
        &session_name,
        &cwd,
        &system_prompt,
    )?;

    // Handle an initial prompt passed via CLI --prompt.
    if let Some(prompt) = initial_prompt {
        println!("ri> {}", prompt);
        submit_prompt(
            &prompt, provider.as_ref(), &model, &system_prompt,
            &tools, &mut store, &mut message_ids, &cwd, thinking,
        ).await?;
    }

    // Set up reedline with persistent history.
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ri")
        .join("history.txt");
    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let history = Box::new(
        FileBackedHistory::with_file(10_000, history_path.clone())
            .unwrap_or_else(|_| FileBackedHistory::new(10_000).expect("history init")),
    );
    let mut editor = Reedline::create().with_history(history);
    let prompt = RiPrompt;

    loop {
        let sig = editor.read_line(&prompt);

        match sig {
            Ok(Signal::Success(buffer)) => {
                let trimmed = buffer.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if trimmed == "/quit" || trimmed == "/exit" {
                    break;
                }

                if trimmed == "/help" {
                    print_help();
                    continue;
                }

                if trimmed.starts_with("/login") {
                    handle_login(trimmed, &model, &mut provider, &mut editor, &prompt).await;
                    continue;
                }

                // Normal prompt.
                submit_prompt(
                    trimmed, provider.as_ref(), &model, &system_prompt,
                    &tools, &mut store, &mut message_ids, &cwd, thinking,
                ).await?;
            }

            Ok(Signal::CtrlC) => {
                continue;
            }

            Ok(Signal::CtrlD) => {
                break;
            }

            Err(e) => {
                println!("input error: {}", e);
                break;
            }
        }
    }

    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Submit a user prompt through the agent loop with TUI output
// ---------------------------------------------------------------------------

async fn submit_prompt(
    text: &str,
    provider: &dyn LlmProvider,
    model: &Model,
    system_prompt: &str,
    tools: &[Box<dyn Tool>],
    store: &mut SessionStore,
    message_ids: &mut Vec<String>,
    cwd: &PathBuf,
    thinking: ThinkingLevel,
) -> eyre::Result<()> {
    let mut tui = TuiRenderer::new()?;
    let cancel = tokio_util::sync::CancellationToken::new();

    let events = agent::submit(
        text, provider, model, system_prompt, tools,
        store, message_ids, cwd, thinking, cancel,
    )?;
    tokio::pin!(events);
    while let Some(evt) = events.next().await {
        tui.handle(&evt);
    }

    tui.teardown();
    Ok(())
}

// ---------------------------------------------------------------------------
// Login handling
// ---------------------------------------------------------------------------

async fn handle_login(
    input: &str,
    model: &Model,
    provider: &mut Box<dyn LlmProvider>,
    editor: &mut Reedline,
    prompt: &RiPrompt,
) {
    let login_name = input.strip_prefix("/login").unwrap().trim();

    let login_provider = if login_name.is_empty() {
        ri_ai::registry::all_providers().into_iter().next()
    } else {
        ri_ai::registry::all_providers()
            .into_iter()
            .find(|p| p.id() == login_name)
    };

    let Some(login_provider) = login_provider else {
        println!("\x1b[31mUnknown provider: {}\x1b[0m", login_name);
        return;
    };

    match login_provider.begin_login().await {
        Ok(Some(AuthMethod::PasteCode { url })) => {
            println!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
            println!("\x1b[4m{}\x1b[0m\n", url);
            println!("\x1b[33mPaste the code below:\x1b[0m");

            // Read the paste code immediately -- no cross-iteration state.
            let code = match editor.read_line(prompt) {
                Ok(Signal::Success(buffer)) => buffer.trim().to_string(),
                _ => {
                    println!("\x1b[31mLogin cancelled.\x1b[0m");
                    return;
                }
            };

            match login_provider.complete_login(&code).await {
                Ok(()) => {
                    resolve_provider_after_login(model, provider).await;
                }
                Err(e) => println!("\x1b[31mlogin failed: {}\x1b[0m", e),
            }
        }
        Ok(Some(AuthMethod::LocalCallback { url, port, path })) => {
            println!("\x1b[33mStarting OAuth login...\x1b[0m");
            match run_local_callback_login(login_provider, &url, port, &path).await {
                Ok(()) => {
                    resolve_provider_after_login(model, provider).await;
                }
                Err(e) => println!("\x1b[31mlogin failed: {}\x1b[0m", e),
            }
        }
        Ok(None) => {
            println!("\x1b[33mNo login needed for this provider.\x1b[0m");
        }
        Err(e) => {
            println!("\x1b[31mlogin error: {}\x1b[0m", e);
        }
    }
}

async fn resolve_provider_after_login(model: &Model, provider: &mut Box<dyn LlmProvider>) {
    match ri_ai::registry::resolve(&model.id).await {
        Ok((p, _)) => {
            *provider = p;
            println!("\x1b[32mLogged in successfully.\x1b[0m");
        }
        Err(e) => println!("\x1b[31mresolve error: {}\x1b[0m", e),
    }
}

async fn run_local_callback_login(
    provider: Box<dyn LlmProvider>,
    auth_url: &str,
    port: u16,
    expected_path: &str,
) -> eyre::Result<()> {
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

    println!("\n\x1b[33mVisit this URL to authorize:\x1b[0m");
    println!("\x1b[4m{}\x1b[0m\n", auth_url);
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(auth_url).spawn();
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let handler = {
        let tx = tx.clone();
        move |Query(params): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            async move {
                let mut guard = tx.lock().await;
                if let Some(tx) = guard.take() {
                    if let Some(error) = params.get("error") {
                        let _ = tx.send(Err(error.clone()));
                        return Html("<h1>Authorization failed</h1>".to_string());
                    }
                    if let Some(code) = params.get("code") {
                        let _ = tx.send(Ok(code.clone()));
                        return Html(
                            "<h1>Success</h1><p>You can close this window.</p>"
                                .to_string(),
                        );
                    }
                    let _ =
                        tx.send(Err("No authorization code in callback".into()));
                }
                Html("<h1>Unexpected request</h1>".to_string())
            }
        }
    };

    let app = Router::new().route(expected_path, get(handler));
    let listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .map_err(|e| {
                eyre::eyre!("Failed to bind OAuth callback on port {}: {}", port, e)
            })?;

    let code = tokio::select! {
        result = axum::serve(listener, app) => {
            result.map_err(|e| eyre::eyre!("OAuth callback server error: {}", e))?;
            return Err(eyre::eyre!("OAuth callback server stopped unexpectedly"));
        }
        result = rx => {
            result
                .map_err(|_| eyre::eyre!("OAuth callback channel closed"))?
                .map_err(|e| eyre::eyre!("OAuth error: {}", e))?
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            return Err(eyre::eyre!("OAuth callback timed out after 5 minutes"));
        }
    };

    provider.complete_login(&code).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn print_help() {
    println!("\x1b[33mCommands:\x1b[0m");
    for p in ri_ai::registry::all_providers() {
        println!("  /login {:<20} - {}", p.id(), p.name());
    }
    println!("  /quit, /exit              - Exit ri");
}

fn spinner_frame(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["*", "o", "O", "o"];
    FRAMES[tick % FRAMES.len()]
}

fn tail_line(s: &str) -> String {
    s.lines().next_back().unwrap_or("").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max)
            .last()
            .unwrap_or(0);
        format!("{}... ({} bytes)", &s[..end], s.len())
    }
}

fn session_name_from_prompt(prompt: Option<&str>) -> String {
    match prompt {
        Some(p) => {
            let words: String = p
                .split_whitespace()
                .take(5)
                .collect::<Vec<_>>()
                .join("-");
            if words.is_empty() {
                "session".to_string()
            } else {
                words
            }
        }
        None => "interactive".to_string(),
    }
}
