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

/// The selectable fields, in display order. The two PIN fields are only
/// reachable/gated in PIN mode (see [`visible_fields`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Keymap,
    Locale,
    Timezone,
    Hostname,
    Chassis,
    Password,
    PasswordConfirm,
    TpmMode,
    TpmPin,
    TpmPinConfirm,
}

/// All fields in display order. The PIN entry/confirm are dropped from the
/// navigable set in automatic mode (see [`visible_fields`]).
const ALL_FIELDS: [Field; 10] = [
    Field::Keymap,
    Field::Locale,
    Field::Timezone,
    Field::Hostname,
    Field::Chassis,
    Field::Password,
    Field::PasswordConfirm,
    Field::TpmMode,
    Field::TpmPin,
    Field::TpmPinConfirm,
];

/// The fields the cursor can land on given the current TPM mode: in automatic
/// mode the two PIN fields are hidden (no PIN to collect).
fn visible_fields(app: &App) -> Vec<Field> {
    ALL_FIELDS
        .iter()
        .copied()
        .filter(|f| app.tpm_pin_mode || !matches!(f, Field::TpmPin | Field::TpmPinConfirm))
        .collect()
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    let fields = visible_fields(app);
    // Clamp the cursor: toggling out of PIN mode can shrink the visible set.
    app.firstboot_cursor = app.firstboot_cursor.min(fields.len() - 1);
    let field = fields[app.firstboot_cursor];
    match key.code {
        KeyCode::Esc => Transition::Back,
        KeyCode::Up => {
            app.firstboot_cursor = app.firstboot_cursor.saturating_sub(1);
            Transition::Stay
        }
        KeyCode::Down => {
            app.firstboot_cursor = (app.firstboot_cursor + 1).min(fields.len() - 1);
            Transition::Stay
        }
        // Tab / Shift-Tab cycle through the fields, WRAPPING (unlike Up/Down,
        // which clamp at the ends) -- the conventional form-navigation feel.
        KeyCode::Tab => {
            app.firstboot_cursor = (app.firstboot_cursor + 1) % fields.len();
            Transition::Stay
        }
        KeyCode::BackTab => {
            app.firstboot_cursor = (app.firstboot_cursor + fields.len() - 1) % fields.len();
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
        // TPM mode is a two-way toggle (PIN <-> automatic).
        KeyCode::Left | KeyCode::Right if field == Field::TpmMode => {
            app.tpm_pin_mode = !app.tpm_pin_mode;
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

/// The editable text buffer for a field, or `None` for the cycled/toggled
/// fields (chassis, TPM mode) which take no typed input.
fn field_buffer(app: &mut App, field: Field) -> Option<&mut String> {
    Some(match field {
        Field::Keymap => &mut app.config.firstboot.keymap,
        Field::Locale => &mut app.config.firstboot.locale,
        Field::Timezone => &mut app.config.firstboot.timezone,
        Field::Hostname => &mut app.config.firstboot.hostname,
        Field::Password => &mut app.password,
        Field::PasswordConfirm => &mut app.password_confirm,
        Field::TpmPin => &mut app.tpm_pin,
        Field::TpmPinConfirm => &mut app.tpm_pin_confirm,
        Field::Chassis | Field::TpmMode => return None,
    })
}

/// Whether the two password entries are non-empty and equal.
fn passwords_match(app: &App) -> bool {
    !app.password.is_empty() && app.password == app.password_confirm
}

/// Whether the TPM PIN entries are acceptable: in automatic mode there is no PIN
/// to check; in PIN mode the two entries must be non-empty and equal.
fn tpm_pin_ok(app: &App) -> bool {
    !app.tpm_pin_mode || (!app.tpm_pin.is_empty() && app.tpm_pin == app.tpm_pin_confirm)
}

/// Whether the screen may advance: all text fields valid, passwords matched,
/// and (in PIN mode) the PIN confirmed.
fn can_advance(app: &App) -> bool {
    app.config.firstboot.fields_complete() && passwords_match(app) && tpm_pin_ok(app)
}

/// Hash the confirmed password into the config, move the PIN (PIN mode only)
/// into the config, and clear all plaintext buffers. Only called once
/// [`can_advance`] holds.
fn commit(app: &mut App) {
    // TODO(phase2): collect username + GECOS on the screen. For now build a
    // placeholder UserConfig from the password buffers so the crate compiles.
    if let Ok(hash) = firstboot::hash_password(&app.password) {
        app.config.firstboot.user = Some(firstboot::UserConfig {
            username: String::new(),
            realname: String::new(),
            password_hash: hash,
            password_plain: app.password.clone(),
        });
    }
    app.config.firstboot.tpm_pin = if app.tpm_pin_mode {
        Some(app.tpm_pin.clone())
    } else {
        None
    };
    app.password.clear();
    app.password_confirm.clear();
    app.tpm_pin.clear();
    app.tpm_pin_confirm.clear();
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

    // PIN mode shows two extra rows (PIN + confirm); automatic mode shows a
    // one-line warning instead. Keep the layout height stable either way.
    let pin_rows = if app.tpm_pin_mode { 2 } else { 1 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),        // keymap
            Constraint::Length(1),        // locale
            Constraint::Length(1),        // timezone
            Constraint::Length(1),        // hostname
            Constraint::Length(1),        // chassis
            Constraint::Length(1),        // spacer
            Constraint::Length(1),        // password
            Constraint::Length(1),        // password confirm
            Constraint::Length(1),        // spacer
            Constraint::Length(1),        // tpm mode
            Constraint::Length(pin_rows), // pin+confirm (PIN) or warning (auto)
            Constraint::Min(1),           // status
            Constraint::Length(1),        // hint
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
    frame.render_widget(tpm_mode_field(app), rows[9]);
    if app.tpm_pin_mode {
        let pin_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(rows[10]);
        frame.render_widget(
            password_field(app, Field::TpmPin, "TPM PIN", &app.tpm_pin),
            pin_area[0],
        );
        frame.render_widget(
            password_field(
                app,
                Field::TpmPinConfirm,
                "confirm PIN",
                &app.tpm_pin_confirm,
            ),
            pin_area[1],
        );
    } else {
        frame.render_widget(tpm_auto_warning(), rows[10]);
    }
    frame.render_widget(Paragraph::new(status(app)), rows[11]);
    frame.render_widget(
        Paragraph::new(hint()).alignment(Alignment::Center),
        rows[12],
    );
}

/// A selectable text row: caret, label, typed value, and a cursor block on the
/// active field.
fn text_field(app: &App, field: Field, label: &str, value: &str) -> Paragraph<'static> {
    let selected = is_selected(app, field);
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
    let selected = is_selected(app, field);
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
    let selected = is_selected(app, Field::Chassis);
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

/// The TPM-mode row: a left/right toggle between PIN (default) and automatic.
fn tpm_mode_field(app: &App) -> Paragraph<'static> {
    let selected = is_selected(app, Field::TpmMode);
    let mut spans = label_spans("disk unlock", selected);
    let mode = if app.tpm_pin_mode {
        "TPM + PIN"
    } else {
        "TPM automatic (no PIN)"
    };
    spans.push(Span::styled(
        format!("\u{2039} {mode} \u{203a}"),
        Style::default().fg(theme::CYAN),
    ));
    Paragraph::new(Line::from(spans))
}

/// The warning shown in automatic mode: without a PIN, any bootable live medium
/// on this machine can TPM-unlock the root, so physical boot access must be
/// locked down.
fn tpm_auto_warning() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        "  warning: without a PIN, secure only if firmware boot order permits \
         ONLY the internal drive (no removable media) and a firmware password \
         prevents changing it -- else a live system can decrypt the root.",
        Style::default().fg(theme::YELLOW),
    )))
    .alignment(Alignment::Left)
}

/// Whether `field` is the one the cursor is currently on, accounting for the
/// mode-dependent visible set.
fn is_selected(app: &App, field: Field) -> bool {
    let fields = visible_fields(app);
    let cursor = app.firstboot_cursor.min(fields.len() - 1);
    fields[cursor] == field
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
    } else if app.tpm_pin_mode && app.tpm_pin.is_empty() {
        (
            "set a TPM unlock PIN (or switch disk unlock to automatic)",
            theme::YELLOW,
        )
    } else if app.tpm_pin_mode && app.tpm_pin != app.tpm_pin_confirm {
        ("TPM PINs do not match", theme::RED)
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
        key("up/down/tab", theme::BLUE),
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
        // Default mode is PIN, so a matching PIN is required to advance.
        app.tpm_pin = "12345678".to_string();
        app.tpm_pin_confirm = "12345678".to_string();
        app
    }

    #[test]
    fn defaults_plus_matching_passwords_can_advance() {
        let app = typed_app();
        assert!(can_advance(&app));
    }

    #[test]
    fn pin_mode_requires_matching_pin() {
        let mut app = typed_app();
        app.tpm_pin_confirm = "87654321".to_string();
        assert!(!can_advance(&app));
        app.tpm_pin.clear();
        app.tpm_pin_confirm.clear();
        assert!(!can_advance(&app)); // empty PIN in PIN mode blocks
    }

    #[test]
    fn automatic_mode_needs_no_pin() {
        let mut app = typed_app();
        app.tpm_pin_mode = false;
        app.tpm_pin.clear();
        app.tpm_pin_confirm.clear();
        assert!(can_advance(&app));
    }

    #[test]
    fn commit_pin_mode_stores_pin_automatic_clears_it() {
        let mut app = typed_app();
        commit(&mut app);
        assert_eq!(app.config.firstboot.tpm_pin.as_deref(), Some("12345678"));
        assert!(app.tpm_pin.is_empty() && app.tpm_pin_confirm.is_empty());

        let mut auto = typed_app();
        auto.tpm_pin_mode = false;
        commit(&mut auto);
        assert_eq!(auto.config.firstboot.tpm_pin, None);
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
        let user = app.config.firstboot.user.as_ref().unwrap();
        assert!(user.password_hash.starts_with("$6$"));
        assert!(app.password.is_empty());
        assert!(app.password_confirm.is_empty());
    }
}
