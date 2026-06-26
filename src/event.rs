//! Input + tick event loop. Polls crossterm for key input and emits a periodic
//! tick so animated UI elements keep moving on otherwise idle screens.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};

/// The interval between [`AppEvent::Tick`] events when no input arrives.
const TICK_RATE: Duration = Duration::from_millis(100);

/// A single event delivered to the application loop.
pub enum AppEvent {
    Tick,
    Key(KeyEvent),
}

/// Blocking event source that interleaves key input with steady ticks.
pub struct EventLoop {
    last_tick: Instant,
}

impl EventLoop {
    pub fn new() -> Self {
        Self {
            last_tick: Instant::now(),
        }
    }

    /// Wait for the next key press, or return a tick once [`TICK_RATE`] elapses.
    pub fn next(&mut self) -> Result<AppEvent> {
        loop {
            let timeout = TICK_RATE.saturating_sub(self.last_tick.elapsed());
            if event::poll(timeout)? {
                if let CrosstermEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        return Ok(AppEvent::Key(key));
                    }
                }
            } else {
                self.last_tick = Instant::now();
                return Ok(AppEvent::Tick);
            }
        }
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}
