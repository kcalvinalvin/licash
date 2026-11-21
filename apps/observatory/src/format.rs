//! Numbers, fees and times as the page says them (apps/DESIGN.md): amounts
//! grouped with their unit, fees as sat and a share, times as spans.

use licash_api::Fee;

/// `12,345 sat`.
pub fn sat(amount: u64) -> String {
    format!("{} sat", grouped(amount))
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

/// A share as a percentage, two significant digits: `0.24%`, `1.2%`,
/// `12%`, `<0.01%` below that; `0%` for none.
pub fn percent(share: f64) -> String {
    let p = share * 100.0;
    if p <= 0.0 {
        "0%".into()
    } else if p < 0.01 {
        "<0.01%".into()
    } else if p < 1.0 {
        format!("{p:.2}%")
    } else if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

/// A fee on `of`: `12 sat · 0.24%`, or `None`.
pub fn fee_of(fee: u64, of: u64) -> String {
    if fee == 0 {
        return "None".into();
    }
    if of == 0 {
        return sat(fee);
    }
    format!("{} · {}", sat(fee), percent(fee as f64 / of as f64))
}

/// A fee's terms: `1 sat + 0.01%`, `0.5%`, `1 sat`, or `None`.
pub fn terms(fee: &Fee) -> String {
    let rate = (fee.ppm > 0).then(|| percent(fee.ppm as f64 / 1_000_000.0));
    match (fee.base, rate) {
        (0, None) => "None".into(),
        (0, Some(rate)) => rate,
        (base, None) => sat(base),
        (base, Some(rate)) => format!("{} + {rate}", sat(base)),
    }
}

/// Uptime, never rounded up to a whole: `99.9%`, `100%` only if whole.
pub fn uptime(share: f64) -> String {
    if share >= 1.0 {
        return "100%".into();
    }
    let tenths = (share * 1000.0).floor() / 10.0;
    format!("{tenths:.1}%")
}

/// A duration: `340 ms`, `1.2 s`, `48 s`, `3 min`, `2.5 h`.
pub fn millis(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms} ms"),
        1000..10_000 => format!("{:.1} s", ms as f64 / 1000.0),
        10_000..120_000 => format!("{} s", ms / 1000),
        120_000..7_200_000 => format!("{} min", ms / 60_000),
        _ => format!("{:.1} h", ms as f64 / 3_600_000.0),
    }
}

/// How long ago `at` was, from `now`, both in ms: `just now`,
/// `5 min ago`, `3 hours ago`, `2 days ago`.
pub fn ago(at: i64, now: i64) -> String {
    let s = (now - at).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..7200 => format!("{} min ago", s / 60),
        7200..172_800 => format!("{} hours ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// How long until `at`, from `now`: `in 5 min`, `in 3 hours`, `now`.
pub fn until(at: i64, now: i64) -> String {
    let s = (at - now).max(0) / 1000;
    match s {
        0..60 => "now".into(),
        60..7200 => format!("in {} min", s / 60),
        7200..172_800 => format!("in {} hours", s / 3600),
        _ => format!("in {} days", s / 86_400),
    }
}

/// An interest rate by the year, from millionths: `2.1% a year`.
pub fn yearly(ppm: u32) -> String {
    if ppm == 0 {
        return "None".into();
    }
    format!("{} a year", percent(f64::from(ppm) / 1_000_000.0))
}

/// `a1b2c3d4…`, for an id where one is needed.
pub fn short(id: &str) -> String {
    if id.len() <= 12 {
        return id.to_owned();
    }
    let cut = id.char_indices().nth(8).map_or(id.len(), |(i, _)| i);
    format!("{}…", &id[..cut])
}

/// Text made safe for HTML.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `s` for a URL's query.
pub fn query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_grouped_with_their_unit() {
        assert_eq!(sat(0), "0 sat");
        assert_eq!(sat(1_010), "1,010 sat");
        assert_eq!(sat(12_345_678), "12,345,678 sat");
    }

    #[test]
    fn a_fee_says_what_it_is_of() {
        assert_eq!(fee_of(12, 5_000), "12 sat · 0.24%");
        assert_eq!(fee_of(0, 5_000), "None");
        assert_eq!(fee_of(1, 1_000_000), "1 sat · <0.01%");
        assert_eq!(fee_of(500, 10_000), "500 sat · 5.0%");
    }

    #[test]
    fn terms_stay_terms() {
        assert_eq!(terms(&Fee { base: 1, ppm: 100 }), "1 sat + 0.01%");
        assert_eq!(terms(&Fee { base: 0, ppm: 5000 }), "0.50%");
        assert_eq!(terms(&Fee { base: 0, ppm: 0 }), "None");
    }

    #[test]
    fn uptime_is_never_rounded_up_to_whole() {
        assert_eq!(uptime(0.99999), "99.9%");
        assert_eq!(uptime(1.0), "100%");
        assert_eq!(uptime(0.5), "50.0%");
    }

    #[test]
    fn times_are_spans() {
        assert_eq!(millis(340), "340 ms");
        assert_eq!(millis(1_234), "1.2 s");
        assert_eq!(millis(180_000), "3 min");
        assert_eq!(ago(0, 5 * 60_000), "5 min ago");
        assert_eq!(until(3 * 3_600_000, 0), "in 3 hours");
    }

    #[test]
    fn html_and_queries_are_escaped() {
        assert_eq!(esc("<a href=\"x\">"), "&lt;a href=&quot;x&quot;&gt;");
        assert_eq!(query("https://h.example/x"), "https%3A%2F%2Fh.example%2Fx");
    }
}
