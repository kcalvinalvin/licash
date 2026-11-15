//! How the wallet says amounts, fees and times to a person: amounts
//! grouped and in sat, fees as a percentage, times as UTC dates. JSON
//! keeps the raw numbers.

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

/// An amount and its fee: `10,000 + 12 sat`.
pub fn plus(amount: u64, fee: u64) -> String {
    format!("{} + {}", grouped(amount), sat(fee))
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
        assert_eq!(plus(10_000, 12), "10,000 + 12 sat");
        assert_eq!(between(1_000, 4_000), "1,000 to 4,000 sat");
    }

    #[test]
    fn fees_are_sat_and_a_percentage() {
        assert_eq!(fee(0, 0), "free");
        assert_eq!(fee(1, 0), "1 sat");
        assert_eq!(fee(0, 100), "0.01%");
        assert_eq!(fee(1, 10_000), "1 sat + 1%");
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
