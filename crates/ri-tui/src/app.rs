// Application state and rendering.
//
// Follows Gemini's recommendation: the TUI renders state snapshots,
// never drives the application. The agent loop sends state updates
// via channels, the TUI renders them.

use ratatui::prelude::*;
use ratatui::widgets::*;

pub struct AppState {
    pub messages: Vec<DisplayMessage>,
    pub is_streaming: bool,
    pub current_text: String,
    pub model_name: String,
    pub status: String,
}

pub struct DisplayMessage {
    pub role: String,
    pub content: String,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            is_streaming: false,
            current_text: String::new(),
            model_name: String::new(),
            status: "Ready".to_string(),
        }
    }
}

pub fn render(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Messages area
    let messages: Vec<Line> = state
        .messages
        .iter()
        .flat_map(|msg| {
            vec![
                Line::from(Span::styled(
                    format!("[{}]", msg.role),
                    Style::default().fg(if msg.role == "user" {
                        Color::Cyan
                    } else {
                        Color::Green
                    }),
                )),
                Line::from(msg.content.as_str()),
                Line::from(""),
            ]
        })
        .collect();

    let chat = Paragraph::new(messages)
        .block(Block::default().borders(Borders::ALL).title(" ri "))
        .wrap(Wrap { trim: false });
    frame.render_widget(chat, chunks[0]);

    // Input area
    let input = Paragraph::new(state.current_text.as_str())
        .block(Block::default().borders(Borders::ALL).title(" Input "));
    frame.render_widget(input, chunks[1]);

    // Status bar
    let status = Paragraph::new(Line::from(vec![
        Span::styled(&state.model_name, Style::default().fg(Color::Yellow)),
        Span::raw(" | "),
        Span::raw(&state.status),
    ]));
    frame.render_widget(status, chunks[2]);
}
