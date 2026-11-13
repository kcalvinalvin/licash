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

/// The HTTP client targets are sent with. Installs the TLS provider once.
pub fn client() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("licash-notify/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
