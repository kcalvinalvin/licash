//! The dashboard's palette: one accent, quiet borders, and color only
//! where it says something.

use ratatui::style::{Color, Modifier, Style};

use crate::page::{Better, Tone};

pub const ACCENT: Color = Color::Rgb(247, 147, 26);
pub const BORDER: Color = Color::Rgb(62, 66, 82);
pub const FOCUS: Color = Color::Rgb(247, 147, 26);
pub const GOOD: Color = Color::Rgb(74, 222, 128);
pub const WARN: Color = Color::Rgb(250, 204, 21);
pub const BAD: Color = Color::Rgb(248, 113, 113);
pub const MUTED: Color = Color::Rgb(124, 128, 146);

pub const DIM: Style = Style::new().fg(MUTED);
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// Labels' colors, in the order they first appeared.
const SERIES: [Color; 8] = [
    Color::Rgb(247, 147, 26),
    Color::Rgb(56, 189, 193),
    Color::Rgb(167, 139, 250),
    Color::Rgb(96, 165, 250),
    Color::Rgb(244, 114, 182),
    Color::Rgb(163, 230, 53),
    Color::Rgb(251, 191, 36),
    Color::Rgb(148, 163, 184),
];

pub fn color(label: usize) -> Color {
    SERIES[label % SERIES.len()]
}

pub fn tone(tone: Tone) -> Style {
    match tone {
        Tone::Normal => Style::new(),
        Tone::Good => Style::new().fg(GOOD),
        Tone::Warn => Style::new().fg(WARN),
        Tone::Bad => Style::new().fg(BAD),
        Tone::Muted => DIM,
    }
}

/// A tone's glyph, so that a state never reads by its color alone.
pub fn glyph(tone: Tone) -> &'static str {
    match tone {
        Tone::Good => "✓",
        Tone::Warn => "!",
        Tone::Bad => "✗",
        Tone::Muted => "–",
        Tone::Normal => "·",
    }
}

/// How a change of `delta` reads for a value better moving `better`.
pub fn trend(delta: f64, better: Better) -> Style {
    match (better, delta.partial_cmp(&0.0)) {
        (_, None | Some(std::cmp::Ordering::Equal)) | (Better::Neither, _) => DIM,
        (Better::Up, Some(std::cmp::Ordering::Greater))
        | (Better::Down, Some(std::cmp::Ordering::Less)) => Style::new().fg(GOOD),
        _ => Style::new().fg(BAD),
    }
}
