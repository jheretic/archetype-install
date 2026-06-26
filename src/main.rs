//! archetype-install entry point: parse `--dry-run`, set up the terminal, run
//! the app, and restore the terminal on exit.

mod app;
mod event;
mod screens;
mod theme;
mod tui;

use anyhow::Result;

use crate::app::App;

fn main() -> Result<()> {
    let dry_run = std::env::args().skip(1).any(|arg| arg == "--dry-run");

    let mut terminal = tui::init()?;
    let result = App::new(dry_run).run(&mut terminal);
    tui::restore()?;
    result
}
