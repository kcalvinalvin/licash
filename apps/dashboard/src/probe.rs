//! A running service, asked over HTTP whether it is up.

use std::time::Duration;

use crate::{Check, Tone};

/// The service at a URL, asked at its health path on every read.
pub struct Probe {
    url: String,
    path: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Probe {
    /// The service at `url`, such as `http://127.0.0.1:8338`, which
    /// answers `path`, such as `/health`, while it is up.
    pub fn new(url: &str, path: &str) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            path: path.to_owned(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()?,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }

    /// Whether the service answers, and how, as the check `Service`.
    pub fn check(&self) -> Check {
        let url = format!("{}{}", self.url, self.path);
        let answer = self.runtime.block_on(async {
            let response = self.client.get(&url).send().await?;
            Ok::<_, reqwest::Error>((response.status(), response.text().await?))
        });
        match answer {
            Ok((status, _)) if status.is_success() => {
                Check::new("Service", Tone::Good, format!("answers at {}", self.url))
            }
            Ok((status, body)) => Check::new(
                "Service",
                Tone::Bad,
                format!("{status} at {}: {}", self.url, body.trim()),
            ),
            Err(e) => Check::new(
                "Service",
                Tone::Warn,
                format!("no answer at {}: {}", self.url, root(&e)),
            ),
        }
    }
}

/// The check of a service no one asked.
pub fn unasked() -> Check {
    Check::new("Service", Tone::Muted, "not asked")
}

fn root(e: &dyn std::error::Error) -> String {
    let mut e = e;
    while let Some(source) = e.source() {
        e = source;
    }
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_that_does_not_answer_is_a_warning() {
        let probe = Probe::new("http://127.0.0.1:1/", "/health").unwrap();
        let check = probe.check();
        assert_eq!((check.name.as_str(), check.tone), ("Service", Tone::Warn));
        assert!(check.detail.starts_with("no answer at http://127.0.0.1:1:"));
    }
}
