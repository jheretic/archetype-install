//! Application state machine: the wizard [`Screen`] enum, the shared
//! [`InstallConfig`] accumulator, and the draw/update loop.
//!
//! The flow is Welcome -> Config -> DiskSelect -> Sizing -> Review, then on a
//! real install Confirm -> Progress -> Result (dry-run ends at Result
//! directly).
//! The destructive Progress step runs on a worker thread (see
//! [`crate::install`]); the loop drains its channel on each tick.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::disk::{enumerate_disks, Disk};
use crate::event::{AppEvent, EventLoop};
use crate::firstboot::FirstbootConfig;
use crate::install::{self, Install, InstallPlan, Outcome, Progress};
use crate::layout::{SizeChoice, Sizing};
use crate::preflight::{self, PreflightResult};
use crate::screens::progress::ProgressState;
use crate::screens::review::ReviewState;
use crate::screens::{
    confirm, disk_select, firstboot as firstboot_screen, preflight as preflight_screen, progress,
    recovery, result, review, sizing, welcome,
};
use crate::tui::Tui;

const GIB: u64 = 1024 * 1024 * 1024;

/// One step in the install wizard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Preflight,
    Welcome,
    Config,
    DiskSelect,
    Sizing,
    Review,
    Confirm,
    Progress,
    Recovery,
    Result,
}

/// The result of handling input on a screen: how the wizard should move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transition {
    Stay,
    Next,
    Back,
    Quit,
}

/// What the process should do after the event loop exits. Read by `main` once
/// the terminal is restored, so a reboot never happens inside raw/alt-screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Exit {
    /// Quit the installer (dry-run end, or shell drop after install).
    #[default]
    Quit,
    /// `systemctl reboot` into the freshly installed system.
    Reboot,
    /// Leave the user at a console after install (success or recovery).
    Shell,
}

/// Install parameters accumulated across the wizard. `target` is the disk
/// chosen on DiskSelect; `sizing` is the root/swap/home allocation set on the
/// Sizing screen.
pub struct InstallConfig {
    pub target: Option<Disk>,
    pub sizing: Sizing,
    pub firstboot: FirstbootConfig,
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
                home: Some(SizeChoice::Grow {
                    weight: 1000,
                    min_bytes: 0,
                }),
            },
            firstboot: FirstbootConfig::default(),
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
    /// Exact-value edit buffer for the selected sizing field. `Some(text)` while
    /// the user is typing a size (e.g. "40G"); `None` when not editing. The
    /// sizing screen renders the buffer with a cursor and commits it on Enter.
    pub sizing_edit: Option<String>,
    pub firstboot_cursor: usize,
    /// Masked root password entry; never persisted. Cleared after hashing.
    pub password: String,
    pub password_confirm: String,
    /// TPM2 unlock mode: true = PIN (default, hardened), false = automatic
    /// (no PIN; relies on firmware boot-order + password, see the firstboot
    /// screen warning). Determines whether the PIN fields below are collected.
    pub tpm_pin_mode: bool,
    /// Masked TPM2 PIN entry; never persisted. Moved into the config on commit
    /// (PIN mode only) and cleared.
    pub tpm_pin: String,
    pub tpm_pin_confirm: String,
    pub review: Option<ReviewState>,
    /// The startup TPM2 preflight verdict, read by the Preflight screen.
    pub preflight: Option<PreflightResult>,
    pub running: bool,
    /// Monotonic tick counter (advances ~10x/sec) driving the Progress spinner.
    pub tick_count: u64,
    pub confirm_input: String,
    pub progress: ProgressState,
    pub exit: Exit,
    /// The running install worker, present only while on the Progress screen.
    install: Option<Install>,
}

impl App {
    pub fn new(dry_run: bool) -> Self {
        // The TPM2 check runs once at startup. A pass skips straight to Welcome;
        // a failure lands on Preflight (a hard stop, or a dry-run warning).
        let preflight = preflight::check();
        let screen = if preflight.ok {
            Screen::Welcome
        } else {
            Screen::Preflight
        };
        Self {
            screen,
            dry_run,
            config: InstallConfig::default(),
            disks: Vec::new(),
            disk_cursor: 0,
            sizing_cursor: 0,
            sizing_edit: None,
            firstboot_cursor: 0,
            password: String::new(),
            password_confirm: String::new(),
            tpm_pin_mode: true,
            tpm_pin: String::new(),
            tpm_pin_confirm: String::new(),
            review: None,
            preflight: Some(preflight),
            running: true,
            tick_count: 0,
            confirm_input: String::new(),
            progress: ProgressState::default(),
            exit: Exit::Quit,
            install: None,
        }
    }

    /// Run the draw/event loop until the user quits.
    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = EventLoop::new();
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            match events.next()? {
                AppEvent::Tick => {
                    // Advance the animation frame (~10 fps at the 100ms tick) so
                    // the Progress screen's spinner moves, reassuring the user
                    // the installer is alive during long steps.
                    self.tick_count = self.tick_count.wrapping_add(1);
                    self.on_tick();
                }
                AppEvent::Key(key) => self.handle_key(key),
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        match self.screen {
            Screen::Preflight => preflight_screen::draw(frame, self),
            Screen::Welcome => welcome::draw(frame),
            Screen::Config => firstboot_screen::draw(frame, self),
            Screen::DiskSelect => disk_select::draw(frame, self),
            Screen::Sizing => sizing::draw(frame, self),
            Screen::Review => review::draw(frame, self),
            Screen::Confirm => confirm::draw(frame, self),
            Screen::Recovery => recovery::draw(frame, self),
            Screen::Result => result::draw(frame, self),
            Screen::Progress => progress::draw(frame, self),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let transition = match self.screen {
            Screen::Preflight => preflight_screen::handle_key(self, key),
            Screen::Welcome => welcome::handle_key(key),
            Screen::Config => firstboot_screen::handle_key(self, key),
            Screen::DiskSelect => disk_select::handle_key(self, key),
            Screen::Sizing => sizing::handle_key(self, key),
            Screen::Review => review::handle_key(self, key),
            Screen::Confirm => confirm::handle_key(self, key),
            Screen::Recovery => recovery::handle_key(self, key),
            Screen::Result => result::handle_key(self, key),
            Screen::Progress => progress::handle_key(),
        };
        self.apply(transition);
    }

    fn apply(&mut self, transition: Transition) {
        match transition {
            Transition::Quit => {
                // Quitting a REAL install (e.g. `q` on the welcome/preflight/etc.
                // screens) drops to a shell rather than just exiting: the install
                // service has nothing to fall back to, so a bare exit would leave
                // a blank console. Only promote the default Exit::Quit -- screens
                // that chose Reboot/Shell explicitly (Result) keep their choice.
                // Dry-run keeps the plain exit (no shell) so it stays a no-op.
                if !self.dry_run && self.exit == Exit::Quit {
                    self.exit = Exit::Shell;
                }
                self.running = false;
            }
            Transition::Back => self.go_back(),
            Transition::Next => self.go_next(),
            Transition::Stay => {}
        }
    }

    fn go_next(&mut self) {
        self.screen = match self.screen {
            // Only reachable in dry-run (a real TPM2 failure offers no advance).
            Screen::Preflight => Screen::Welcome,
            Screen::Welcome => Screen::Config,
            Screen::Config => {
                // TODO(phase3): root is now LOCKED and the admin path is the
                // homed wheel user; there is no live root password to set here.
                // Phase 3 wires the credstore write in install.rs.
                self.load_disks();
                Screen::DiskSelect
            }
            Screen::DiskSelect => Screen::Sizing,
            Screen::Sizing => {
                self.build_review();
                Screen::Review
            }
            // Dry-run stops at Result without ever reaching Confirm/Progress.
            Screen::Review if self.dry_run => Screen::Result,
            Screen::Review => Screen::Confirm,
            // Confirm only advances after its type-to-wipe gate passed. We
            // re-authorize independently here: if the gate cannot be cleared
            // (it always can at this point), stay on Confirm rather than risk
            // an ungated jump into Progress.
            Screen::Confirm => {
                if self.start_install() {
                    Screen::Progress
                } else {
                    Screen::Confirm
                }
            }
            // The recovery key has been acknowledged; reboot is now permitted.
            Screen::Recovery => Screen::Result,
            other => other,
        };
    }

    /// Authorize and spawn the install worker. Returns false (staying on
    /// Confirm) if the safety gate rejects, so Progress is unreachable without
    /// an authorized [`InstallPlan`].
    fn start_install(&mut self) -> bool {
        let device = match self.config.target.as_ref() {
            Some(disk) => disk.name.clone(),
            None => return false,
        };
        match InstallPlan::authorize(
            self.dry_run,
            &device,
            &self.confirm_input,
            self.config.firstboot.clone(),
            self.config.sizing.home.is_some(),
        ) {
            Some(plan) => {
                self.progress = ProgressState::default();
                self.install = Some(install::spawn(plan));
                true
            }
            None => false,
        }
    }

    /// Drain any queued progress messages on each tick so the worker thread
    /// never blocks and the log stays current. On a terminal outcome, join the
    /// worker and move to Result.
    fn on_tick(&mut self) {
        use std::sync::mpsc::TryRecvError;

        let Some(install) = self.install.as_ref() else {
            return;
        };
        let mut finished = false;
        loop {
            match install.progress.try_recv() {
                Ok(Progress::Step(step)) => self.progress.step = Some(step),
                Ok(Progress::Line(line)) => self.progress.log.push(line),
                Ok(Progress::RecoveryKey(key)) => self.progress.recovery_key = Some(key),
                Ok(Progress::Done(outcome)) => {
                    self.progress.outcome = Some(outcome);
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                // The worker dropped the channel without a terminal Done
                // (e.g. it panicked). Surface a failure rather than hang on
                // Progress; the panic hook already restored the terminal.
                Err(TryRecvError::Disconnected) => {
                    if self.progress.outcome.is_none() {
                        self.progress.outcome = Some(Outcome::Failed {
                            step: self
                                .progress
                                .step
                                .clone()
                                .unwrap_or_else(|| "install".to_string()),
                            error: "the install worker stopped unexpectedly".to_string(),
                        });
                    }
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            if let Some(mut install) = self.install.take() {
                install.join();
            }
            // On a successful install with a recovery key, divert to the
            // Recovery screen first; it blocks reboot until the user confirms
            // they have saved the key, then advances to Result. Any other
            // outcome (incomplete/failed, or no key) goes straight to Result.
            self.screen = if matches!(self.progress.outcome, Some(Outcome::Success))
                && self.progress.recovery_key.is_some()
            {
                Screen::Recovery
            } else {
                Screen::Result
            };
        }
    }

    fn go_back(&mut self) {
        self.screen = match self.screen {
            Screen::Config => Screen::Welcome,
            Screen::DiskSelect => Screen::Config,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quitting_a_real_install_drops_to_shell() {
        let mut app = App::new(false);
        app.apply(Transition::Quit);
        assert!(!app.running);
        assert_eq!(app.exit, Exit::Shell);
    }

    #[test]
    fn quitting_a_dry_run_just_exits() {
        let mut app = App::new(true);
        app.apply(Transition::Quit);
        assert!(!app.running);
        assert_eq!(app.exit, Exit::Quit);
    }

    #[test]
    fn explicit_exit_choice_survives_quit() {
        // The Result screen sets exit (e.g. Reboot) BEFORE returning Quit; the
        // promotion must not clobber it.
        let mut app = App::new(false);
        app.exit = Exit::Reboot;
        app.apply(Transition::Quit);
        assert_eq!(app.exit, Exit::Reboot);
    }
}
