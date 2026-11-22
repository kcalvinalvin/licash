//! A one-shot inbox check that can outlive the foreground command.

use std::{
    env,
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{bail, Context};
use licash_wallet::Wallet;

use crate::{collection_warnings, route, say, transport_at, Cli};

const WARNING_BYTES: u64 = 4096;

pub struct Collection {
    wallet: PathBuf,
    tor: bool,
    proxy: Option<String>,
}

impl Collection {
    pub fn new(wallet: PathBuf, cli: &Cli) -> Self {
        Self {
            wallet,
            tor: cli.tor,
            proxy: cli.proxy.clone(),
        }
    }

    /// The worker has its own process and never inherits a pipe from the caller.
    pub fn start(self) {
        if let Err(e) = self.spawn() {
            warn(&format!("could not start background collection ({e:#})"));
        }
    }

    fn spawn(self) -> anyhow::Result<()> {
        if self.lock(true)?.is_none() {
            return Ok(());
        }
        let mut child = Command::new(env::current_exe()?);
        child
            .arg("--wallet")
            .arg(&self.wallet)
            .arg("collect-background")
            .env("LICASH_WALLET_TOR", if self.tor { "true" } else { "false" })
            .env_remove("LICASH_WALLET_PROXY")
            .env_remove("LICASH_WALLET_RENEW_PPM")
            .env_remove("LICASH_WALLET_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(proxy) = &self.proxy {
            child.env("LICASH_WALLET_PROXY", proxy);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            child.process_group(0);
        }
        child.spawn()?;
        Ok(())
    }

    pub fn report(&self) {
        if let Err(e) = self.report_saved() {
            warn(&format!(
                "could not read background collection warnings ({e:#})"
            ));
        }
    }

    fn report_saved(&self) -> anyhow::Result<()> {
        if let Some(mut file) = self.lock(false)? {
            let mut text = Vec::new();
            (&mut file).take(WARNING_BYTES).read_to_end(&mut text)?;
            if !text.is_empty() {
                for line in String::from_utf8_lossy(&text).lines() {
                    warn(line);
                }
                file.set_len(0)?;
            }
        }
        Ok(())
    }

    /// Explicit collection waits for any pass that already has the wallet.
    /// Watch releases this guard between passes.
    pub async fn wait_for_turn(&self) -> anyhow::Result<File> {
        loop {
            if let Some(file) = self.lock(true)? {
                return Ok(file);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn work(&self) -> anyhow::Result<()> {
        let Some(mut file) = self.lock(true)? else {
            return Ok(());
        };
        let mut warnings = String::new();
        let collect = async {
            let (http, tor) = transport_at(&self.wallet, self.tor, self.proxy.as_deref())?;
            let wallet = Wallet::open(&self.wallet, http)?;
            if !wallet.hubs()?.is_empty() {
                let route = route(tor, self.proxy.as_deref())?;
                let collected = licash_nostr::wallet::collect(&wallet, &route).await?;
                collection_warnings(&collected, |warning| {
                    if warnings.len() < WARNING_BYTES as usize {
                        warnings.push_str(&warning.replace(['\r', '\n', '\t'], " "));
                        warnings.push('\n');
                    }
                });
            }
            anyhow::Ok(())
        };
        let result = tokio::time::timeout(Duration::from_secs(120), collect).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warnings.push_str(&format!("background collection failed ({e:#})\n")),
            Err(_) => warnings.push_str("background collection timed out\n"),
        }
        if !warnings.is_empty() {
            // Keep a previous failure through successful runs, and replace it
            // with the latest failure if it has not been reported yet.
            file.set_len(0)?;
            let end = warnings.floor_char_boundary(WARNING_BYTES as usize);
            file.write_all(&warnings.as_bytes()[..end])?;
        }
        Ok(())
    }

    fn lock(&self, create: bool) -> anyhow::Result<Option<File>> {
        let path = status_path(&self.wallet)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(e) if !create && e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            bail!("background collection status is not a regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid reads the process identity and takes no arguments.
            if meta.uid() != unsafe { libc::geteuid() }
                || meta.nlink() != 1
                || meta.mode() & 0o077 != 0
            {
                bail!("background collection status must be private to the wallet owner");
            }
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

fn status_path(wallet: &Path) -> anyhow::Result<PathBuf> {
    let mut path = wallet
        .canonicalize()
        .context("locate the wallet")?
        .into_os_string();
    path.push(".collect");
    Ok(path.into())
}

fn warn(message: &str) {
    eprintln!("warning: {}", say::terminal(message));
}
