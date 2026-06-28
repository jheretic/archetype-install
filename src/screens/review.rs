//! Review: show the exact `repart.d` text that would be written, and (when
//! available) the plan `systemd-repart --dry-run` computes from it.
//!
//! The generated text is always shown; the repart dry-run is best-effort and a
//! missing or failing `systemd-repart` degrades to a notice rather than an
//! error. The rendered bytes here are the same ones [`generate`] writes to
//! [`crate::repart::generate::OUTPUT_DIR`].

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{is_quit, App, Transition};
use crate::layout::Sizing;
use crate::repart::generate;
use crate::repart::runner;
use crate::theme;

/// Precomputed Review content for the current disk + sizing: the generated
/// definition text and, optionally, repart's own dry-run plan.
pub struct ReviewState {
    /// The generated `.conf` set, joined for display.
    pub text: String,
    /// The output dir the set was written to, or an error string.
    pub output: Result<PathBuf, String>,
    /// repart's dry-run plan, or a notice why it is unavailable.
    pub repart_plan: RepartPlan,
    /// Top line of the viewport.
    pub scroll: u16,
}

/// The optional `systemd-repart --dry-run` result.
pub enum RepartPlan {
    /// repart ran and printed a plan.
    Plan(String),
    /// repart ran but failed; carries a short reason.
    Failed(String),
    /// repart could not be started (e.g. not installed).
    Unavailable(String),
}

impl ReviewState {
    /// Generate the definition set for `sizing` on `disk_bytes`, write it, then
    /// attempt a guarded repart dry-run against `device`.
    pub fn build(sizing: &Sizing, disk_bytes: u64, device: &str) -> Self {
        match generate::generate(sizing, disk_bytes) {
            Ok((dir, files)) => {
                let text = files
                    .iter()
                    .map(|file| format!("# {}\n{}", file.filename, file.contents))
                    .collect::<Vec<_>>()
                    .join("\n");
                let repart_plan = run_repart(&dir, device);
                Self {
                    text,
                    output: Ok(dir),
                    repart_plan,
                    scroll: 0,
                }
            }
            Err(err) => Self {
                text: format!("failed to generate repart.d definitions:\n{err}"),
                output: Err(err.to_string()),
                repart_plan: RepartPlan::Unavailable("definitions not generated".to_string()),
                scroll: 0,
            },
        }
    }
}

/// Run the guarded dry-run, mapping each failure mode onto a [`RepartPlan`].
fn run_repart(dir: &std::path::Path, device: &str) -> RepartPlan {
    match runner::dry_run(dir, device) {
        Ok(outcome) if outcome.success() => RepartPlan::Plan(outcome.stdout),
        Ok(outcome) => RepartPlan::Failed(format!(
            "systemd-repart exited {}: {}",
            outcome
                .exit_code
                .map_or_else(|| "by signal".to_string(), |code| code.to_string()),
            outcome.stderr.trim()
        )),
        Err(err) => RepartPlan::Unavailable(err.to_string()),
    }
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Transition {
    if is_quit(key) {
        return Transition::Quit;
    }
    match key.code {
        KeyCode::Esc => Transition::Back,
        KeyCode::Up | KeyCode::Char('k') => {
            scroll(app, -1);
            Transition::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            scroll(app, 1);
            Transition::Stay
        }
        KeyCode::Enter => Transition::Next,
        _ => Transition::Stay,
    }
}

fn scroll(app: &mut App, delta: i16) {
    if let Some(review) = app.review.as_mut() {
        review.scroll = review.scroll.saturating_add_signed(delta);
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let dry_run = app.dry_run;
    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .style(Style::default().bg(theme::BG).fg(theme::FG))
        .title(Span::styled(
            if dry_run {
                " review (dry-run \u{2014} no changes will be made) "
            } else {
                " review generated partition layout "
            },
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
            Constraint::Min(1),    // generated text + repart plan
            Constraint::Length(1), // hint
        ])
        .split(inner);

    match app.review.as_ref() {
        Some(review) => draw_body(frame, rows[0], review),
        None => frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no layout to review",
                Style::default().fg(theme::RED),
            ))),
            rows[0],
        ),
    }

    frame.render_widget(
        Paragraph::new(hint(dry_run)).alignment(Alignment::Center),
        rows[1],
    );
}

fn draw_body(frame: &mut Frame, area: Rect, review: &ReviewState) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let defs = Paragraph::new(review.text.clone())
        .block(panel(" repart.d definitions "))
        .scroll((review.scroll, 0));
    frame.render_widget(defs, columns[0]);

    let (plan_title, plan_text, plan_color) = match &review.repart_plan {
        RepartPlan::Plan(text) => (" repart dry-run plan ", text.clone(), theme::FG),
        RepartPlan::Failed(reason) => (" repart dry-run (failed) ", reason.clone(), theme::YELLOW),
        RepartPlan::Unavailable(reason) => (
            " repart dry-run (unavailable) ",
            reason.clone(),
            theme::BRIGHT_BLACK,
        ),
    };
    let plan = Paragraph::new(Span::styled(plan_text, Style::default().fg(plan_color)))
        .block(panel(plan_title))
        .scroll((review.scroll, 0));
    frame.render_widget(plan, columns[1]);
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BRIGHT_BLACK))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(theme::CYAN),
        ))
}

fn hint(dry_run: bool) -> Line<'static> {
    let key = |label: &'static str, color| {
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let text = |label: &'static str| Span::styled(label, Style::default().fg(theme::FG));

    Line::from(vec![
        key("up/down", theme::BLUE),
        text(" scroll   "),
        key("Enter", theme::GREEN),
        text(if dry_run {
            " finish   "
        } else {
            " continue   "
        }),
        key("Esc", theme::RED),
        text(" back"),
    ])
}
