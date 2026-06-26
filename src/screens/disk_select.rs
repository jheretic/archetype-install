//! Disk selection: list detected install targets and store the chosen one.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{is_quit, App, Transition};
use crate::theme;

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    if is_quit(key) {
        return Transition::Quit;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            move_cursor(app, -1);
            Transition::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_cursor(app, 1);
            Transition::Stay
        }
        KeyCode::Enter if !app.disks.is_empty() => {
            app.config.target = Some(app.disks[app.disk_cursor].clone());
            Transition::Next
        }
        _ => Transition::Stay,
    }
}

fn move_cursor(app: &mut App, delta: isize) {
    if app.disks.is_empty() {
        return;
    }
    let last = app.disks.len() - 1;
    app.disk_cursor = match delta {
        d if d < 0 => app.disk_cursor.saturating_sub(1),
        _ => (app.disk_cursor + 1).min(last),
    };
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " select install target ",
            Style::default()
                .fg(theme::BRIGHT_FG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = frame_block.inner(area);
    frame.render_widget(frame_block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Min(1),    // disk list (or empty notice)
            Constraint::Length(1), // hint
        ])
        .split(inner);

    if app.disks.is_empty() {
        draw_empty(frame, rows[0]);
    } else {
        draw_list(frame, rows[0], app);
    }

    frame.render_widget(
        Paragraph::new(hint(app.disks.is_empty())).alignment(Alignment::Center),
        rows[1],
    );
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "No eligible disks found. The live medium and read-only devices are excluded.",
            Style::default().fg(theme::YELLOW),
        )))
        .alignment(Alignment::Center),
        area,
    );
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .disks
        .iter()
        .map(|disk| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<20}", disk.display_model()),
                    Style::default().fg(theme::FG),
                ),
                Span::styled(
                    format!("{:>12}", disk.human_size()),
                    Style::default().fg(theme::CYAN),
                ),
                Span::styled(
                    format!("   {}", disk.name),
                    Style::default().fg(theme::BRIGHT_BLACK),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(theme::BG)
                .bg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(Some(app.disk_cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

fn hint(empty: bool) -> Line<'static> {
    let key = |label: &'static str, color| {
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let text = |label: &'static str| Span::styled(label, Style::default().fg(theme::FG));

    let mut spans = Vec::new();
    if !empty {
        spans.push(key("up/down", theme::BLUE));
        spans.push(text(" to move   "));
        spans.push(key("Enter", theme::GREEN));
        spans.push(text(" to select   "));
    }
    spans.push(key("q", theme::RED));
    spans.push(text(" to quit"));

    Line::from(spans)
}
