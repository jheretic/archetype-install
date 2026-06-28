//! Result: the wizard's terminal screen.
//!
//! On the dry-run path this reports that nothing was changed and points at the
//! generated definitions. The real-install summary arrives in Phase 6.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Transition};
use crate::theme;

pub fn handle_key(key: KeyEvent) -> Transition {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => Transition::Quit,
        _ => Transition::Stay,
    }
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

    lines.push(Line::from(vec![
        Span::styled(
            "Enter",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" or ", Style::default().fg(theme::FG)),
        Span::styled(
            "q",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to exit", Style::default().fg(theme::FG)),
    ]));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}
