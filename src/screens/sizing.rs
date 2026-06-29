//! Partition sizing: allocate the disk's free space (size minus
//! [`layout::FIXED_TOTAL_BYTES`]) across root, swap, and home.
//!
//! root and swap are adjustable fixed sizes; home grows to soak whatever
//! remains. A live gauge shows the committed fraction of the free space and
//! turns red when the request is invalid, which also blocks advancing. The
//! sizing math and validation live in [`crate::layout`]; this screen only edits
//! the [`Sizing`] and renders the result.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph};
use ratatui::Frame;

use crate::app::{is_quit, App, Transition};
use crate::layout::{self, SizeChoice, Sizing};
use crate::theme;

const GIB: u64 = 1024 * 1024 * 1024;
/// How much one left/right keypress moves the selected size.
const STEP_BYTES: u64 = GIB;
/// Smallest swap we keep while it is enabled; below this, `s` disables it.
const SWAP_MIN_BYTES: u64 = GIB;

/// The adjustable fields, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Root,
    Swap,
}

const FIELDS: [Field; 2] = [Field::Root, Field::Swap];

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    if is_quit(key) {
        return Transition::Quit;
    }
    match key.code {
        KeyCode::Esc => Transition::Back,
        KeyCode::Up | KeyCode::Char('k') => {
            app.sizing_cursor = app.sizing_cursor.saturating_sub(1);
            Transition::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.sizing_cursor = (app.sizing_cursor + 1).min(FIELDS.len() - 1);
            Transition::Stay
        }
        KeyCode::Left | KeyCode::Char('h') => {
            adjust(app, -(STEP_BYTES as i64));
            Transition::Stay
        }
        KeyCode::Right | KeyCode::Char('l') => {
            adjust(app, STEP_BYTES as i64);
            Transition::Stay
        }
        KeyCode::Char('s') => {
            toggle_swap(&mut app.config.sizing);
            Transition::Stay
        }
        KeyCode::Char('o') => {
            toggle_home(&mut app.config.sizing);
            Transition::Stay
        }
        KeyCode::Enter if is_valid(app) => Transition::Next,
        _ => Transition::Stay,
    }
}

/// Disk free space, or 0 when no target is selected or the disk is too small.
fn available_bytes(app: &App) -> u64 {
    app.config
        .target
        .as_ref()
        .and_then(|disk| layout::allocatable_bytes(disk.size_bytes).ok())
        .unwrap_or(0)
}

/// Bytes the current choices are guaranteed to claim (root + swap floor); home
/// grows from a zero floor and so contributes nothing here.
fn committed_bytes(sizing: &Sizing) -> u64 {
    choice_bytes(sizing.root) + sizing.swap.map_or(0, choice_bytes)
}

fn choice_bytes(choice: SizeChoice) -> u64 {
    match choice {
        SizeChoice::Fixed(bytes) => bytes,
        SizeChoice::Grow { min_bytes, .. } => min_bytes,
    }
}

/// Move the selected field by `delta` bytes, clamped to its floor and to the
/// free space left for the other committed partitions.
fn adjust(app: &mut App, delta: i64) {
    let available = available_bytes(app);
    let sizing = &mut app.config.sizing;
    let field = FIELDS[app.sizing_cursor];

    let (current, floor) = match field {
        Field::Root => (choice_bytes(sizing.root), layout::ROOT_MIN_BYTES),
        Field::Swap => match sizing.swap {
            Some(choice) => (choice_bytes(choice), SWAP_MIN_BYTES),
            None => return,
        },
    };

    let others = committed_bytes(sizing) - current;
    let ceiling = available.saturating_sub(others);
    let next = current
        .saturating_add_signed(delta)
        .clamp(floor, ceiling.max(floor));

    match field {
        Field::Root => sizing.root = SizeChoice::Fixed(next),
        Field::Swap => sizing.swap = Some(SizeChoice::Fixed(next)),
    }
}

/// Enable swap (at its floor) when off, or disable it when on.
fn toggle_swap(sizing: &mut Sizing) {
    sizing.swap = match sizing.swap {
        Some(_) => None,
        None => Some(SizeChoice::Fixed(SWAP_MIN_BYTES)),
    };
}

/// Toggle the home partition. When off, its space is left as free GPT space for
/// the user to partition later; when restored, it grows to fill the remainder.
fn toggle_home(sizing: &mut Sizing) {
    sizing.home = match sizing.home {
        Some(_) => None,
        None => Some(SizeChoice::Grow {
            weight: 1000,
            min_bytes: 0,
        }),
    };
}

/// Whether the current sizing validates against the chosen disk.
fn is_valid(app: &App) -> bool {
    app.config
        .target
        .as_ref()
        .is_some_and(|disk| layout::plan(&app.config.sizing, disk.size_bytes).is_ok())
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " allocate free space ",
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
            Constraint::Length(1), // root row
            Constraint::Length(1), // swap row
            Constraint::Length(1), // home row
            Constraint::Length(1), // spacer
            Constraint::Length(1), // gauge
            Constraint::Min(1),    // status
            Constraint::Length(1), // hint
        ])
        .split(inner);

    let sizing = &app.config.sizing;
    let available = available_bytes(app);
    let committed = committed_bytes(sizing);
    let remaining = available.saturating_sub(committed);

    draw_field(
        frame,
        rows[0],
        "root",
        app,
        Field::Root,
        choice_bytes(sizing.root),
    );
    draw_swap(frame, rows[1], app);
    draw_home(frame, rows[2], app, remaining);
    draw_gauge(frame, rows[4], committed, available);
    draw_status(frame, rows[5], app, available);

    frame.render_widget(
        Paragraph::new(hint(sizing.swap.is_some(), sizing.home.is_some()))
            .alignment(Alignment::Center),
        rows[6],
    );
}

fn draw_field(frame: &mut Frame, area: Rect, label: &str, app: &App, field: Field, bytes: u64) {
    let selected = FIELDS[app.sizing_cursor] == field;
    frame.render_widget(
        Paragraph::new(field_line(label, &human_bytes(bytes), selected)),
        area,
    );
}

fn draw_swap(frame: &mut Frame, area: Rect, app: &App) {
    let selected = FIELDS[app.sizing_cursor] == Field::Swap;
    let value = match app.config.sizing.swap {
        Some(choice) => human_bytes(choice_bytes(choice)),
        None => "disabled".to_string(),
    };
    frame.render_widget(Paragraph::new(field_line("swap", &value, selected)), area);
}

fn draw_home(frame: &mut Frame, area: Rect, app: &App, remaining: u64) {
    let line = if app.config.sizing.home.is_some() {
        Line::from(vec![
            Span::styled("    home  ", Style::default().fg(theme::FG)),
            Span::styled(
                format!("{} ", human_bytes(remaining)),
                Style::default().fg(theme::CYAN),
            ),
            Span::styled(
                "(grows to fill remaining)",
                Style::default().fg(theme::BRIGHT_BLACK),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("    home  ", Style::default().fg(theme::FG)),
            Span::styled("disabled ", Style::default().fg(theme::CYAN)),
            Span::styled(
                "(remaining left as free space)",
                Style::default().fg(theme::BRIGHT_BLACK),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// One adjustable row: a selection caret, the label, and the value.
fn field_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    let (caret, label_style) = if selected {
        (
            "> ",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default().fg(theme::FG))
    };
    Line::from(vec![
        Span::styled(caret, label_style),
        Span::styled(format!("{label:<6}"), label_style),
        Span::styled(value.to_string(), Style::default().fg(theme::CYAN)),
    ])
}

fn draw_gauge(frame: &mut Frame, area: Rect, committed: u64, available: u64) {
    let ratio = if available == 0 {
        1.0
    } else {
        (committed as f64 / available as f64).clamp(0.0, 1.0)
    };
    let over = committed > available;
    let color = if over { theme::RED } else { theme::GREEN };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color).bg(theme::BRIGHT_BLACK))
        .ratio(ratio)
        .label(format!(
            "{} of {} committed",
            human_bytes(committed),
            human_bytes(available)
        ));
    frame.render_widget(gauge, area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, available: u64) {
    let line = match app
        .config
        .target
        .as_ref()
        .map(|disk| layout::plan(&app.config.sizing, disk.size_bytes))
    {
        Some(Ok(_)) => {
            let remaining =
                human_bytes(available.saturating_sub(committed_bytes(&app.config.sizing)));
            let summary = if app.config.sizing.home.is_some() {
                format!("{remaining} free for home")
            } else {
                format!("{remaining} left as free space")
            };
            Line::from(Span::styled(summary, Style::default().fg(theme::GREEN)))
        }
        Some(Err(err)) => Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            "no target disk selected",
            Style::default().fg(theme::RED),
        )),
    };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn hint(swap_enabled: bool, home_enabled: bool) -> Line<'static> {
    let key = |label: &'static str, color| {
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let text = |label: &'static str| Span::styled(label, Style::default().fg(theme::FG));

    Line::from(vec![
        key("up/down", theme::BLUE),
        text(" field   "),
        key("left/right", theme::BLUE),
        text(" size   "),
        key("s", theme::YELLOW),
        text(if swap_enabled {
            " swap off   "
        } else {
            " swap on   "
        }),
        key("o", theme::YELLOW),
        text(if home_enabled {
            " home off   "
        } else {
            " home on   "
        }),
        key("Enter", theme::GREEN),
        text(" next   "),
        key("Esc", theme::RED),
        text(" back"),
    ])
}

/// Human-readable binary size, e.g. `16.0 GiB`. Mirrors
/// [`crate::disk::Disk::human_size`]; kept separate because it formats a bare
/// byte count rather than a disk. (Consider consolidating if a third caller
/// appears.)
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::disk::Disk;

    const DISK_512G: u64 = 512 * GIB;

    fn app_with_disk(size_bytes: u64) -> App {
        let mut app = App::new(true);
        app.config.target = Some(Disk {
            name: "/dev/sdb".into(),
            size_bytes,
            model: None,
        });
        app
    }

    #[test]
    fn default_sizing_is_valid_and_advances() {
        let app = app_with_disk(DISK_512G);
        assert!(is_valid(&app));
    }

    #[test]
    fn over_allocation_blocks_advancing() {
        let mut app = app_with_disk(DISK_512G);
        // Force root past the free space directly, bypassing the adjust clamp.
        app.config.sizing.root = SizeChoice::Fixed(DISK_512G);
        assert!(!is_valid(&app));
    }

    #[test]
    fn adjust_clamps_root_to_its_floor() {
        let mut app = app_with_disk(DISK_512G);
        app.sizing_cursor = 0; // root
        for _ in 0..1000 {
            adjust(&mut app, -(STEP_BYTES as i64));
        }
        assert_eq!(choice_bytes(app.config.sizing.root), layout::ROOT_MIN_BYTES);
        assert!(is_valid(&app));
    }

    #[test]
    fn adjust_cannot_over_commit_the_free_space() {
        let mut app = app_with_disk(DISK_512G);
        app.sizing_cursor = 0; // root
        for _ in 0..10_000 {
            adjust(&mut app, STEP_BYTES as i64);
        }
        assert!(
            is_valid(&app),
            "adjust must keep the sizing within free space"
        );
    }

    #[test]
    fn toggle_swap_round_trips() {
        let mut sizing = Sizing::default();
        let had_swap = sizing.swap.is_some();
        toggle_swap(&mut sizing);
        assert_eq!(sizing.swap.is_some(), !had_swap);
        toggle_swap(&mut sizing);
        assert_eq!(sizing.swap.is_some(), had_swap);
    }

    #[test]
    fn toggle_home_round_trips() {
        let mut sizing = Sizing::default();
        let had_home = sizing.home.is_some();
        toggle_home(&mut sizing);
        assert_eq!(sizing.home.is_some(), !had_home);
        toggle_home(&mut sizing);
        assert_eq!(sizing.home.is_some(), had_home);
    }

    #[test]
    fn sizing_is_valid_with_home_omitted() {
        let mut app = app_with_disk(DISK_512G);
        toggle_home(&mut app.config.sizing);
        assert!(app.config.sizing.home.is_none());
        assert!(is_valid(&app));
    }
}
