//! Welcome splash: the chevron-ribbon wordmark, a title, and the entry prompt.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
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
    if key.code == KeyCode::Enter {
        return Transition::Next;
    }
    Transition::Stay
}

pub fn draw(frame: &mut Frame) {
    let area = frame.area();

    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG));
    let inner = frame_block.inner(area);
    frame.render_widget(frame_block, area);

    let content = centered(inner, 60, 9);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // ribbon
            Constraint::Length(1),
            Constraint::Length(1), // title
            Constraint::Length(1), // subtitle
            Constraint::Length(1),
            Constraint::Length(1), // prompt
        ])
        .split(content);

    frame.render_widget(
        Paragraph::new(theme::ribbon()).alignment(Alignment::Center),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Archetype Linux Installer",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "self-replicating immutable install",
            Style::default().fg(theme::CYAN),
        )))
        .alignment(Alignment::Center),
        rows[3],
    );

    let prompt = Line::from(vec![
        Span::styled("press ", Style::default().fg(theme::FG)),
        Span::styled(
            "Enter",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to begin   ", Style::default().fg(theme::FG)),
        Span::styled(
            "q",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to quit", Style::default().fg(theme::FG)),
    ]);
    frame.render_widget(Paragraph::new(prompt).alignment(Alignment::Center), rows[5]);
}

/// Center a `width` x `height` box inside `area`, clamped to `area`'s bounds.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect::new(x, y, width, height)
}
