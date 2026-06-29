//! TPM2 preflight gate: the wizard's first screen.
//!
//! On a pass it is skipped (the loop advances straight to Welcome). On a real
//! failure it is a hard stop: an afterglow error screen whose only action is to
//! quit. In `--dry-run` a failure is downgraded to a yellow warning that still
//! lets the operator advance, so dev boxes without a TPM can exercise the flow.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{is_quit, App, Transition};
use crate::theme;

pub fn handle_key(app: &App, key: KeyEvent) -> Transition {
    if is_quit(key) {
        return Transition::Quit;
    }
    // Advancing is only offered in dry-run; a real failing TPM2 cannot proceed.
    if app.dry_run && key.code == KeyCode::Enter {
        return Transition::Next;
    }
    Transition::Stay
}

pub fn draw(frame: &mut Frame, app: &App) {
    let detail = app
        .preflight
        .as_ref()
        .map(|result| result.detail.as_str())
        .unwrap_or_default();

    let (title, lines) = if app.dry_run {
        (" TPM2 not detected ", warning_lines(detail))
    } else {
        (" TPM2 required ", failure_lines(detail))
    };

    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn failure_lines(detail: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Archetype requires a TPM2 security device. None was detected.",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    lines.extend(detail_lines(detail));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "The installer cannot continue.",
        Style::default().fg(theme::FG),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("q", theme::RED),
        text(" or "),
        key("Esc", theme::RED),
        text(" to quit"),
    ]));
    lines
}

fn warning_lines(detail: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "TPM2 not detected \u{2014} dry-run continues; a real install requires one.",
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    lines.extend(detail_lines(detail));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("Enter", theme::GREEN),
        text(" to continue    "),
        key("q", theme::RED),
        text(" to quit"),
    ]));
    lines
}

/// Render the captured `systemd-creds` output (if any) as dim lines.
fn detail_lines(detail: &str) -> Vec<Line<'static>> {
    detail
        .lines()
        .map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme::BRIGHT_BLACK),
            ))
        })
        .collect()
}

fn key(label: &'static str, color: ratatui::style::Color) -> Span<'static> {
    Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn text(label: &'static str) -> Span<'static> {
    Span::styled(label, Style::default().fg(theme::FG))
}
