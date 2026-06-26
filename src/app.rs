//! Application state machine: the wizard [`Screen`] enum, the shared
//! [`InstallConfig`] accumulator, and the draw/update loop.
//!
//! Phase 2 only wires Welcome -> Quit. Later phases fill the remaining screens
//! and the transitions between them.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::event::{AppEvent, EventLoop};
use crate::screens::welcome;
use crate::tui::Tui;

/// One step in the install wizard. Variants past [`Screen::Welcome`] are
/// placeholders that later phases render and wire up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum Screen {
    Welcome,
    DiskSelect,
    Sizing,
    Review,
    Confirm,
    Progress,
    Result,
}

/// The result of handling input on a screen: how the wizard should move.
/// `Next`/`Back` are part of the wizard vocabulary; Phase 2 only acts on `Quit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum Transition {
    Stay,
    Next,
    Back,
    Quit,
}

/// Install parameters accumulated across the wizard. Empty in Phase 2; later
/// phases add the chosen disk and the root/swap/home size choices.
#[derive(Default)]
pub struct InstallConfig {}

/// Top-level application state. `dry_run` and `config` are populated now and
/// consumed by later wizard phases.
pub struct App {
    pub screen: Screen,
    #[allow(dead_code)]
    pub dry_run: bool,
    #[allow(dead_code)]
    pub config: InstallConfig,
    pub running: bool,
}

impl App {
    pub fn new(dry_run: bool) -> Self {
        Self {
            screen: Screen::Welcome,
            dry_run,
            config: InstallConfig::default(),
            running: true,
        }
    }

    /// Run the draw/event loop until the user quits.
    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = EventLoop::new();
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            match events.next()? {
                AppEvent::Tick => {}
                AppEvent::Key(key) => self.handle_key(key),
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        match self.screen {
            Screen::Welcome => welcome::draw(frame),
            _ => welcome::draw(frame),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let transition = match self.screen {
            Screen::Welcome => welcome::handle_key(key),
            _ => Transition::Stay,
        };
        self.apply(transition);
    }

    fn apply(&mut self, transition: Transition) {
        match transition {
            Transition::Quit => self.running = false,
            Transition::Stay | Transition::Next | Transition::Back => {}
        }
    }
}

/// Shared helper: treat `q`/`Esc` as a quit request.
pub fn is_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
}
