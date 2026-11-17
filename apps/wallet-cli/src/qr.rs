//! QR codes drawn on a terminal, for a payer's phone to scan off it.

use qrcode::{Color, EcLevel, QrCode};

/// Light modules around the code, as its specification asks.
const QUIET: usize = 4;
/// Black on bright white, whatever the terminal's own colors: a code drawn
/// light on dark is not read by every scanner.
const PAINT: &str = "\x1b[30;107m";
const RESET: &str = "\x1b[0m";

/// `text` as a QR code of half blocks, two rows of modules a line: square
/// in the usual terminal font, small enough for a long payment code.
pub fn draw(text: &str) -> anyhow::Result<String> {
    // The lowest correction holds the most: a screen does not smudge.
    let code = QrCode::with_error_correction_level(text, EcLevel::L)
        .map_err(|e| anyhow::anyhow!("no QR code: {e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let side = width + 2 * QUIET;
    let dark = |x: usize, y: usize| {
        let (Some(x), Some(y)) = (x.checked_sub(QUIET), y.checked_sub(QUIET)) else {
            return false;
        };
        x < width && y < width && colors[y * width + x] == Color::Dark
    };
    let mut out = String::new();
    for y in (0..side).step_by(2) {
        out.push_str(PAINT);
        for x in 0..side {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str(RESET);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a phone reads off `drawing`, each module 4 pixels square.
    fn scan(drawing: &str) -> Vec<String> {
        const SCALE: usize = 4;
        let rows: Vec<Vec<bool>> = drawing
            .lines()
            .map(|line| line.trim_start_matches(PAINT).trim_end_matches(RESET))
            .flat_map(|line| {
                let half = |upper: bool| {
                    line.chars()
                        .map(|c| match c {
                            '█' => true,
                            '▀' => upper,
                            '▄' => !upper,
                            ' ' => false,
                            other => panic!("{other:?} in a drawing"),
                        })
                        .collect::<Vec<_>>()
                };
                [half(true), half(false)]
            })
            .collect();
        let (width, height) = (rows[0].len() * SCALE, rows.len() * SCALE);
        let mut image =
            rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
                match rows[y / SCALE][x / SCALE] {
                    true => 0,
                    false => 255,
                }
            });
        image
            .detect_grids()
            .into_iter()
            .map(|grid| grid.decode().unwrap().1)
            .collect()
    }

    #[test]
    fn a_phone_reads_back_what_was_drawn() {
        for text in [
            "BITCOIN:BCRT1P",
            // As long as a payment code naming two hubs, a mailbox and a
            // silent payment address.
            &format!(
                "BITCOIN:BCRT1P{}?SP=TSP1Q{}",
                "Q".repeat(120),
                "A".repeat(200)
            ),
            "lnbcrt10u1pjexample",
        ] {
            assert_eq!(scan(&draw(text).unwrap()), [text]);
        }
    }

    #[test]
    fn every_line_paints_its_own_colors_and_resets_them() {
        let drawing = draw("x").unwrap();
        let lines: Vec<_> = drawing.lines().collect();
        // 21 modules and the quiet zone on both sides, two rows a line.
        assert_eq!(lines.len(), (21 + 2 * QUIET).div_ceil(2));
        for line in lines {
            assert!(line.starts_with(PAINT) && line.ends_with(RESET), "{line:?}");
            let body = line.trim_start_matches(PAINT).trim_end_matches(RESET);
            assert_eq!(body.chars().count(), 21 + 2 * QUIET);
        }
    }

    #[test]
    fn too_much_for_a_code_is_an_error() {
        assert!(draw(&"x".repeat(3000)).is_err());
    }
}
