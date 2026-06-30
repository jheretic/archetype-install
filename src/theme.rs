//! afterglow 16-color palette and the chevron-ribbon wordmark.
//!
//! The hex values are a verbatim copy of the `afterglow)` row in
//! `archetype-logo.sh` / `vt-palette.sh`. They are duplicated here rather than
//! shared across the shell/Rust boundary; keep the two in sync by hand.
//!
//! The full 16-slot palette is defined even though Phase 2 renders only part of
//! it; later phases draw the rest.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub const BG: Color = Color::Rgb(0x1c, 0x14, 0x30);
pub const RED: Color = Color::Rgb(0xff, 0x6a, 0x5e);
pub const GREEN: Color = Color::Rgb(0x9e, 0xd1, 0x7b);
pub const YELLOW: Color = Color::Rgb(0xff, 0xaa, 0x52);
pub const BLUE: Color = Color::Rgb(0x7d, 0x8f, 0xd6);
pub const MAGENTA: Color = Color::Rgb(0xd5, 0x79, 0xc2);
pub const CYAN: Color = Color::Rgb(0x5f, 0xc6, 0xc4);
pub const FG: Color = Color::Rgb(0xf4, 0xe3, 0xea);

pub const BRIGHT_BLACK: Color = Color::Rgb(0x4f, 0x40, 0x68);
pub const BRIGHT_RED: Color = Color::Rgb(0xff, 0x8a, 0x7e);
pub const BRIGHT_GREEN: Color = Color::Rgb(0xbc, 0xe2, 0x9c);
pub const BRIGHT_YELLOW: Color = Color::Rgb(0xff, 0xc2, 0x79);
pub const BRIGHT_BLUE: Color = Color::Rgb(0x9f, 0xb0, 0xec);
pub const BRIGHT_MAGENTA: Color = Color::Rgb(0xec, 0x9b, 0xdb);
pub const BRIGHT_CYAN: Color = Color::Rgb(0x8a, 0xde, 0xdb);
pub const BRIGHT_FG: Color = Color::Rgb(0xfd, 0xf2, 0xf8);

/// Powerline right-pointing filled separator (U+E0B0).
const CHEVRON: &str = "\u{e0b0}";

/// The four chevron bands in warm->cool order, each knocking out one letter of
/// "arch" against the accent. Mirrors `archetype-logo.sh`'s band semantics.
const BANDS: [(char, Color); 4] = [('a', RED), ('r', YELLOW), ('c', GREEN), ('h', BLUE)];

/// The connected chevron ribbon wordmark: `>a>r>c>h>etype`.
///
/// Each band renders its letter knocked out (text in [`BG`]) on the accent, and
/// the joining [`CHEVRON`] takes the band's color as foreground over the next
/// band's color as background, producing seamless diagonal seams.
pub fn ribbon() -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(BANDS.len() * 2 + 2);

    // Indented left edge: a leading CHEVRON drawn as the knockout color (BG) on
    // the first band's color, so a dark wedge points RIGHT into band 0 -- the
    // same inward notch as the fish prompt's left cap (archetype-logo.sh).
    if let Some((_, first_accent)) = BANDS.first() {
        spans.push(Span::styled(
            CHEVRON,
            Style::default().fg(BG).bg(*first_accent),
        ));
    }

    for (index, (letter, accent)) in BANDS.iter().enumerate() {
        spans.push(Span::styled(
            format!(" {letter} "),
            Style::default()
                .fg(BG)
                .bg(*accent)
                .add_modifier(Modifier::BOLD),
        ));
        let next_bg = BANDS.get(index + 1).map(|(_, color)| *color);
        let mut seam = Style::default().fg(*accent);
        if let Some(color) = next_bg {
            seam = seam.bg(color);
        }
        spans.push(Span::styled(CHEVRON, seam));
    }

    spans.push(Span::styled(
        "etype",
        Style::default().fg(FG).add_modifier(Modifier::BOLD),
    ));

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ribbon_spells_archetype() {
        let text: String = ribbon()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            text,
            "\u{e0b0} a \u{e0b0} r \u{e0b0} c \u{e0b0} h \u{e0b0}etype"
        );
    }

    #[test]
    fn afterglow_bg_matches_source_hex() {
        assert_eq!(BG, Color::Rgb(0x1c, 0x14, 0x30));
        assert_eq!(FG, Color::Rgb(0xf4, 0xe3, 0xea));
    }
}
