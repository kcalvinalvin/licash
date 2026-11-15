//! How values read, alike in every panel.

use crate::series::Unit;

/// `value` with commas between thousands: `-12,345`.
pub fn grouped(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `value` in `unit`, in full: `12,345 sat`.
pub fn amount(value: i64, unit: Unit) -> String {
    match unit {
        Unit::Sat => format!("{} sat", grouped(value)),
        Unit::Count => grouped(value),
        Unit::Blocks => format!("{} blocks", grouped(value)),
    }
}

/// `value` in `unit`, short enough for an axis or a narrow card: `12.3M sat`.
pub fn compact(value: f64, unit: Unit) -> String {
    scaled(value, unit, 1e4)
}

/// `value` in `unit` shorter still, for a narrow column: `5.7k sat`.
pub fn shortest(value: f64, unit: Unit) -> String {
    scaled(value, unit, 1e3)
}

/// `value` in `unit`, in thousands from `thousands` on, and in millions
/// and billions.
fn scaled(value: f64, unit: Unit, thousands: f64) -> String {
    let abs = value.abs();
    let (scaled, suffix) = if abs >= 1e9 {
        (value / 1e9, "G")
    } else if abs >= 1e6 {
        (value / 1e6, "M")
    } else if abs >= thousands {
        (value / 1e3, "k")
    } else {
        (value, "")
    };
    let number = if suffix.is_empty() {
        if scaled.fract() == 0.0 {
            grouped(scaled as i64)
        } else {
            format!("{scaled:.1}")
        }
    } else if scaled.abs() >= 100.0 {
        format!("{scaled:.0}{suffix}")
    } else {
        format!("{scaled:.1}{suffix}")
    };
    match unit {
        Unit::Sat => format!("{number} sat"),
        Unit::Count => number,
        Unit::Blocks => format!("{number} blocks"),
    }
}

/// A share or a ratio, as a percentage: `93.4%`, or `0.25%` under 1%.
pub fn percent(ratio: f64) -> String {
    let pct = ratio * 100.0;
    if pct != 0.0 && pct.abs() < 1.0 {
        format!("{pct:.2}%")
    } else {
        format!("{pct:.1}%")
    }
}

/// A relative change: `▲ 12.3%`, `▼ 4.0%`, `= 0%`, or `▲ new` from zero.
pub fn change(change: Option<f64>, value: f64) -> String {
    match change {
        None if value != 0.0 => "▲ new".into(),
        None => "–".into(),
        Some(c) if c.abs() < 0.0005 => "= 0%".into(),
        Some(c) => format!("{} {}", arrow(c), percent(c.abs())),
    }
}

/// A change of a ratio, in percentage points: `▲ 2.1 pts`.
pub fn points(delta: f64) -> String {
    let pts = delta * 100.0;
    if pts.abs() < 0.05 {
        "= 0 pts".into()
    } else {
        format!("{} {:.1} pts", arrow(delta), pts.abs())
    }
}

fn arrow(delta: f64) -> &'static str {
    if delta > 0.0 {
        "▲"
    } else {
        "▼"
    }
}

/// A span of seconds, roughly: `45s`, `12m`, `3h`, `2d`.
pub fn span(seconds: i64) -> String {
    let s = seconds.abs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 2 * 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// When `at` was, from `now`: `5m ago`, `in 3h`, or its date beyond a week.
pub fn when(at: i64, now: i64) -> String {
    let ago = now - at;
    if ago.abs() > 7 * 86_400 {
        date(at)
    } else if ago.abs() < 30 {
        "just now".into()
    } else if ago > 0 {
        format!("{} ago", span(ago))
    } else {
        format!("in {}", span(ago))
    }
}

/// Blocks from now with the time they take: `144 · ~1d`, `3 past`.
pub fn blocks(blocks: i64) -> String {
    if blocks < 0 {
        format!("{} past", grouped(-blocks))
    } else {
        format!("{} · ~{}", grouped(blocks), span(blocks * 600))
    }
}

/// Days of runway: `12.5d`, `>1y`.
pub fn days(days: f64) -> String {
    if days > 365.0 {
        ">1y".into()
    } else if days >= 10.0 {
        format!("{days:.0}d")
    } else {
        format!("{days:.1}d")
    }
}

/// The UTC date of `at`: `2026-11-15`.
pub fn date(at: i64) -> String {
    let (y, m, d) = civil(at.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// A bucket's start, as an axis reads it for buckets of `bucket` seconds:
/// `14:00`, `Nov 15`, `Nov 15 06h`.
pub fn tick(at: i64, bucket: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d) = civil(at.div_euclid(86_400));
    let hour = at.rem_euclid(86_400) / 3600;
    let month = MONTHS[(m - 1) as usize];
    if bucket < 3600 * 6 {
        format!("{hour:02}:00")
    } else if bucket < 86_400 {
        format!("{month} {d} {hour:02}h")
    } else if bucket >= 30 * 86_400 {
        format!("{month} {y}")
    } else {
        format!("{month} {d}")
    }
}

/// The civil date of a day since the Unix epoch (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `text` in lines at most `width` wide, broken between words; a word
/// longer than a line is split.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let used = line.chars().count();
            let room = if used == 0 {
                width
            } else {
                width.saturating_sub(used + 1)
            };
            if word.len() <= room {
                if used > 0 {
                    line.push(' ');
                }
                line.extend(word.iter());
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            lines.push(word[..width].iter().collect());
            word.drain(..width);
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_wraps_between_words() {
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("abcdefghij  k", 4), ["abcd", "efgh", "ij k"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("fits", 10), ["fits"]);
    }

    #[test]
    fn amounts_group_thousands() {
        assert_eq!(amount(1_234_567, Unit::Sat), "1,234,567 sat");
        assert_eq!(amount(-1_000, Unit::Count), "-1,000");
        assert_eq!(amount(999, Unit::Count), "999");
        assert_eq!(compact(12_345_678.0, Unit::Sat), "12.3M sat");
        assert_eq!(compact(2_500.0, Unit::Count), "2,500");
        assert_eq!(compact(45_000.0, Unit::Count), "45.0k");
        assert_eq!(shortest(5_720.9, Unit::Sat), "5.7k sat");
        assert_eq!(shortest(999.0, Unit::Count), "999");
    }

    #[test]
    fn changes_read_with_arrows() {
        assert_eq!(change(Some(0.123), 1.0), "▲ 12.3%");
        assert_eq!(change(Some(-0.04), 1.0), "▼ 4.0%");
        assert_eq!(change(None, 5.0), "▲ new");
        assert_eq!(change(None, 0.0), "–");
        assert_eq!(percent(0.0025), "0.25%");
        assert_eq!(points(-0.021), "▼ 2.1 pts");
    }

    #[test]
    fn times_read_relative_to_now() {
        assert_eq!(when(1_000, 1_010), "just now");
        assert_eq!(when(0, 3 * 3600), "3h ago");
        assert_eq!(when(10 * 3600, 0), "in 10h");
        assert_eq!(blocks(144), "144 · ~24h");
        assert_eq!(blocks(1_008), "1,008 · ~7d");
        assert_eq!(blocks(-3), "3 past");
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(date(0), "1970-01-01");
        // 2026-11-15T00:00:00Z
        assert_eq!(date(1_794_700_800), "2026-11-15");
        assert_eq!(date(1_794_700_800 - 1), "2026-11-14");
        assert_eq!(tick(1_794_700_800, 86_400), "Nov 15");
        assert_eq!(tick(1_794_700_800 + 14 * 3600, 3600), "14:00");
        assert_eq!(tick(1_794_700_800 + 6 * 3600, 6 * 3600), "Nov 15 06h");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}
