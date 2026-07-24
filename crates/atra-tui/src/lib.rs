use std::{io, path::PathBuf};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

mod app;
mod controller;
mod input;
mod ui;

pub async fn run(endpoint: PathBuf, history_path: PathBuf) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let result = app::App::load(endpoint, history_path)
        .await?
        .run(&mut terminal.terminal)
        .await;
    terminal.restore()?;
    result
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .context("failed to enter the terminal UI")?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))
                .context("failed to initialize the terminal")?,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if !self.restored {
            disable_raw_mode().context("failed to disable terminal raw mode")?;
            execute!(
                self.terminal.backend_mut(),
                PopKeyboardEnhancementFlags,
                DisableMouseCapture,
                LeaveAlternateScreen
            )
            .context("failed to leave the terminal UI")?;
            self.terminal
                .show_cursor()
                .context("failed to show cursor")?;
            self.restored = true;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
