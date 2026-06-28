//! Destructive confirmation: the last gate before partitions are written.
//!
//! The user must TYPE the target device's name exactly (e.g. `/dev/sdb`) to
//! enable proceeding. Anything other than an exact match leaves the proceed
//! action disabled; [`crate::install::InstallPlan::authorize`] re-checks the
//! same match independently, so a UI bug cannot open the destructive path.
//!
//! Reached only on a real (non-dry-run) install. `q` is NOT a quit key here:
//! it is a literal character the user may need to type. Esc goes back.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Transition};
use crate::theme;

/// Whether `typed` exactly names `device`. The single source of truth for the
/// UI's "proceed enabled" state; the install gate checks the same equality.
pub fn matches_device(device: &str, typed: &str) -> bool {
    !device.is_empty() && typed == device
}

/// The chosen target device path, or `""` when none is selected (which can
/// never satisfy [`matches_device`]).
fn target_device(app: &App) -> &str {
    app.config
        .target
        .as_ref()
        .map_or("", |disk| disk.name.as_str())
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    match key.code {
        KeyCode::Esc => Transition::Back,
        KeyCode::Enter => {
            let device = target_device(app).to_string();
            if matches_device(&device, &app.confirm_input) {
                Transition::Next
            } else {
                Transition::Stay
            }
        }
        KeyCode::Backspace => {
            app.confirm_input.pop();
            Transition::Stay
        }
        KeyCode::Char(c) => {
            app.confirm_input.push(c);
            Transition::Stay
        }
        _ => Transition::Stay,
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let device = target_device(app);
    let armed = matches_device(device, &app.confirm_input);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::RED))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " confirm destructive install ",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Min(1),    // warning + what-gets-wiped
            Constraint::Length(1), // input
            Constraint::Length(1), // spacer
            Constraint::Length(1), // hint
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(warning(app, device)), rows[0]);
    frame.render_widget(Paragraph::new(input_line(app, armed)), rows[1]);
    frame.render_widget(
        Paragraph::new(hint(armed)).alignment(Alignment::Center),
        rows[3],
    );
}

fn warning(app: &App, device: &str) -> Vec<Line<'static>> {
    let model = app.config.target.as_ref().map_or_else(
        || "unknown device".to_string(),
        |d| d.display_model().to_string(),
    );
    let size = app
        .config
        .target
        .as_ref()
        .map_or_else(String::new, |d| d.human_size());

    vec![
        Line::from(Span::styled(
            "This ERASES the entire disk. All existing partitions and data will be",
            Style::default().fg(theme::YELLOW),
        )),
        Line::from(Span::styled(
            "destroyed and replaced with a fresh Archetype install.",
            Style::default().fg(theme::YELLOW),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Target: ", Style::default().fg(theme::FG)),
            Span::styled(
                device.to_string(),
                Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {model}  {size}"),
                Style::default().fg(theme::BRIGHT_BLACK),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Type ", Style::default().fg(theme::FG)),
            Span::styled(
                device.to_string(),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " exactly to enable the install.",
                Style::default().fg(theme::FG),
            ),
        ]),
    ]
}

fn input_line(app: &App, armed: bool) -> Line<'static> {
    let value_style = if armed {
        Style::default()
            .fg(theme::GREEN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::FG)
    };
    Line::from(vec![
        Span::styled("> ", Style::default().fg(theme::BRIGHT_BLACK)),
        Span::styled(app.confirm_input.clone(), value_style),
        Span::styled("\u{2588}", Style::default().fg(theme::BRIGHT_BLACK)),
    ])
}

fn hint(armed: bool) -> Line<'static> {
    let key = |label: &'static str, color| {
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let text = |label: &'static str| Span::styled(label, Style::default().fg(theme::FG));

    let mut spans = Vec::new();
    if armed {
        spans.push(key("Enter", theme::RED));
        spans.push(text(" ERASE & install   "));
    } else {
        spans.push(Span::styled(
            "type the device name to enable   ",
            Style::default().fg(theme::BRIGHT_BLACK),
        ));
    }
    spans.push(key("Esc", theme::GREEN));
    spans.push(text(" back (safe)"));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_arms_proceed() {
        assert!(matches_device("/dev/sdb", "/dev/sdb"));
    }

    #[test]
    fn mismatches_are_rejected() {
        assert!(!matches_device("/dev/sdb", "/dev/sda"));
        assert!(!matches_device("/dev/sdb", "/dev/sdb1"));
        assert!(!matches_device("/dev/sdb", "sdb"));
        assert!(!matches_device("/dev/sdb", "/dev/sdb "));
        assert!(!matches_device("/dev/sdb", " /dev/sdb"));
        assert!(!matches_device("/dev/sdb", "/DEV/SDB"));
        assert!(!matches_device("/dev/sdb", ""));
    }

    #[test]
    fn empty_device_never_matches() {
        assert!(!matches_device("", ""));
        assert!(!matches_device("", "anything"));
    }
}
