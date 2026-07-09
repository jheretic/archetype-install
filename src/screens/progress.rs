//! Progress: render the live install log streamed from the worker thread.
//!
//! The worker (see [`crate::install`]) runs repart + post-steps off-thread and
//! sends [`Progress`] messages on a channel. The app drains that channel on
//! each tick (so the loop stays responsive without async) and accumulates them
//! into [`ProgressState`]; this screen only renders. The transition to Result
//! happens in the app once a terminal [`Outcome`] arrives — no key handling
//! advances it.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Transition};
use crate::install::Outcome;
use crate::theme;

/// Accumulated install progress: the current step, the full log, and the
/// terminal outcome once the worker reports it.
#[derive(Default)]
pub struct ProgressState {
    pub step: Option<String>,
    pub log: Vec<String>,
    pub outcome: Option<Outcome>,
    /// The recovery key enrolled on the root, surfaced by the worker. Displayed
    /// on the Recovery screen (with a QR code) before reboot is offered.
    pub recovery_key: Option<String>,
}

/// The braille spinner frames, advanced one per tick.
const SPINNER: [char; 10] = [
    '\u{2807}', '\u{280b}', '\u{2819}', '\u{2838}', '\u{2830}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280f}',
];

/// Progress ignores input; it advances only when the worker finishes.
pub fn handle_key() -> Transition {
    Transition::Stay
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " installing \u{2014} do not power off ",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // current step
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // log
        ])
        .split(inner);

    let step = app.progress.step.as_deref().unwrap_or("starting\u{2026}");
    // Braille spinner cycled by the tick counter (~10 fps) so the user can see
    // the installer is working even while a single step runs for a long time.
    // Frozen (static glyph) once a terminal outcome has arrived.
    let spinner = if app.progress.outcome.is_some() {
        '\u{25b8}' // ▸ (done: no longer animating)
    } else {
        SPINNER[(app.tick_count as usize) % SPINNER.len()]
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{spinner} {step}"),
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );

    let lines: Vec<Line> = app
        .progress
        .log
        .iter()
        .map(|line| {
            let color = if line.starts_with('!') {
                theme::YELLOW
            } else {
                theme::BRIGHT_BLACK
            };
            Line::from(Span::styled(line.clone(), Style::default().fg(color)))
        })
        .collect();

    // Pin the viewport to the tail so the newest lines stay visible.
    let height = rows[2].height as usize;
    let scroll = lines.len().saturating_sub(height) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[2]);
}
