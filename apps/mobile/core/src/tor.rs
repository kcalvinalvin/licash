//! [`TorClient`]: the app's one Tor client, for as long as the app runs,
//! wallet or not. Wallets opened over Tor reach hubs and relays through
//! it, and the light client reaches Bitcoin through its SOCKS5 proxy, so
//! the light client can start before a wallet is made or opened.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use crate::{LicashError, TorStatus};

/// The app's Tor client.
#[derive(uniffi::Object)]
pub struct TorClient {
    runtime: tokio::runtime::Runtime,
    tor: licash_tor::Tor,
    dir: PathBuf,
    /// The SOCKS5 proxy through it, once asked for ([`TorClient::proxy`]).
    socks: Mutex<Option<licash_tor::Socks>>,
}

#[uniffi::export]
impl TorClient {
    /// The Tor client keeping its state and directory cache in `dir`. One
    /// kept in `legacy` by an earlier version, inside the wallet's
    /// directory, is moved to `dir` the first time, should `dir` hold
    /// none; one that cannot be moved is used where it is. It connects to
    /// the Tor network once first used, or asked how it stands.
    #[uniffi::constructor]
    pub fn new(dir: String, legacy: Option<String>) -> Result<Arc<Self>, LicashError> {
        let dir = placed(Path::new(&dir), legacy.as_deref().map(Path::new));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tor")
            .enable_all()
            .build()
            .map_err(LicashError::failed)?;
        let tor = {
            let _inside = runtime.enter();
            licash_tor::Tor::new(&dir).map_err(LicashError::failed)?
        };
        Ok(Arc::new(Self {
            runtime,
            tor,
            dir,
            socks: Mutex::new(None),
        }))
    }

    /// Where it keeps its state: `dir`, or `legacy` if that could not move.
    pub fn dir(&self) -> String {
        self.dir.display().to_string()
    }

    /// How far it is in connecting to the Tor network. Not connected, it
    /// starts connecting now rather than at its first use.
    pub fn status(&self) -> TorStatus {
        let status = self.tor.status();
        if !status.ready {
            let tor = self.tor.clone();
            self.runtime.spawn(async move {
                if let Err(e) = tor.bootstrap().await {
                    tracing::warn!("connecting to Tor: {e}");
                }
            });
        }
        TorStatus {
            percent: status.percent,
            ready: status.ready,
            summary: status.summary,
            blocked: status.blocked,
        }
    }

    /// A SOCKS5 proxy through it, for the light client to reach everything
    /// through, as `socks5://username:password@host:port`: on the loopback
    /// interface, only with these credentials, while the client lives. The
    /// same each time.
    pub fn proxy(&self) -> Result<String, LicashError> {
        let mut socks = self.socks.lock().unwrap_or_else(PoisonError::into_inner);
        if socks.is_none() {
            let served = self.runtime.block_on(self.tor.socks());
            *socks = Some(served.map_err(LicashError::failed)?);
        }
        let socks = socks.as_ref().expect("served above");
        let (user, password) = (socks.username(), socks.password());
        Ok(format!("socks5://{user}:{password}@{}", socks.address()))
    }
}

impl TorClient {
    /// The client itself, for a wallet's transport and relays.
    pub(crate) fn tor(&self) -> licash_tor::Tor {
        self.tor.clone()
    }
}

/// Where a Tor client of `dir` keeps its state, moving it from `legacy`
/// first if only `legacy` holds any:
///
/// | `dir` holds state | `legacy` holds state | used                        |
/// |-------------------|----------------------|-----------------------------|
/// | yes               | any                  | `dir`, `legacy` left alone  |
/// | no                | yes                  | `legacy` moved to `dir`     |
/// | no                | yes, cannot move     | `legacy`, where it is       |
/// | no                | no                   | `dir`, fresh                |
///
/// Nothing is deleted: what a move leaves is what it could not take.
fn placed(dir: &Path, legacy: Option<&Path>) -> PathBuf {
    let holds = |d: &Path| d.join("state").exists() || d.join("cache").exists();
    let Some(legacy) = legacy.filter(|l| *l != dir && holds(l)) else {
        return dir.to_path_buf();
    };
    if holds(dir) {
        return dir.to_path_buf();
    }
    let moved = (|| -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        for part in ["state", "cache"] {
            let from = legacy.join(part);
            if from.exists() {
                fs::rename(&from, dir.join(part))?;
            }
        }
        Ok(())
    })();
    match moved {
        Ok(()) => {
            tracing::info!("Tor's state moved out of the wallet's directory");
            dir.to_path_buf()
        }
        Err(e) => {
            // A part moved and one not: put it back, so one place holds all.
            for part in ["state", "cache"] {
                let to = legacy.join(part);
                if !to.exists() && dir.join(part).exists() {
                    let _ = fs::rename(dir.join(part), &to);
                }
            }
            tracing::warn!("Tor's state stays in the wallet's directory: {e}");
            legacy.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::placed;
    use std::fs;

    fn holding(dir: &std::path::Path, marker: &str) {
        fs::create_dir_all(dir.join("state")).unwrap();
        fs::write(dir.join("state/marker"), marker).unwrap();
        fs::create_dir_all(dir.join("cache")).unwrap();
    }

    #[test]
    fn a_fresh_install_uses_the_new_place() {
        let root = tempfile::tempdir().unwrap();
        let (dir, legacy) = (root.path().join("tor"), root.path().join("wallet/tor"));
        assert_eq!(placed(&dir, Some(&legacy)), dir);
        assert_eq!(placed(&dir, None), dir);
        assert!(!legacy.exists());
    }

    #[test]
    fn state_in_the_wallet_is_moved_once() {
        let root = tempfile::tempdir().unwrap();
        let (dir, legacy) = (root.path().join("tor"), root.path().join("wallet/tor"));
        holding(&legacy, "old");
        assert_eq!(placed(&dir, Some(&legacy)), dir);
        assert_eq!(fs::read_to_string(dir.join("state/marker")).unwrap(), "old");
        assert!(dir.join("cache").exists());
        assert!(!legacy.join("state").exists() && !legacy.join("cache").exists());
        // Again: nothing left to move, the new place used.
        assert_eq!(placed(&dir, Some(&legacy)), dir);
    }

    #[test]
    fn state_in_both_places_leaves_the_old_alone() {
        let root = tempfile::tempdir().unwrap();
        let (dir, legacy) = (root.path().join("tor"), root.path().join("wallet/tor"));
        holding(&dir, "new");
        holding(&legacy, "old");
        assert_eq!(placed(&dir, Some(&legacy)), dir);
        assert_eq!(fs::read_to_string(dir.join("state/marker")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(legacy.join("state/marker")).unwrap(),
            "old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_that_cannot_move_is_used_where_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("locked");
        fs::create_dir_all(&parent).unwrap();
        let (dir, legacy) = (parent.join("tor"), root.path().join("wallet/tor"));
        holding(&legacy, "old");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        // Permissions do not bind root: nothing to test then.
        let binding = fs::create_dir(parent.join("probe")).is_err();
        let used = placed(&dir, Some(&legacy));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        if !binding {
            return;
        }
        assert_eq!(used, legacy);
        assert_eq!(
            fs::read_to_string(legacy.join("state/marker")).unwrap(),
            "old"
        );
    }
}
