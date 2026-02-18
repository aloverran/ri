//! Interactive TUI for agent sessions.
//!
//! Uses a small ratatui Viewport::Inline pinned to the bottom of the terminal.
//! Completed content is pushed to normal terminal scrollback via insert_before.
//! The viewport only handles live concerns: streaming preview, input, and status.

use crate::agent::{self, AgentEvent};
use ri::{AuthMethod, LlmProvider, Model, SessionStore, StreamEvent, ThinkingLevel, Tool, Usage};

use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
    Frame, Terminal, TerminalOptions, Viewport,
};
use tui_textarea::TextArea;

/// Fixed height of the inline viewport at the bottom of the terminal.
const VIEWPORT_HEIGHT: u16 = 8;

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

enum Phase {
    Input,
    Waiting,
    Thinking,
    Responding,
    Tool(String),
}

impl Phase {
    fn label(&self) -> &str {
        match self {
            Phase::Input => "input",
            Phase::Waiting => "waiting",
            Phase::Thinking => "thinking",
            Phase::Responding => "responding",
            Phase::Tool(_) => "tool",
        }
    }

    fn detail(&self) -> String {
        match self {
            Phase::Input => String::new(),
            Phase::Waiting => "sending request...".into(),
            Phase::Thinking => "reasoning...".into(),
            Phase::Responding => "writing...".into(),
            Phase::Tool(name) => format!("executing: {}", name),
        }
    }
}

// ---------------------------------------------------------------------------
// TUI state (separated from Terminal to avoid borrow conflicts in draw)
// ---------------------------------------------------------------------------

struct TuiState {
    phase: Phase,
    text_buf: String,
    text_emitted: usize,
    in_code_fence: bool,
    thinking_buf: String,
    thinking_emitted: usize,
    textarea: TextArea<'static>,
    total_usage: Usage,
    model_name: String,
    tick: usize,
}

// ---------------------------------------------------------------------------
// TUI handle
// ---------------------------------------------------------------------------

struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    state: TuiState,
}

impl Tui {
    fn new(model_name: String) -> eyre::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let (_, term_height) = crossterm::terminal::size()?;
        let height = VIEWPORT_HEIGHT.min(term_height);

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        let mut tui = Self {
            terminal,
            state: TuiState {
                phase: Phase::Input,
                text_buf: String::new(),
                text_emitted: 0,
                in_code_fence: false,
                thinking_buf: String::new(),
                thinking_emitted: 0,
                textarea: new_textarea(),
                total_usage: Usage::default(),
                model_name,
                tick: 0,
            },
        };
        tui.draw()?;
        Ok(tui)
    }

    /// Push styled lines to scrollback above the viewport, then redraw.
    /// The emit and redraw are wrapped in synchronized output so the
    /// terminal displays the result atomically.
    fn emit_and_draw(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if !lines.is_empty() {
            let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
            let height = wrapped_height(&lines, width);
            if height > 0 {
                sync_start()?;
                self.terminal.insert_before(height, |buf| {
                    Paragraph::new(Text::from(lines))
                        .wrap(Wrap { trim: false })
                        .render(buf.area, buf);
                })?;
                self.draw_inner()?;
                sync_end()?;
                return Ok(());
            }
        }
        self.draw()
    }

    fn draw(&mut self) -> io::Result<()> {
        self.draw_inner()
    }

    fn draw_inner(&mut self) -> io::Result<()> {
        self.state.tick += 1;
        self.terminal
            .draw(|frame| render_viewport(frame, &self.state))?;
        Ok(())
    }

    fn handle_resize(&mut self) -> io::Result<()> {
        self.draw()
    }

    // -- Progressive emission --

    /// Emit completed paragraphs from text_buf to scrollback.
    /// A "safe boundary" is a \n\n that's outside an unclosed code fence.
    fn try_emit_text(&mut self) -> io::Result<()> {
        let (emit_str, new_offset, new_fence) = {
            let buf = &self.state.text_buf;
            let from = self.state.text_emitted;
            let bytes = buf.as_bytes();
            let mut pos = from;
            let mut last_safe = from;
            let mut in_fence = self.state.in_code_fence;

            while pos < bytes.len() {
                if (pos == 0 || bytes[pos - 1] == b'\n')
                    && pos + 3 <= bytes.len()
                    && &bytes[pos..pos + 3] == b"```"
                {
                    in_fence = !in_fence;
                }
                if !in_fence
                    && pos + 1 < bytes.len()
                    && bytes[pos] == b'\n'
                    && bytes[pos + 1] == b'\n'
                {
                    last_safe = pos + 2;
                }
                pos += 1;
            }

            if last_safe > from {
                (Some(buf[from..last_safe].to_string()), last_safe, in_fence)
            } else {
                (None, from, in_fence)
            }
        };

        self.state.in_code_fence = new_fence;
        self.state.text_emitted = new_offset;

        if let Some(text) = emit_str {
            let md = tui_markdown::from_str(&text);
            let lines: Vec<Line<'static>> = md.lines.into_iter().map(own_line).collect();
            self.emit_and_draw(lines)
        } else {
            self.draw()
        }
    }

    /// Emit completed lines from thinking_buf to scrollback.
    fn try_emit_thinking(&mut self) -> io::Result<()> {
        let emit_str = {
            let buf = &self.state.thinking_buf;
            let from = self.state.thinking_emitted;
            buf[from..].rfind('\n').map(|i| {
                let boundary = from + i + 1;
                (buf[from..boundary].to_string(), boundary)
            })
        };

        if let Some((text, new_offset)) = emit_str {
            self.state.thinking_emitted = new_offset;
            let lines: Vec<Line<'static>> = text
                .lines()
                .map(|l| {
                    Line::styled(
                        l.to_string(),
                        Style::default().add_modifier(Modifier::DIM),
                    )
                })
                .collect();
            self.emit_and_draw(lines)
        } else {
            self.draw()
        }
    }

    /// Flush all remaining text_buf content to scrollback.
    fn flush_text(&mut self) -> io::Result<()> {
        let remaining = self.state.text_buf[self.state.text_emitted..].to_string();
        self.state.text_buf.clear();
        self.state.text_emitted = 0;
        self.state.in_code_fence = false;
        if !remaining.is_empty() {
            let md = tui_markdown::from_str(&remaining);
            let lines: Vec<Line<'static>> = md.lines.into_iter().map(own_line).collect();
            self.emit_and_draw(lines)
        } else {
            Ok(())
        }
    }

    /// Flush all remaining thinking_buf content to scrollback.
    fn flush_thinking(&mut self) -> io::Result<()> {
        let remaining = self.state.thinking_buf[self.state.thinking_emitted..].to_string();
        self.state.thinking_buf.clear();
        self.state.thinking_emitted = 0;
        if !remaining.is_empty() {
            let lines: Vec<Line<'static>> = remaining
                .lines()
                .map(|l| {
                    Line::styled(
                        l.to_string(),
                        Style::default().add_modifier(Modifier::DIM),
                    )
                })
                .collect();
            self.emit_and_draw(lines)
        } else {
            Ok(())
        }
    }

    // -- Agent events --

    fn handle_agent_event(&mut self, evt: &AgentEvent) -> io::Result<()> {
        match evt {
            AgentEvent::Stream(se) => self.handle_stream_event(se),
            AgentEvent::ToolStart { name, .. } => {
                self.state.phase = Phase::Tool(name.clone());
                self.emit_and_draw(vec![Line::from(vec![
                    Span::styled("tool: ", Style::default().fg(Color::Yellow)),
                    Span::raw(name.clone()),
                ])])
            }
            AgentEvent::ToolEnd {
                output, is_error, ..
            } => {
                let style = if *is_error {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                let display = truncate(output, 50_000);
                let lines: Vec<Line<'static>> = display
                    .lines()
                    .map(|l| Line::styled(l.to_string(), style))
                    .collect();
                self.state.phase = Phase::Waiting;
                self.emit_and_draw(lines)
            }
            AgentEvent::MessageComplete(_) => Ok(()),
            AgentEvent::Error(msg) => self.emit_and_draw(vec![Line::styled(
                format!("error: {}", msg),
                Style::default().fg(Color::Red),
            )]),
        }
    }

    fn handle_stream_event(&mut self, se: &StreamEvent) -> io::Result<()> {
        match se {
            StreamEvent::ThinkingStart => {
                self.state.thinking_buf.clear();
                self.state.thinking_emitted = 0;
                self.state.phase = Phase::Thinking;
                self.draw()
            }
            StreamEvent::ThinkingDelta(d) => {
                self.state.thinking_buf.push_str(d);
                self.try_emit_thinking()
            }
            StreamEvent::ThinkingEnd { .. } => {
                self.flush_thinking()?;
                self.state.phase = Phase::Waiting;
                self.draw()
            }
            StreamEvent::TextStart => {
                self.state.text_buf.clear();
                self.state.text_emitted = 0;
                self.state.in_code_fence = false;
                self.state.phase = Phase::Responding;
                self.draw()
            }
            StreamEvent::TextDelta(d) => {
                self.state.text_buf.push_str(d);
                self.try_emit_text()
            }
            StreamEvent::TextEnd { .. } => {
                self.flush_text()?;
                self.state.phase = Phase::Waiting;
                self.draw()
            }
            StreamEvent::ToolCallStart { name, .. } => {
                self.state.phase = Phase::Tool(name.clone());
                self.draw()
            }
            StreamEvent::Usage(u) => {
                self.state.total_usage.input_tokens += u.input_tokens;
                self.state.total_usage.output_tokens += u.output_tokens;
                self.state.total_usage.cache_read_tokens += u.cache_read_tokens;
                self.state.total_usage.cache_write_tokens += u.cache_write_tokens;
                self.emit_and_draw(vec![Line::styled(
                    format!(
                        "tokens: {} in / {} out / {} cached",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                )])
            }
            StreamEvent::Error(msg) => self.emit_and_draw(vec![Line::styled(
                format!("error: {}", msg),
                Style::default().fg(Color::Red),
            )]),
            _ => self.draw(),
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Clear the viewport so stale content doesn't linger after exit.
        let _ = self.terminal.draw(|frame| {
            frame.render_widget(Paragraph::new(""), frame.area());
        });
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Viewport rendering
// ---------------------------------------------------------------------------

fn render_viewport(frame: &mut Frame, state: &TuiState) {
    let area = frame.area();

    let chunks = Layout::vertical([
        Constraint::Min(0),    // main: preview or input
        Constraint::Length(1), // status bar
    ])
    .split(area);

    let main_area = chunks[0];
    let status_area = chunks[1];

    if matches!(state.phase, Phase::Input) {
        frame.render_widget(&state.textarea, main_area);
    } else {
        render_preview(frame, state, main_area);
    }

    render_status_bar(frame, state, status_area);
}

/// Render a live preview of in-progress streaming content.
fn render_preview(frame: &mut Frame, state: &TuiState, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let lines: Vec<Line<'static>> = match &state.phase {
        Phase::Thinking => {
            let pending = &state.thinking_buf[state.thinking_emitted..];
            if pending.is_empty() {
                vec![Line::styled(
                    "thinking...",
                    Style::default().add_modifier(Modifier::DIM),
                )]
            } else {
                pending
                    .lines()
                    .map(|l| {
                        Line::styled(
                            l.to_string(),
                            Style::default().add_modifier(Modifier::DIM),
                        )
                    })
                    .collect()
            }
        }
        Phase::Responding => {
            let pending = &state.text_buf[state.text_emitted..];
            if pending.is_empty() {
                vec![]
            } else {
                let text = tui_markdown::from_str(pending);
                text.lines.into_iter().map(own_line).collect()
            }
        }
        _ => vec![Line::styled(
            state.phase.detail(),
            Style::default().add_modifier(Modifier::DIM),
        )],
    };

    // Scroll to tail so we always show the most recent content.
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = para.line_count(area.width) as u16;
    let scroll = total.saturating_sub(area.height);
    frame.render_widget(para.scroll((scroll, 0)), area);
}

fn render_status_bar(frame: &mut Frame, state: &TuiState, area: Rect) {
    let spinner = spinner_frame(state.tick);

    let left = format!(" {} {} | {} ", spinner, state.phase.label(), state.model_name);
    let right = format!(
        " {}in/{}out/{}cache ",
        state.total_usage.input_tokens,
        state.total_usage.output_tokens,
        state.total_usage.cache_read_tokens,
    );

    let bar_width = area.width as usize;
    let content_len = left.len() + right.len();
    let padding = if bar_width > content_len {
        " ".repeat(bar_width - content_len)
    } else {
        String::new()
    };

    let bar = Line::from(vec![Span::raw(left), Span::raw(padding), Span::raw(right)])
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));

    frame.render_widget(Paragraph::new(bar), area);
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_block(
        Block::default()
            .borders(Borders::TOP)
            .title(" ri> ")
            .style(Style::default().fg(Color::Cyan)),
    );
    ta.set_cursor_line_style(Style::default());
    ta
}

enum InputResult {
    Submit(String),
    Quit,
}

async fn read_input(tui: &mut Tui, events: &mut EventStream) -> io::Result<InputResult> {
    loop {
        let event = events.next().await;
        match event {
            Some(Ok(Event::Key(key))) => {
                if key.code == KeyCode::Char('d')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return Ok(InputResult::Quit);
                }
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return Ok(InputResult::Quit);
                }
                if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
                    let text = tui.state.textarea.lines().join("\n");
                    tui.state.textarea = new_textarea();
                    return Ok(InputResult::Submit(text));
                }
                tui.state.textarea.input(key);
                tui.draw()?;
            }
            Some(Ok(Event::Resize(_, _))) => {
                tui.handle_resize()?;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            None => return Ok(InputResult::Quit),
        }
    }
}

// ---------------------------------------------------------------------------
// Main loop
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
    let (mut store, mut message_ids) = SessionStore::init(&session_name, &cwd, &system_prompt)?;

    let mut tui = Tui::new(model.name.clone())?;
    let mut events = EventStream::new();

    if let Some(prompt) = initial_prompt {
        run_prompt(
            &prompt,
            &mut tui,
            provider.as_ref(),
            &model,
            &system_prompt,
            &tools,
            &mut store,
            &mut message_ids,
            &cwd,
            thinking,
            &mut events,
        )
        .await?;
    }

    loop {
        tui.state.phase = Phase::Input;
        tui.draw()?;

        match read_input(&mut tui, &mut events).await? {
            InputResult::Submit(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "/quit" || trimmed == "/exit" {
                    break;
                }
                if trimmed == "/help" {
                    let help = help_text();
                    let md = tui_markdown::from_str(&help);
                    let lines = md.lines.into_iter().map(own_line).collect();
                    tui.emit_and_draw(lines)?;
                    continue;
                }
                if trimmed.starts_with("/login") {
                    handle_login(&trimmed, &model, &mut provider, &mut tui).await;
                    continue;
                }
                run_prompt(
                    &trimmed,
                    &mut tui,
                    provider.as_ref(),
                    &model,
                    &system_prompt,
                    &tools,
                    &mut store,
                    &mut message_ids,
                    &cwd,
                    thinking,
                    &mut events,
                )
                .await?;
            }
            InputResult::Quit => break,
        }
    }

    drop(tui);
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt submission + agent streaming
// ---------------------------------------------------------------------------

async fn run_prompt(
    text: &str,
    tui: &mut Tui,
    provider: &dyn LlmProvider,
    model: &Model,
    system_prompt: &str,
    tools: &[Box<dyn Tool>],
    store: &mut SessionStore,
    message_ids: &mut Vec<String>,
    cwd: &PathBuf,
    thinking: ThinkingLevel,
    term_events: &mut EventStream,
) -> eyre::Result<()> {
    tui.emit_and_draw(vec![Line::from(vec![
        Span::styled("ri> ", Style::default().fg(Color::Cyan)),
        Span::raw(text.to_string()),
    ])])?;

    tui.state.phase = Phase::Waiting;
    tui.draw()?;

    let cancel = tokio_util::sync::CancellationToken::new();
    let agent_stream = agent::submit(
        text, provider, model, system_prompt, tools, store, message_ids, cwd, thinking,
        cancel.clone(),
    )?;
    tokio::pin!(agent_stream);

    loop {
        tokio::select! {
            agent_evt = agent_stream.next() => {
                match agent_evt {
                    Some(evt) => { tui.handle_agent_event(&evt)?; }
                    None => break,
                }
            }
            term_evt = term_events.next() => {
                match term_evt {
                    Some(Ok(Event::Key(key))) => {
                        if (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                            || key.code == KeyCode::Esc
                        {
                            cancel.cancel();
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        tui.handle_resize()?;
                    }
                    _ => {}
                }
            }
        }
    }

    // Flush any remaining streaming content to scrollback (e.g. after cancellation).
    if !tui.state.text_buf.is_empty() {
        tui.flush_text()?;
    }
    if !tui.state.thinking_buf.is_empty() {
        tui.flush_thinking()?;
    }

    tui.state.phase = Phase::Input;
    tui.draw()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

async fn handle_login(
    input: &str,
    model: &Model,
    provider: &mut Box<dyn LlmProvider>,
    tui: &mut Tui,
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
        let _ = tui.emit_and_draw(vec![Line::styled(
            format!("Unknown provider: {}", login_name),
            Style::default().fg(Color::Red),
        )]);
        return;
    };

    match login_provider.begin_login().await {
        Ok(Some(AuthMethod::PasteCode { url })) => {
            let msg = format!(
                "Visit this URL to authorize:\n{}\n\nPaste-code login not yet supported in TUI. Use --mode print.",
                url
            );
            let md = tui_markdown::from_str(&msg);
            let _ = tui.emit_and_draw(md.lines.into_iter().map(own_line).collect());
        }
        Ok(Some(AuthMethod::LocalCallback { url, port, path })) => {
            let msg = format!("Starting OAuth login...\nVisit: {}", url);
            let md = tui_markdown::from_str(&msg);
            let _ = tui.emit_and_draw(md.lines.into_iter().map(own_line).collect());

            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(&url).spawn();
            }

            match run_local_callback_login(login_provider, &url, port, &path).await {
                Ok(()) => match ri_ai::registry::resolve(&model.id).await {
                    Ok((p, _)) => {
                        *provider = p;
                        let _ = tui.emit_and_draw(vec![Line::styled(
                            "Logged in successfully.",
                            Style::default().fg(Color::Green),
                        )]);
                    }
                    Err(e) => {
                        let _ = tui.emit_and_draw(vec![Line::styled(
                            format!("resolve error: {}", e),
                            Style::default().fg(Color::Red),
                        )]);
                    }
                },
                Err(e) => {
                    let _ = tui.emit_and_draw(vec![Line::styled(
                        format!("login failed: {}", e),
                        Style::default().fg(Color::Red),
                    )]);
                }
            }
        }
        Ok(None) => {
            let _ = tui.emit_and_draw(vec![Line::raw(
                "No login needed for this provider.",
            )]);
        }
        Err(e) => {
            let _ = tui.emit_and_draw(vec![Line::styled(
                format!("login error: {}", e),
                Style::default().fg(Color::Red),
            )]);
        }
    }
}

async fn run_local_callback_login(
    provider: Box<dyn LlmProvider>,
    _auth_url: &str,
    port: u16,
    expected_path: &str,
) -> eyre::Result<()> {
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

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
                            "<h1>Success</h1><p>You can close this window.</p>".to_string(),
                        );
                    }
                    let _ = tx.send(Err("No authorization code in callback".into()));
                }
                Html("<h1>Unexpected request</h1>".to_string())
            }
        }
    };

    let app = Router::new().route(expected_path, get(handler));
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .map_err(|e| eyre::eyre!("Failed to bind OAuth callback on port {}: {}", port, e))?;

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
// Synchronized output (DEC 2026)
// ---------------------------------------------------------------------------

fn sync_start() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?2026h")?;
    out.flush()
}

fn sync_end() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn own_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

fn spinner_frame(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["*", "o", "O", "o"];
    FRAMES[tick % FRAMES.len()]
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
        format!("{}...", &s[..end])
    }
}

fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    let text = Text::from(lines.to_vec());
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    para.line_count(width).min(u16::MAX as usize) as u16
}

fn help_text() -> String {
    let mut text = String::from("**Commands:**\n");
    for p in ri_ai::registry::all_providers() {
        text.push_str(&format!("- `/login {}` - {}\n", p.id(), p.name()));
    }
    text.push_str("- `/quit`, `/exit` - Exit ri\n");
    text.push_str("- `Ctrl+C` - Cancel running agent\n");
    text
}

fn session_name_from_prompt(prompt: Option<&str>) -> String {
    match prompt {
        Some(p) => {
            let words: String = p.split_whitespace().take(5).collect::<Vec<_>>().join("-");
            if words.is_empty() {
                "session".to_string()
            } else {
                words
            }
        }
        None => "interactive".to_string(),
    }
}
