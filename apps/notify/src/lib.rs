//! Notifications for Licash services: the operator's targets, named by
//! Apprise-style URLs ([`Target`]), and a [`Relay`] that delivers a
//! service's event outbox to each of them, at least once and in order.

mod relay;
mod target;

pub use relay::{Item, Outbox, Relay, Timing};
pub use target::Target;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low,
    Normal,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
    pub priority: Priority,
}

impl Message {
    pub fn new(priority: Priority, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            priority,
        }
    }
}

/// `amount` sat as an operator reads it: `12,345 sat`.
pub fn sat(amount: u64) -> String {
    grouped(amount) + " sat"
}

/// `amount` sat, signed: `-1,234 sat`.
pub fn sat_signed(amount: i64) -> String {
    let sign = if amount < 0 { "-" } else { "" };
    format!("{sign}{}", sat(amount.unsigned_abs()))
}

/// `amount` sat and its rate of `of` as a percentage, rounded as the
/// dashboards show it: `106 sat (0.21%)`, `1 sat (12.5%)`. No rate of
/// nothing.
pub fn sat_rate(amount: i64, of: u64) -> String {
    if of == 0 {
        return sat_signed(amount);
    }
    let (abs, of) = (u128::from(amount.unsigned_abs()), u128::from(of));
    let rounded = |scale: u128| u64::try_from((abs * scale + of / 2) / of).unwrap_or(u64::MAX);
    // Below 1% in hundredths of a percent, from 1% in tenths.
    let (rate, units) = match rounded(10_000) {
        0 if abs == 0 => ("0".to_owned(), 0),
        h if h < 100 => (format!("0.{h:02}"), h),
        _ => {
            let t = rounded(1_000);
            (format!("{}.{}", grouped(t / 10), t % 10), t)
        }
    };
    let sign = if amount < 0 && units > 0 { "-" } else { "" };
    format!("{} ({sign}{rate}%)", sat_signed(amount))
}

/// `n` with commas between thousands.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `id` cut to its first 12 characters, as the dashboards show it.
pub fn short(id: &str) -> String {
    const SHOWN: usize = 12;
    match id.char_indices().nth(SHOWN) {
        Some((cut, _)) => format!("{}…", &id[..cut]),
        None => id.to_owned(),
    }
}

/// A body of one `Label: value` bullet per line.
pub fn list<V: AsRef<str>>(fields: &[(&str, V)]) -> String {
    fields
        .iter()
        .map(|(label, value)| format!("• {label}: {}", value.as_ref()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The HTTP client targets are sent with. Installs the TLS provider once.
pub fn client() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("licash-notify/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_with_commas_between_thousands() {
        assert_eq!(sat(0), "0 sat");
        assert_eq!(sat(999), "999 sat");
        assert_eq!(sat(1_000), "1,000 sat");
        assert_eq!(sat(123_456), "123,456 sat");
        assert_eq!(sat(1_234_567), "1,234,567 sat");
        assert_eq!(sat(u64::MAX), "18,446,744,073,709,551,615 sat");
    }

    #[test]
    fn signed_amounts_and_rates_in_percent() {
        assert_eq!(sat_signed(-1_234), "-1,234 sat");
        assert_eq!(sat_signed(5), "5 sat");
        assert_eq!(sat_rate(106, 50_000), "106 sat (0.21%)");
        assert_eq!(sat_rate(-55, 50_000), "-55 sat (-0.11%)");
        // Rounded: below 1% to a hundredth, from 1% to a tenth.
        assert_eq!(sat_rate(1, 3), "1 sat (33.3%)");
        assert_eq!(sat_rate(2, 3), "2 sat (66.7%)");
        assert_eq!(sat_rate(1, 8), "1 sat (12.5%)");
        assert_eq!(sat_rate(1, 1_000_000), "1 sat (0.00%)");
        assert_eq!(sat_rate(30, 1), "30 sat (3,000.0%)");
        assert_eq!(sat_rate(0, 7), "0 sat (0%)");
        assert_eq!(sat_rate(5, 0), "5 sat");
        assert!(sat_rate(i64::MIN, 1).starts_with("-9,223,372,036,854,775,808 sat (-"));
    }

    #[test]
    fn ids_are_cut_to_twelve_characters() {
        assert_eq!(short(&"ab".repeat(32)), "abababababab…");
        assert_eq!(short("abcdef012345"), "abcdef012345");
        assert_eq!(short("p1"), "p1");
        assert_eq!(short("ééééééééééééé"), "éééééééééééé…");
    }

    #[test]
    fn fields_list_one_bullet_per_line() {
        assert_eq!(
            list(&[("Amount", sat(5_000)), ("Swap", short("ab"))]),
            "• Amount: 5,000 sat\n• Swap: ab"
        );
    }
}
