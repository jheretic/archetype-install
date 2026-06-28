//! Destructive-confirm stub.
//!
//! Reached only on a real (non-dry-run) install. Phase 6 fills this with the
//! type-the-disk-name confirmation and the transition into Progress/execute;
//! for now it is a placeholder that can only go back.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{is_quit, Transition};
use crate::theme;

pub fn handle_key(key: KeyEvent) -> Transition {
    if is_quit(key) {
        return Transition::Quit;
    }
    match key.code {
        KeyCode::Esc => Transition::Back,
        _ => Transition::Stay,
    }
}

pub fn draw(frame: &mut Frame) {
    let area = frame.area();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " confirm install ",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            "Destructive confirmation is implemented in Phase 6.",
            Style::default().fg(theme::YELLOW),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Esc",
                Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to go back", Style::default().fg(theme::FG)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}
