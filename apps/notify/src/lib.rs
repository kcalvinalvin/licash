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
    let digits = amount.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 4);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped + " sat"
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
