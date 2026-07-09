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

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;
/// How much one left/right keypress moves the selected size.
const STEP_BYTES: u64 = GIB;
/// Smallest swap we keep while it is enabled; below this, `s` disables it.
const SWAP_MIN_BYTES: u64 = GIB;

/// Parse a human size string into bytes. A bare number defaults to GiB; a
/// unit suffix (case-insensitive, optional trailing "iB"/"B") overrides:
/// `k/m/g/t` -> KiB/MiB/GiB/TiB. Accepts a decimal fraction (e.g. "1.5T").
/// Returns `None` on empty, non-numeric, negative, or unknown-unit input. Pure.
fn parse_size(input: &str) -> Option<u64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // Split the trailing alphabetic unit from the leading number.
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    // Normalise the unit: strip an optional "iB"/"B" tail, take the first char.
    let unit = unit.trim().to_ascii_lowercase();
    let mult = match unit.chars().next() {
        None => GIB, // bare number -> GiB
        Some('k') => KIB,
        Some('m') => MIB,
        Some('g') => GIB,
        Some('t') => TIB,
        Some('b') if unit == "b" => 1,
        _ => return None,
    };
    // Guard the float->int multiply against overflow/NaN.
    let bytes = value * mult as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

/// The adjustable fields, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Root,
    Swap,
}

const FIELDS: [Field; 2] = [Field::Root, Field::Swap];

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    // While editing an exact value, keys go to the edit buffer, not navigation.
    if app.sizing_edit.is_some() {
        return handle_edit_key(app, key);
    }
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
        // A digit begins exact-value entry on the selected editable field (root,
        // or swap when enabled). The typed digit seeds the buffer.
        KeyCode::Char(c) if c.is_ascii_digit() && editable_selected(app) => {
            app.sizing_edit = Some(c.to_string());
            Transition::Stay
        }
        KeyCode::Enter if is_valid(app) => Transition::Next,
        _ => Transition::Stay,
    }
}

/// Whether the selected field accepts an exact value: root always, swap only
/// when enabled (a disabled swap has nothing to edit).
fn editable_selected(app: &App) -> bool {
    match FIELDS[app.sizing_cursor] {
        Field::Root => true,
        Field::Swap => app.config.sizing.swap.is_some(),
    }
}

/// Key handling while the exact-value edit buffer is active. Digits, one
/// decimal point, and a unit letter (k/m/g/t/b) are accepted; Backspace edits;
/// Enter commits the parsed value (invalid input keeps editing); Esc cancels.
fn handle_edit_key(app: &mut App, key: KeyEvent) -> Transition {
    match key.code {
        KeyCode::Esc => {
            app.sizing_edit = None;
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.sizing_edit.as_mut() {
                buf.pop();
            }
        }
        KeyCode::Enter => {
            let parsed = app.sizing_edit.as_deref().and_then(parse_size);
            if let Some(bytes) = parsed {
                set_selected(app, bytes);
                app.sizing_edit = None;
            }
            // Unparseable -> stay in edit mode so the user can fix it.
        }
        KeyCode::Char(c)
            if c.is_ascii_digit()
                || c == '.'
                || matches!(c.to_ascii_lowercase(), 'k' | 'm' | 'g' | 't' | 'b' | 'i') =>
        {
            if let Some(buf) = app.sizing_edit.as_mut() {
                buf.push(c);
            }
        }
        _ => {}
    }
    Transition::Stay
}

/// Set the selected field to an exact byte value, clamped to its floor and the
/// free space left for the other committed partitions (same bounds as [`adjust`]).
fn set_selected(app: &mut App, bytes: u64) {
    let available = available_bytes(app);
    let sizing = &mut app.config.sizing;
    let field = FIELDS[app.sizing_cursor];
    let (floor, current) = match field {
        Field::Root => (layout::ROOT_MIN_BYTES, choice_bytes(sizing.root)),
        Field::Swap => match sizing.swap {
            Some(choice) => (SWAP_MIN_BYTES, choice_bytes(choice)),
            None => return,
        },
    };
    let others = committed_bytes(sizing) - current;
    let ceiling = available.saturating_sub(others);
    let next = bytes.clamp(floor, ceiling.max(floor));
    match field {
        Field::Root => sizing.root = SizeChoice::Fixed(next),
        Field::Swap => sizing.swap = Some(SizeChoice::Fixed(next)),
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

    let hint_line = if app.sizing_edit.is_some() {
        edit_hint()
    } else {
        hint(sizing.swap.is_some(), sizing.home.is_some())
    };
    frame.render_widget(
        Paragraph::new(hint_line).alignment(Alignment::Center),
        rows[6],
    );
}

fn draw_field(frame: &mut Frame, area: Rect, label: &str, app: &App, field: Field, bytes: u64) {
    let selected = FIELDS[app.sizing_cursor] == field;
    let value = editing_value(app, field).unwrap_or_else(|| human_bytes(bytes));
    frame.render_widget(Paragraph::new(field_line(label, &value, selected)), area);
}

fn draw_swap(frame: &mut Frame, area: Rect, app: &App) {
    let selected = FIELDS[app.sizing_cursor] == Field::Swap;
    let value = editing_value(app, Field::Swap).unwrap_or_else(|| match app.config.sizing.swap {
        Some(choice) => human_bytes(choice_bytes(choice)),
        None => "disabled".to_string(),
    });
    frame.render_widget(Paragraph::new(field_line("swap", &value, selected)), area);
}

/// When `field` is the one currently being edited, the buffer text plus a block
/// cursor (so the row shows live typing); otherwise `None`.
fn editing_value(app: &App, field: Field) -> Option<String> {
    if FIELDS[app.sizing_cursor] == field {
        app.sizing_edit.as_ref().map(|buf| format!("{buf}\u{2588}"))
    } else {
        None
    }
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

/// The hint shown while typing an exact value.
fn edit_hint() -> Line<'static> {
    let key = |label: &'static str, color| {
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let text = |label: &'static str| Span::styled(label, Style::default().fg(theme::FG));
    Line::from(vec![
        text("type a size (e.g. 40G, 512M, 1.5T)   "),
        key("Enter", theme::GREEN),
        text(" set   "),
        key("Esc", theme::RED),
        text(" cancel"),
    ])
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
        key("0-9", theme::BLUE),
        text(" type exact   "),
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

    #[test]
    fn parse_size_bare_number_defaults_to_gib() {
        assert_eq!(parse_size("40"), Some(40 * GIB));
        assert_eq!(parse_size("  8 "), Some(8 * GIB));
    }

    #[test]
    fn parse_size_accepts_unit_suffixes_case_insensitive() {
        assert_eq!(parse_size("512M"), Some(512 * MIB));
        assert_eq!(parse_size("512mib"), Some(512 * MIB));
        assert_eq!(parse_size("2G"), Some(2 * GIB));
        assert_eq!(parse_size("1T"), Some(TIB));
        assert_eq!(parse_size("1024k"), Some(1024 * KIB));
        assert_eq!(parse_size("1048576b"), Some(MIB));
    }

    #[test]
    fn parse_size_accepts_decimal_fraction() {
        assert_eq!(parse_size("1.5T"), Some(TIB + TIB / 2));
        assert_eq!(parse_size("0.5G"), Some(GIB / 2));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("-5G"), None);
        assert_eq!(parse_size("5X"), None);
        assert_eq!(parse_size("G"), None);
    }

    #[test]
    fn set_selected_clamps_to_floor_and_ceiling() {
        let mut app = app_with_disk(DISK_512G);
        app.sizing_cursor = 0; // root
                               // Below floor -> floored.
        set_selected(&mut app, 1);
        assert_eq!(choice_bytes(app.config.sizing.root), layout::ROOT_MIN_BYTES);
        // Absurdly large -> clamped within free space, still valid.
        set_selected(&mut app, 10_000 * GIB);
        assert!(is_valid(&app));
        // A sane exact value lands verbatim.
        set_selected(&mut app, 40 * GIB);
        assert_eq!(choice_bytes(app.config.sizing.root), 40 * GIB);
    }
}
