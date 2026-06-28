//! Result: the wizard's terminal screen.
//!
//! Three shapes:
//! - dry-run: "no changes were made", points at the generated definitions.
//! - install success: offers reboot into the new system or dropping to a shell.
//! - install failure: surfaces the verbatim error and leaves a recoverable
//!   console (exit to a shell) rather than halting.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Exit, Transition};
use crate::install::Outcome;
use crate::theme;

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    // On the install paths, `r` reboots and any exit key drops to a shell.
    let installed = app.progress.outcome.is_some();
    match key.code {
        KeyCode::Char('r') if installed && reboot_offered(app) => {
            app.exit = Exit::Reboot;
            Transition::Quit
        }
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
            if installed {
                app.exit = Exit::Shell;
            }
            Transition::Quit
        }
        _ => Transition::Stay,
    }
}

/// Reboot is offered only after a successful install.
fn reboot_offered(app: &App) -> bool {
    matches!(app.progress.outcome, Some(Outcome::Success))
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " done ",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match &app.progress.outcome {
        Some(Outcome::Success) => success_lines(),
        Some(Outcome::Incomplete { detail }) => incomplete_lines(detail),
        Some(Outcome::Failed { step, error }) => failure_lines(step, error),
        None => dry_run_lines(app),
    };

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn dry_run_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Dry-run complete \u{2014} no changes were made.",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if let Some(Ok(dir)) = app.review.as_ref().map(|review| &review.output) {
        lines.push(Line::from(vec![
            Span::styled(
                "Generated definitions written to ",
                Style::default().fg(theme::FG),
            ),
            Span::styled(dir.display().to_string(), Style::default().fg(theme::CYAN)),
        ]));
        lines.push(Line::from(Span::styled(
            "(under /run \u{2014} cleared on reboot)",
            Style::default().fg(theme::BRIGHT_BLACK),
        )));
        lines.push(Line::from(""));
    }

    lines.push(exit_hint());
    lines
}

fn success_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Install complete.",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Remove the install medium before rebooting.",
            Style::default().fg(theme::FG),
        )),
        Line::from(""),
        Line::from(vec![
            key("r", theme::GREEN),
            text(" reboot into the new system    "),
            key("q", theme::YELLOW),
            text(" drop to a shell"),
        ]),
    ]
}

fn incomplete_lines(detail: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Install INCOMPLETE.",
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Partitions were written, but setup did not finish:",
            Style::default().fg(theme::FG),
        )),
        Line::from(Span::styled(
            detail.to_string(),
            Style::default().fg(theme::FG),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Do NOT reboot \u{2014} the system may not boot. Drop to a shell to finish.",
            Style::default().fg(theme::RED),
        )),
        Line::from(""),
        Line::from(vec![
            key("q", theme::GREEN),
            text(" drop to a recovery shell"),
        ]),
    ]
}

fn failure_lines(step: &str, error: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Install FAILED.",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("during: {step}"),
            Style::default().fg(theme::YELLOW),
        )),
        Line::from(""),
    ];
    for line in error.lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme::FG),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "The disk may be partially written. Inspect before retrying.",
        Style::default().fg(theme::BRIGHT_BLACK),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("q", theme::GREEN),
        text(" drop to a recovery shell"),
    ]));
    lines
}

fn exit_hint() -> Line<'static> {
    Line::from(vec![
        key("Enter", theme::GREEN),
        text(" or "),
        key("q", theme::RED),
        text(" to exit"),
    ])
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
