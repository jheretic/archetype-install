//! Recovery: show the enrolled recovery key and a QR code, and block reboot
//! until the user confirms they have saved it.
//!
//! Reached only after a successful non-dry-run install that enrolled a recovery
//! key (see [`crate::app::App::on_tick`]). Pressing the confirm key advances to
//! Result, where reboot is offered; until then there is no way forward, so the
//! key cannot be skipped past.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Transition};
use crate::recovery::qr_matrix;
use crate::theme;

/// Only the explicit confirm key (Enter) advances; nothing else leaves this
/// screen, so reboot stays blocked until the user acknowledges the key.
pub fn handle_key(_app: &mut App, key: KeyEvent) -> Transition {
    match key.code {
        KeyCode::Enter => Transition::Next,
        _ => Transition::Stay,
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::YELLOW))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " recovery key \u{2014} save this now ",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let key = match app.progress.recovery_key.as_deref() {
        Some(key) => key,
        None => return,
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(4), // heading + key
            Constraint::Min(1),    // QR code
            Constraint::Length(4), // warning + confirm hint
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(key_lines(key)).alignment(Alignment::Center),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(qr_lines(key)).alignment(Alignment::Center),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(footer_lines()).alignment(Alignment::Center),
        rows[2],
    );
}

fn key_lines(key: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "A recovery key was enrolled on the encrypted root:",
            Style::default().fg(theme::FG),
        )),
        Line::from(""),
        Line::from(Span::styled(
            key.to_string(),
            Style::default()
                .fg(theme::BRIGHT_GREEN)
                .add_modifier(Modifier::BOLD),
        )),
    ]
}

/// Render the QR matrix with Unicode half-blocks: each character cell stacks two
/// vertical modules (upper/lower), halving the height so the code fits the
/// terminal. Dark modules are drawn in the foreground colour over the
/// background, matching the afterglow palette.
fn qr_lines(key: &str) -> Vec<Line<'static>> {
    let Some(matrix) = qr_matrix(key) else {
        return vec![Line::from(Span::styled(
            "(could not render QR code)",
            Style::default().fg(theme::BRIGHT_BLACK),
        ))];
    };
    // A 2-module quiet zone around the code is required by the QR spec for
    // reliable scanning.
    const QUIET: usize = 2;
    let width = matrix.len();
    let dark = |r: usize, c: usize| -> bool {
        r >= QUIET
            && c >= QUIET
            && r < width + QUIET
            && c < width + QUIET
            && matrix[r - QUIET][c - QUIET]
    };
    let padded = width + 2 * QUIET;
    let style = Style::default().fg(theme::FG).bg(theme::BG);

    let mut lines = Vec::with_capacity(padded.div_ceil(2));
    for top in (0..padded).step_by(2) {
        let mut cell = String::with_capacity(padded);
        for col in 0..padded {
            let upper = dark(top, col);
            let lower = top + 1 < padded && dark(top + 1, col);
            cell.push(match (upper, lower) {
                (true, true) => '\u{2588}',  // full block
                (true, false) => '\u{2580}', // upper half
                (false, true) => '\u{2584}', // lower half
                (false, false) => ' ',
            });
        }
        lines.push(Line::from(Span::styled(cell, style)));
    }
    lines
}

fn footer_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Write this down or scan it \u{2014} it is the only way to recover if the TPM fails.",
            Style::default().fg(theme::YELLOW),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Enter",
                Style::default()
                    .fg(theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " once you have saved the recovery key",
                Style::default().fg(theme::FG),
            ),
        ]),
    ]
}
