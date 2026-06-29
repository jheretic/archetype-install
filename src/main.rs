//! archetype-install entry point: parse `--dry-run`, set up the terminal, run
//! the app, and restore the terminal on exit.

mod app;
mod disk;
mod event;
mod firstboot;
mod install;
mod layout;
mod preflight;
mod repart;
mod screens;
mod theme;
mod tui;

use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::Result;

use crate::app::{App, Exit};

fn main() -> Result<()> {
    let dry_run = std::env::args().skip(1).any(|arg| arg == "--dry-run");

    let mut terminal = tui::init()?;
    let mut app = App::new(dry_run);
    let result = app.run(&mut terminal);
    tui::restore()?;
    result?;

    // Act on the exit choice only after the terminal is restored, so neither a
    // reboot nor a shell exec happens from inside raw/alt-screen mode.
    match app.exit {
        Exit::Reboot => {
            if let Err(err) = Command::new("systemctl").arg("reboot").status() {
                eprintln!("failed to reboot (run `systemctl reboot` manually): {err}");
            }
        }
        Exit::Shell => {
            // The UI promises a recovery console (esp. after a failed/incomplete
            // install); deliver one by exec'ing an interactive login shell so the
            // operator isn't dropped back to whatever started us (the install
            // service would otherwise just exit).
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            let err = Command::new(&shell).arg("-l").exec();
            // exec only returns on failure.
            eprintln!("failed to start a recovery shell ({shell}): {err}");
        }
        Exit::Quit => {}
    }
    Ok(())
}
