//! How the wallet says amounts, fees and times to a person: amounts
//! grouped and in sat, fees as a percentage, times as UTC dates. JSON
//! keeps the raw numbers.

/// Human output must not execute control sequences supplied by a peer.
/// Preserve the layout's newlines and tabs, and ordinary Unicode text.
pub fn terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The text tracing formatter writes UTF-8 chunks. Keep warnings visible
/// without allowing peer-supplied fields to execute terminal controls.
pub struct TerminalWriter<W>(pub W);

impl<W: std::io::Write> std::io::Write for TerminalWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.0.write_all(terminal(text).as_bytes())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod terminal_tests {
    #[test]
    fn peer_controls_are_literal_but_unicode_and_layout_survive() {
        assert_eq!(super::terminal("José\n\tpaid"), "José\n\tpaid");
        assert_eq!(
            super::terminal("\u{1b}[2J\r\u{7}\u{9b}"),
            "\\u{1b}[2J\\r\\u{7}\\u{9b}"
        );
    }

    #[test]
    fn diagnostic_text_is_escaped_without_suppressing_warnings() {
        use std::io::Write;
        let mut writer = super::TerminalWriter(Vec::new());
        writer
            .write_all("WARN José: \u{1b}[2J\u{9b}clear\n".as_bytes())
            .unwrap();
        writer.flush().unwrap();
        assert_eq!(
            String::from_utf8(writer.0).unwrap(),
            "WARN José: \\u{1b}[2J\\u{9b}clear\n"
        );
    }

    #[test]
    fn tracing_display_fields_pass_through_the_terminal_writer() {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = tempfile::tempfile().unwrap();
        let output = file.try_clone().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || super::TerminalWriter(output.try_clone().unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let reason = "José: \u{1b}[2J\u{9b}clear";
            tracing::warn!(%reason, "payment needs attention");
        });
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.contains("WARN"), "{text:?}");
        assert!(text.contains("payment needs attention"), "{text:?}");
        assert!(text.contains("José: \\u{1b}[2J\\u{9b}clear"), "{text:?}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{9b}'),
            "{text:?}"
        );
    }
}

/// `12,345`.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `12,345 sat`.
pub fn sat(amount: u64) -> String {
    format!("{} sat", grouped(amount))
}

/// An amount and its fee, with what the fee is of it: `10,000 + 12 sat
/// (0.12%)`.
pub fn plus(amount: u64, fee: u64) -> String {
    match share(fee, amount) {
        Some(share) => format!("{} + {} ({share})", grouped(amount), sat(fee)),
        None => format!("{} + {}", grouped(amount), sat(fee)),
    }
}

/// A cost and its share of the payment, without suggesting it was received.
pub fn fee_of(fee: u64, amount: u64) -> String {
    if fee == 0 {
        return "None".into();
    }
    match share(fee, amount) {
        Some(share) => format!("{} ({share})", sat(fee)),
        None => sat(fee),
    }
}

/// What `fee` is of `of`, to two significant digits: `0.24%`, `1.2%`,
/// `12%`, `<0.01%` below. Nothing for no fee, or a fee of nothing.
pub fn share(fee: u64, of: u64) -> Option<String> {
    if fee == 0 || of == 0 {
        return None;
    }
    let (fee, of) = (u128::from(fee), u128::from(of));
    let at = |scale: u128| u64::try_from((fee * scale + of / 2) / of).unwrap_or(u64::MAX);
    Some(match at(10_000) {
        0 => "<0.01%".into(),
        h if h < 100 => format!("0.{h:02}%"),
        _ => match at(1_000) {
            t if t < 100 && t % 10 > 0 => format!("{}.{}%", t / 10, t % 10),
            t if t < 100 => format!("{}%", t / 10),
            _ => format!("{}%", grouped(at(100))),
        },
    })
}

/// What a hub or provider takes: `1,000 to 4,000 sat`.
pub fn between(min: u64, max: u64) -> String {
    format!("{} to {}", grouped(min), sat(max))
}

/// A fee of `base` sat and `ppm` parts per million of what it moves:
/// `1 sat + 0.01%`, either part alone if the other is nothing.
pub fn fee(base: u64, ppm: u64) -> String {
    match (base, ppm) {
        (0, 0) => "free".into(),
        (base, 0) => sat(base),
        (0, ppm) => percent(ppm),
        (base, ppm) => format!("{} + {}", sat(base), percent(ppm)),
    }
}

/// What a hub charges a payment: [`fee`] on what it moves and, if it
/// charges for the notes made, that for each: `1 sat + 0.01% + 0.1 sat a
/// note`.
pub fn payment_fee(fees: &licash_api::Fees) -> String {
    let on_value = fee(fees.spend.base, fees.spend.ppm);
    let a_note = match (fees.output_msat / 1000, fees.output_msat % 1000) {
        (0, 0) => return on_value,
        (whole, 0) => format!("{} a note", sat(whole)),
        (whole, rest) => format!(
            "{}.{} sat a note",
            grouped(whole),
            format!("{rest:03}").trim_end_matches('0')
        ),
    };
    match (fees.spend.base, fees.spend.ppm) {
        (0, 0) => a_note,
        _ => format!("{on_value} + {a_note}"),
    }
}

/// Parts per million as a percentage: `0.01%`, `1%`, `2.5%`.
pub fn percent(ppm: u64) -> String {
    let (whole, rest) = (ppm / 10_000, ppm % 10_000);
    match rest {
        0 => format!("{}%", grouped(whole)),
        _ => format!(
            "{}.{}%",
            grouped(whole),
            format!("{rest:04}").trim_end_matches('0')
        ),
    }
}

/// Unix seconds as a UTC date and time: `2026-11-15 13:00`.
pub fn when(at: i64) -> String {
    let (days, secs) = (at.div_euclid(86_400), at.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// The year, month and day `days` after 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_grouped_and_in_sat() {
        assert_eq!(sat(0), "0 sat");
        assert_eq!(sat(999), "999 sat");
        assert_eq!(sat(1_000), "1,000 sat");
        assert_eq!(sat(1_234_567), "1,234,567 sat");
        assert_eq!(plus(10_000, 12), "10,000 + 12 sat (0.12%)");
        assert_eq!(plus(22, 0), "22 + 0 sat");
        assert_eq!(between(1_000, 4_000), "1,000 to 4,000 sat");
    }

    #[test]
    fn a_fee_reads_as_a_share_of_what_it_is_on() {
        assert_eq!(fee_of(0, 42), "None");
        assert_eq!(fee_of(1, 42), "1 sat (2.4%)");
        assert_eq!(fee_of(5, 0), "5 sat");
        assert_eq!(share(0, 100), None);
        assert_eq!(share(5, 0), None);
        assert_eq!(share(1, 1_000_000), Some("<0.01%".into()));
        assert_eq!(share(24, 10_000), Some("0.24%".into()));
        assert_eq!(share(1, 100), Some("1%".into()));
        assert_eq!(share(1, 20), Some("5%".into()));
        assert_eq!(share(12, 1_000), Some("1.2%".into()));
        assert_eq!(share(999, 10_000), Some("10%".into()));
        assert_eq!(share(1, 8), Some("13%".into()));
        assert_eq!(share(30, 1), Some("3,000%".into()));
    }

    #[test]
    fn fees_are_sat_and_a_percentage() {
        assert_eq!(fee(0, 0), "free");
        assert_eq!(fee(1, 0), "1 sat");
        assert_eq!(fee(0, 100), "0.01%");
        assert_eq!(fee(1, 10_000), "1 sat + 1%");
        let fees = |base, ppm, output_msat| licash_api::Fees {
            spend: licash_api::Fee { base, ppm },
            output_msat,
            ..Default::default()
        };
        assert_eq!(payment_fee(&fees(0, 0, 0)), "free");
        assert_eq!(payment_fee(&fees(1, 100, 0)), "1 sat + 0.01%");
        assert_eq!(
            payment_fee(&fees(1, 100, 100)),
            "1 sat + 0.01% + 0.1 sat a note"
        );
        assert_eq!(payment_fee(&fees(0, 0, 2_000)), "2 sat a note");
        assert_eq!(payment_fee(&fees(0, 0, 1_250)), "1.25 sat a note");
        assert_eq!(percent(25_000), "2.5%");
        assert_eq!(percent(1), "0.0001%");
    }

    #[test]
    fn times_are_utc_dates() {
        assert_eq!(when(0), "1970-01-01 00:00");
        // 2026-11-15T13:05:00Z
        assert_eq!(when(1_794_700_800 + 13 * 3600 + 300), "2026-11-15 13:05");
        assert_eq!(when(951_782_400), "2000-02-29 00:00");
        assert_eq!(when(-1), "1969-12-31 23:59");
    }
}
