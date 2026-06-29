//! System configuration: collect the first-boot fields (keymap, locale,
//! timezone, hostname, chassis) and the root password.
//!
//! A cursor selects one field. Text fields take typed input; the chassis field
//! cycles left/right within {desktop, laptop, server}; the two password fields
//! are masked. Advancing is gated on every text field being filled, the
//! hostname being valid, and the two passwords being non-empty and equal. On
//! advance the password is hashed into the config and the typed buffers are
//! cleared, so plaintext never outlives this screen.
//!
//! Like the Confirm screen, `q` is NOT a quit key here -- it is a literal
//! character the user may type. Esc goes back.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Transition};
use crate::firstboot;
use crate::theme;

/// The selectable fields, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Keymap,
    Locale,
    Timezone,
    Hostname,
    Chassis,
    Password,
    PasswordConfirm,
}

const FIELDS: [Field; 7] = [
    Field::Keymap,
    Field::Locale,
    Field::Timezone,
    Field::Hostname,
    Field::Chassis,
    Field::Password,
    Field::PasswordConfirm,
];

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    let field = FIELDS[app.firstboot_cursor];
    match key.code {
        KeyCode::Esc => Transition::Back,
        KeyCode::Up => {
            app.firstboot_cursor = app.firstboot_cursor.saturating_sub(1);
            Transition::Stay
        }
        KeyCode::Down => {
            app.firstboot_cursor = (app.firstboot_cursor + 1).min(FIELDS.len() - 1);
            Transition::Stay
        }
        KeyCode::Left if field == Field::Chassis => {
            app.config.firstboot.chassis = app.config.firstboot.chassis.prev();
            Transition::Stay
        }
        KeyCode::Right if field == Field::Chassis => {
            app.config.firstboot.chassis = app.config.firstboot.chassis.next();
            Transition::Stay
        }
        KeyCode::Backspace => {
            field_buffer(app, field).map(String::pop);
            Transition::Stay
        }
        KeyCode::Char(c) => {
            if let Some(buffer) = field_buffer(app, field) {
                buffer.push(c);
            }
            Transition::Stay
        }
        KeyCode::Enter if can_advance(app) => {
            commit(app);
            Transition::Next
        }
        _ => Transition::Stay,
    }
}

/// The editable text buffer for a field, or `None` for the chassis field (which
/// is cycled, not typed).
fn field_buffer(app: &mut App, field: Field) -> Option<&mut String> {
    Some(match field {
        Field::Keymap => &mut app.config.firstboot.keymap,
        Field::Locale => &mut app.config.firstboot.locale,
        Field::Timezone => &mut app.config.firstboot.timezone,
        Field::Hostname => &mut app.config.firstboot.hostname,
        Field::Password => &mut app.password,
        Field::PasswordConfirm => &mut app.password_confirm,
        Field::Chassis => return None,
    })
}

/// Whether the two password entries are non-empty and equal.
fn passwords_match(app: &App) -> bool {
    !app.password.is_empty() && app.password == app.password_confirm
}

/// Whether the screen may advance: all text fields valid and passwords matched.
fn can_advance(app: &App) -> bool {
    app.config.firstboot.fields_complete() && passwords_match(app)
}

/// Hash the confirmed password into the config and clear the plaintext buffers.
/// Only called once [`can_advance`] holds.
fn commit(app: &mut App) {
    if let Ok(hash) = firstboot::hash_root_password(&app.password) {
        app.config.firstboot.root_password_hash = Some(hash);
    }
    app.password.clear();
    app.password_confirm.clear();
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            " system configuration ",
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
            Constraint::Length(1), // keymap
            Constraint::Length(1), // locale
            Constraint::Length(1), // timezone
            Constraint::Length(1), // hostname
            Constraint::Length(1), // chassis
            Constraint::Length(1), // spacer
            Constraint::Length(1), // password
            Constraint::Length(1), // password confirm
            Constraint::Min(1),    // status
            Constraint::Length(1), // hint
        ])
        .split(inner);

    let fb = &app.config.firstboot;
    frame.render_widget(
        text_field(app, Field::Keymap, "keymap", &fb.keymap),
        rows[0],
    );
    frame.render_widget(
        text_field(app, Field::Locale, "locale", &fb.locale),
        rows[1],
    );
    frame.render_widget(
        text_field(app, Field::Timezone, "timezone", &fb.timezone),
        rows[2],
    );
    frame.render_widget(
        text_field(app, Field::Hostname, "hostname", &fb.hostname),
        rows[3],
    );
    frame.render_widget(chassis_field(app), rows[4]);
    frame.render_widget(
        password_field(app, Field::Password, "root password", &app.password),
        rows[6],
    );
    frame.render_widget(
        password_field(
            app,
            Field::PasswordConfirm,
            "confirm",
            &app.password_confirm,
        ),
        rows[7],
    );
    frame.render_widget(Paragraph::new(status(app)), rows[8]);
    frame.render_widget(Paragraph::new(hint()).alignment(Alignment::Center), rows[9]);
}

/// A selectable text row: caret, label, typed value, and a cursor block on the
/// active field.
fn text_field(app: &App, field: Field, label: &str, value: &str) -> Paragraph<'static> {
    let selected = FIELDS[app.firstboot_cursor] == field;
    let mut spans = label_spans(label, selected);
    spans.push(Span::styled(
        value.to_string(),
        Style::default().fg(theme::CYAN),
    ));
    if selected {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme::BRIGHT_BLACK),
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// A masked password row: the value renders as dots.
fn password_field(app: &App, field: Field, label: &str, value: &str) -> Paragraph<'static> {
    let selected = FIELDS[app.firstboot_cursor] == field;
    let mut spans = label_spans(label, selected);
    spans.push(Span::styled(
        "\u{2022}".repeat(value.chars().count()),
        Style::default().fg(theme::CYAN),
    ));
    if selected {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme::BRIGHT_BLACK),
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// The chassis row: a left/right-cycled choice within {desktop, laptop, server}.
fn chassis_field(app: &App) -> Paragraph<'static> {
    let selected = FIELDS[app.firstboot_cursor] == Field::Chassis;
    let mut spans = label_spans("chassis", selected);
    spans.push(Span::styled(
        format!(
            "\u{2039} {} \u{203a}",
            app.config.firstboot.chassis.as_str()
        ),
        Style::default().fg(theme::CYAN),
    ));
    Paragraph::new(Line::from(spans))
}

/// The caret + fixed-width label shared by every row.
fn label_spans(label: &str, selected: bool) -> Vec<Span<'static>> {
    let (caret, style) = if selected {
        (
            "> ",
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default().fg(theme::FG))
    };
    vec![
        Span::styled(caret, style),
        Span::styled(format!("{label:<14}"), style),
    ]
}

/// The gating status line: what still blocks advancing, or a ready message.
fn status(app: &App) -> Line<'static> {
    let (text, color) = if !firstboot::valid_hostname(&app.config.firstboot.hostname) {
        ("hostname must be 1-63 DNS chars (a-z, 0-9, -)", theme::RED)
    } else if !app.config.firstboot.fields_complete() {
        ("keymap, locale, and timezone must not be empty", theme::RED)
    } else if app.password.is_empty() {
        ("set a root password", theme::YELLOW)
    } else if app.password != app.password_confirm {
        ("passwords do not match", theme::RED)
    } else {
        ("configuration complete", theme::GREEN)
    };
    Line::from(Span::styled(
        text,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn hint() -> Line<'static> {
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
        text(" chassis   "),
        key("Enter", theme::GREEN),
        text(" next   "),
        key("Esc", theme::RED),
        text(" back"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    fn typed_app() -> App {
        let mut app = App::new(true);
        app.password = "hunter2hunter2".to_string();
        app.password_confirm = "hunter2hunter2".to_string();
        app
    }

    #[test]
    fn defaults_plus_matching_passwords_can_advance() {
        let app = typed_app();
        assert!(can_advance(&app));
    }

    #[test]
    fn mismatched_passwords_block_advancing() {
        let mut app = typed_app();
        app.password_confirm = "different".to_string();
        assert!(!can_advance(&app));
    }

    #[test]
    fn empty_password_blocks_advancing() {
        let mut app = typed_app();
        app.password.clear();
        app.password_confirm.clear();
        assert!(!can_advance(&app));
    }

    #[test]
    fn empty_required_field_blocks_advancing() {
        let mut app = typed_app();
        app.config.firstboot.timezone.clear();
        assert!(!can_advance(&app));
    }

    #[test]
    fn invalid_hostname_blocks_advancing() {
        let mut app = typed_app();
        app.config.firstboot.hostname = "bad host".to_string();
        assert!(!can_advance(&app));
    }

    #[test]
    fn commit_hashes_and_clears_plaintext() {
        let mut app = typed_app();
        assert!(can_advance(&app));
        commit(&mut app);
        let hash = app.config.firstboot.root_password_hash.as_ref().unwrap();
        assert!(hash.starts_with("$6$"));
        assert!(app.password.is_empty());
        assert!(app.password_confirm.is_empty());
    }
}
