//! Terminal lifecycle: raw mode + alternate screen entry/exit, plus a panic
//! hook that restores the terminal before the default hook prints, so a panic
//! never leaves the bare console in raw/alt-screen garbage.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enter raw mode + the alternate screen and install the panic-restore hook.
pub fn init() -> Result<Tui> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    install_panic_hook();
    Terminal::new(CrosstermBackend::new(io::stdout())).map_err(Into::into)
}

/// Leave the alternate screen and disable raw mode. Idempotent enough to be
/// safe to call from both the normal exit path and the panic hook.
pub fn restore() -> Result<()> {
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Chain a terminal restore in front of the existing panic hook so the console
/// is sane before any panic message is printed.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        previous(info);
    }));
}
