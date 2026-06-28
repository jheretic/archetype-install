//! Application state machine: the wizard [`Screen`] enum, the shared
//! [`InstallConfig`] accumulator, and the draw/update loop.
//!
//! Phase 2 only wires Welcome -> Quit. Later phases fill the remaining screens
//! and the transitions between them.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::disk::{enumerate_disks, Disk};
use crate::event::{AppEvent, EventLoop};
use crate::layout::{SizeChoice, Sizing};
use crate::screens::review::ReviewState;
use crate::screens::{confirm, disk_select, result, review, sizing, welcome};
use crate::tui::Tui;

const GIB: u64 = 1024 * 1024 * 1024;

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

/// Install parameters accumulated across the wizard. `target` is the disk
/// chosen on DiskSelect; `sizing` is the root/swap/home allocation set on the
/// Sizing screen.
pub struct InstallConfig {
    pub target: Option<Disk>,
    pub sizing: Sizing,
}

impl Default for InstallConfig {
    /// root and swap start as adjustable fixed sizes; home grows to fill the
    /// remaining free space.
    fn default() -> Self {
        Self {
            target: None,
            sizing: Sizing {
                root: SizeChoice::Fixed(16 * GIB),
                swap: Some(SizeChoice::Fixed(4 * GIB)),
                home: SizeChoice::Grow {
                    weight: 1000,
                    min_bytes: 0,
                },
            },
        }
    }
}

/// Top-level application state. `dry_run` and `config` are populated now and
/// consumed by later wizard phases.
pub struct App {
    pub screen: Screen,
    pub dry_run: bool,
    pub config: InstallConfig,
    pub disks: Vec<Disk>,
    pub disk_cursor: usize,
    pub sizing_cursor: usize,
    pub review: Option<ReviewState>,
    pub running: bool,
}

impl App {
    pub fn new(dry_run: bool) -> Self {
        Self {
            screen: Screen::Welcome,
            dry_run,
            config: InstallConfig::default(),
            disks: Vec::new(),
            disk_cursor: 0,
            sizing_cursor: 0,
            review: None,
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
            Screen::DiskSelect => disk_select::draw(frame, self),
            Screen::Sizing => sizing::draw(frame, self),
            Screen::Review => review::draw(frame, self),
            Screen::Confirm => confirm::draw(frame),
            Screen::Result => result::draw(frame, self),
            Screen::Progress => welcome::draw(frame),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let transition = match self.screen {
            Screen::Welcome => welcome::handle_key(key),
            Screen::DiskSelect => disk_select::handle_key(self, key),
            Screen::Sizing => sizing::handle_key(self, key),
            Screen::Review => review::handle_key(self, key),
            Screen::Confirm => confirm::handle_key(key),
            Screen::Result => result::handle_key(key),
            Screen::Progress => Transition::Stay,
        };
        self.apply(transition);
    }

    fn apply(&mut self, transition: Transition) {
        match transition {
            Transition::Quit => self.running = false,
            Transition::Back => self.go_back(),
            Transition::Next => self.go_next(),
            Transition::Stay => {}
        }
    }

    fn go_next(&mut self) {
        self.screen = match self.screen {
            Screen::Welcome => {
                self.load_disks();
                Screen::DiskSelect
            }
            Screen::DiskSelect => Screen::Sizing,
            Screen::Sizing => {
                self.build_review();
                Screen::Review
            }
            // Dry-run stops at Result without ever reaching Confirm/Progress;
            // a real install would continue to the (Phase 6) Confirm screen.
            Screen::Review if self.dry_run => Screen::Result,
            Screen::Review => Screen::Confirm,
            other => other,
        };
    }

    fn go_back(&mut self) {
        self.screen = match self.screen {
            Screen::DiskSelect => Screen::Welcome,
            Screen::Sizing => Screen::DiskSelect,
            Screen::Review => Screen::Sizing,
            Screen::Confirm => Screen::Review,
            other => other,
        };
    }

    /// Render the `repart.d` set for the chosen disk + sizing and (optionally)
    /// run a guarded `systemd-repart --dry-run` for the Review screen.
    fn build_review(&mut self) {
        self.review = self
            .config
            .target
            .as_ref()
            .map(|disk| ReviewState::build(&self.config.sizing, disk.size_bytes, &disk.name));
    }

    /// Populate the disk list on entering DiskSelect. An enumeration failure
    /// leaves the list empty so the screen shows its empty-state notice.
    fn load_disks(&mut self) {
        self.disks = enumerate_disks().unwrap_or_default();
        self.disk_cursor = 0;
    }
}

/// Shared helper: treat `q`/`Esc` as a quit request.
pub fn is_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
}
