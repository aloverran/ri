// Terminal abstraction over crossterm.

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use std::io;

pub struct TerminalHandle {
    pub columns: u16,
    pub rows: u16,
}

impl TerminalHandle {
    pub fn new() -> io::Result<Self> {
        let (columns, rows) = crossterm::terminal::size()?;
        Ok(Self { columns, rows })
    }

    pub fn enter_raw_mode(&self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(())
    }

    pub fn exit_raw_mode(&self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()?;
        Ok(())
    }

    pub async fn next_event(&self) -> io::Result<Event> {
        // Poll for terminal events asynchronously
        tokio::task::spawn_blocking(|| {
            event::read()
        })
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }
}
