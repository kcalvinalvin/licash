//! Explicit backups. Existing wallets and startup behavior are unchanged.
//! A phrase encodes the original 256-bit seed as BIP39 entropy, not the
//! BIP39 PBKDF seed. Only a state bundle also preserves notes and contracts:
//! one snapshot of `wallet.sqlite`, which holds the wallet's tables and the
//! holder's contracts, and the files kept beside it.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path},
    sync::Arc,
};

use anyhow::{ensure, Context as _, Result};
use bip39::{Language, Mnemonic};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{kept_file, wallet_file, Kept, Wallet};
use crate::{ChainSource, LicashError, Network, TorClient};

/// The bundle's format: 2 keeps the contracts in `wallet.sqlite`, where
/// format 1 kept them in a `contracts.sqlite` of their own, which this
/// version restores no more.
const FORMAT: u32 = 2;
const MAX_FILES: usize = 100_000;
const MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct Entry {
    bytes: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    format: u32,
    network: String,
    files: BTreeMap<String, Entry>,
}

fn failed(e: anyhow::Error) -> LicashError {
    LicashError::from_any(e, |reason| LicashError::Failed { reason })
}

fn seed_of(phrase: &str) -> Result<[u8; 32]> {
    let words = Mnemonic::parse_in_normalized(
        Language::English,
        &phrase.split_whitespace().collect::<Vec<_>>().join(" "),
    )?;
    words
        .to_entropy()
        .try_into()
        .map_err(|_| anyhow::anyhow!("a Licash phrase has 24 words"))
}

#[uniffi::export]
impl Wallet {
    /// Sensitive: show only after an explicit reveal. This restores keys,
    /// not notes or one-time recovery state. Never log or copy automatically.
    pub fn backup_phrase(&self) -> Result<String, LicashError> {
        let seed = self.wallet.backup_seed()?;
        Mnemonic::from_entropy(&seed)
            .map(|words| words.to_string())
            .map_err(LicashError::failed)
    }

    /// Explicit key-only restore, looking for its coins from `birthday`,
    /// the block written down with the words, or from the first block
    /// (a full scan) if none. Missing metadata on ordinary startup never
    /// selects this path.
    #[uniffi::constructor]
    pub fn from_phrase(
        dir: String,
        phrase: String,
        network: Network,
        birthday: Option<u32>,
        tor: Option<Arc<TorClient>>,
    ) -> Result<Arc<Self>, LicashError> {
        let seed = seed_of(&phrase).map_err(failed)?;
        let birthday = birthday.unwrap_or(1).max(1);
        let dir = std::path::PathBuf::from(dir);
        if holds_state(&dir) {
            return Err(LicashError::invalid("a wallet is there already"));
        }
        fs::create_dir_all(&dir).map_err(LicashError::failed)?;
        fs::write(
            kept_file(&dir),
            serde_json::to_vec(&Kept {
                birthday,
                ..Kept::default()
            })
            .map_err(LicashError::failed)?,
        )
        .map_err(LicashError::failed)?;
        Self::start(dir, tor, |path, http| {
            licash_wallet::Wallet::create(path, &seed, network.profile(), http)
        })
    }

    /// Freeze contract drivers and take WAL-aware database snapshots before
    /// copying instances. In-flight ordinary payments remain resumable rows.
    /// The bundle is private, unencrypted, and excludes Tor/proving caches.
    pub fn export_state_bundle(&self, to: String) -> Result<(), LicashError> {
        self.bundle(Path::new(&to)).map_err(failed)
    }

    /// Install only into an absent destination, after validating the entire
    /// archive. The caller must warn against keeping two wallet copies live.
    #[uniffi::constructor]
    pub fn from_state_bundle(
        dir: String,
        bundle: String,
        tor: Option<Arc<TorClient>>,
    ) -> Result<Arc<Self>, LicashError> {
        let dir = std::path::PathBuf::from(dir);
        let install = || -> Result<()> {
            ensure!(
                !holds_state(&dir),
                "a wallet is there already; nothing was overwritten"
            );
            let parent = dir.parent().context("no wallet parent directory")?;
            fs::create_dir_all(parent)?;
            let staged = tempfile::Builder::new()
                .prefix(".wallet-restore-")
                .tempdir_in(parent)?;
            let manifest = extract(Path::new(&bundle), staged.path())?;
            validate_database(&wallet_file(staged.path()))?;
            let http = licash_transport::Http::new(None)?;
            let wallet = licash_wallet::Wallet::open(&wallet_file(staged.path()), http)?;
            ensure!(
                wallet.network().name() == manifest.network,
                "bundle network disagrees with its wallet"
            );
            let staged_wallet = wallet_file(staged.path());
            licash_contracts::holder::validate_backup(&wallet, &staged_wallet)?;
            // Consent to exit is given on the phone it acts on, not restored.
            licash_contracts::holder::exit::revoke(&staged_wallet)?;
            drop(wallet);
            ensure!(
                !holds_state(&dir),
                "a wallet is there now; nothing was overwritten"
            );
            if dir.exists() {
                // What a wallet's making left before it failed, no wallet's
                // state: moved aside, never deleted.
                let aside = tempfile::Builder::new()
                    .prefix(".wallet-leftover-")
                    .tempdir_in(parent)?
                    .keep();
                fs::rename(&dir, aside.join("wallet"))?;
            }
            fs::rename(staged.path(), &dir)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        };
        install().map_err(failed)?;
        Self::open(dir.display().to_string(), tor)
    }

    /// At the user's explicit word only: look for payments on Bitcoin again
    /// from the wallet's first block, forgetting only what the chain told
    /// (coins found, spends seen) until found again. Notes, pending
    /// payments, contracts, recoveries and their one-time signing state are
    /// untouched. The block looked from, or none if it never looked.
    pub fn rescan_chain(&self, chain: Arc<dyn ChainSource>) -> Result<Option<u32>, LicashError> {
        let _contracts = self.contracting.try_lock().map_err(|_| {
            LicashError::invalid("wait for the protection under way before resetting")
        })?;
        let Some(next) = self.wallet.silent_next()? else {
            return Ok(None);
        };
        // Its first block found by time only on a chain as far as it looked:
        // one catching up would find a block too early.
        if self.kept().birthday == 0 {
            let height = chain
                .height()
                .map_err(|e| LicashError::failed(format!("the light client: {e}")))?;
            if height < next.saturating_sub(1) {
                return Err(LicashError::invalid(
                    "wait until Bitcoin is caught up, then reset",
                ));
            }
        }
        let from = self.first_block(chain.as_ref())?.min(next);
        self.wallet.watch_silent(None, from)?;
        tracing::info!(from, next, "looking at Bitcoin again, as asked");
        Ok(Some(from))
    }
}

impl Wallet {
    fn bundle(&self, to: &Path) -> Result<()> {
        let _contracts = self.contracting.try_lock().map_err(|_| {
            anyhow::anyhow!("wait for contract work to finish before making a state bundle")
        })?;
        let snapshots = tempfile::Builder::new()
            .prefix(".wallet-snapshot-")
            .tempdir_in(self.dir.parent().context("no wallet parent")?)?;
        // One snapshot holds the wallet's tables and the contracts' alike.
        self.wallet.backup_to(&wallet_file(snapshots.path()))?;
        let mut paths = vec!["wallet.sqlite".to_owned()];
        for name in ["app.json", "contacts.json", "contracts-change"] {
            let source = self.dir.join(name);
            if source.exists() {
                ensure!(
                    fs::symlink_metadata(&source)?.is_file(),
                    "not a regular wallet state file"
                );
                let copy = snapshots.path().join(name);
                fs::copy(source, &copy)?;
                // Caught mid-write: a bundle that would not restore is no backup.
                if name.ends_with(".json") {
                    serde_json::from_slice::<serde_json::Value>(&fs::read(&copy)?)
                        .with_context(|| format!("{name} changed during backup; try again"))?;
                }
                paths.push(name.into());
            }
        }
        collect_contracts(&self.dir, Path::new("contracts"), &mut paths)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let output = options
            .open(to)
            .context("creating a private state bundle")?;
        let write = || -> Result<()> {
            let mut archive = tar::Builder::new(&output);
            let mut files = BTreeMap::new();
            let mut total = 0u64;
            for name in paths {
                let source = if name.starts_with("contracts/") {
                    self.dir.join(&name)
                } else {
                    snapshots.path().join(&name)
                };
                let before = hash(&mut File::open(&source)?, &mut std::io::sink())?;
                total = total
                    .checked_add(before.bytes)
                    .context("bundle size overflow")?;
                ensure!(
                    total <= MAX_BYTES && files.len() < MAX_FILES,
                    "state bundle is too large"
                );
                ensure!(
                    !name.starts_with("contracts/") || before.bytes <= 128 * 1024 * 1024,
                    "contract state file is too large"
                );
                archive.append_path_with_name(&source, &name)?;
                ensure!(
                    hash(&mut File::open(&source)?, &mut std::io::sink())? == before,
                    "state changed during backup; try again"
                );
                files.insert(name, before);
            }
            let manifest = Manifest {
                kind: "licash-state-bundle".into(),
                format: FORMAT,
                network: self.wallet.network().name().into(),
                files,
            };
            let bytes = serde_json::to_vec(&manifest)?;
            ensure!(
                bytes.len() <= 4 * 1024 * 1024,
                "state bundle manifest is too large"
            );
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            archive.append_data(&mut header, "manifest.json", bytes.as_slice())?;
            archive.finish()?;
            output.sync_all()?;
            Ok(())
        };
        if let Err(error) = write() {
            drop(output);
            let _ = fs::remove_file(to);
            return Err(error);
        }
        Ok(())
    }
}

/// Whether `dir` holds any wallet state, be it in part: a restore never
/// lands over it.
fn holds_state(dir: &Path) -> bool {
    [
        "wallet.sqlite",
        "wallet.sqlite-wal",
        "contracts",
        "contacts.json",
    ]
    .iter()
    .any(|name| dir.join(name).exists())
}

fn allowed(path: &Path) -> bool {
    if !path
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return false;
    }
    let parts = path.iter().filter_map(|p| p.to_str()).collect::<Vec<_>>();
    match parts.as_slice() {
        ["wallet.sqlite" | "app.json" | "contacts.json" | "contracts-change"] => true,
        ["contracts", id, file] => {
            id.len() == 64
                && id.bytes().all(|b| b.is_ascii_hexdigit())
                && file.len() <= 128
                && !file.ends_with(".new")
        }
        _ => false,
    }
}

fn collect_contracts(root: &Path, relative: &Path, out: &mut Vec<String>) -> Result<()> {
    let source = root.join(relative);
    if !source.exists() {
        return Ok(());
    }
    ensure!(
        !fs::symlink_metadata(&source)?.file_type().is_symlink(),
        "symlinks cannot be backed up"
    );
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() && relative == Path::new("contracts") {
            collect_contracts(root, &path, out)?;
        } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".new") {
            continue;
        } else {
            ensure!(
                kind.is_file() && allowed(&path),
                "unexpected contract state path"
            );
            out.push(path.to_str().context("non-UTF8 state path")?.to_owned());
        }
    }
    Ok(())
}

fn hash(input: &mut impl Read, output: &mut impl Write) -> Result<Entry> {
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 65_536];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        ensure!(bytes <= MAX_BYTES, "state file too large");
        digest.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    Ok(Entry {
        bytes,
        sha256: hex::encode(digest.finalize()),
    })
}

fn extract(bundle: &Path, to: &Path) -> Result<Manifest> {
    ensure!(
        fs::metadata(bundle)?.len() <= MAX_BYTES + 16 * 1024 * 1024,
        "state bundle is too large"
    );
    let mut archive = tar::Archive::new(File::open(bundle)?);
    let (mut actual, mut manifest) = (BTreeMap::new(), None);
    let mut total = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        ensure!(
            entry.header().entry_type().is_file(),
            "bundle contains a link or non-file entry"
        );
        let path = entry.path()?.into_owned();
        let size = entry.size();
        total = total.checked_add(size).context("bundle size overflow")?;
        ensure!(
            total <= MAX_BYTES && actual.len() < MAX_FILES,
            "state bundle is too large"
        );
        if path == Path::new("manifest.json") {
            ensure!(
                manifest.is_none() && size <= 4 * 1024 * 1024,
                "invalid bundle manifest"
            );
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            manifest = Some(serde_json::from_slice::<Manifest>(&bytes)?);
            continue;
        }
        ensure!(allowed(&path), "unsafe or unsupported state path");
        ensure!(
            !path.starts_with("contracts") || size <= 128 * 1024 * 1024,
            "contract state file is too large"
        );
        let name = path.to_str().context("non-UTF8 state path")?.to_owned();
        ensure!(!actual.contains_key(&name), "duplicate state file");
        let dest = to.join(&path);
        fs::create_dir_all(dest.parent().context("no state parent")?)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(dest)?;
        let digest = hash(&mut entry, &mut file)?;
        file.sync_all()?;
        ensure!(digest.bytes == size, "truncated state file");
        actual.insert(name, digest);
    }
    let manifest = manifest.context("not a Licash state bundle")?;
    ensure!(
        manifest.kind == "licash-state-bundle" && manifest.format == FORMAT,
        "unknown state bundle format"
    );
    ensure!(
        actual.contains_key("wallet.sqlite") && manifest.files == actual,
        "state bundle checksum or file list mismatch"
    );
    for name in ["app.json", "contacts.json"] {
        let path = to.join(name);
        if path.exists() {
            serde_json::from_slice::<serde_json::Value>(&fs::read(path)?)?;
        }
    }
    Ok(manifest)
}

/// Bound records before opening an imported database through normal wallet
/// readers. No migrations or writes run during this integrity preflight.
fn validate_database(path: &Path) -> Result<()> {
    let c =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    c.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
        64 * 1024 * 1024,
    )?;
    let checked: String = c.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(checked == "ok", "invalid database in state bundle");
    let mut names = c.prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?;
    let tables = names
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(tables.len() <= 1_024, "too many state tables");
    for table in tables {
        let query = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let mut stmt = c.prepare(&query)?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(dir: &Path) -> Arc<Wallet> {
        Wallet::create(dir.display().to_string(), Network::Regtest, 42, None).unwrap()
    }

    #[test]
    fn a_phrase_encodes_the_existing_raw_seed_without_changing_derivation() {
        let dir = tempfile::tempdir().unwrap();
        let original = wallet(&dir.path().join("original"));
        let phrase = original.backup_phrase().unwrap();
        assert_eq!(phrase.split_whitespace().count(), 24);
        let restored = Wallet::from_phrase(
            dir.path().join("restored").display().to_string(),
            phrase.clone(),
            Network::Regtest,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            original.wallet.backup_seed().unwrap(),
            restored.wallet.backup_seed().unwrap()
        );
        assert_eq!(original.wallet.address(), restored.wallet.address());
        let kept: Kept =
            serde_json::from_slice(&fs::read(kept_file(&restored.dir)).unwrap()).unwrap();
        assert_eq!(
            kept.birthday, 1,
            "full scanning requires this explicit restore action"
        );
        // Its birthday written down with the words: looked from there.
        assert_eq!(original.birthday(), Some(42));
        let dated = Wallet::from_phrase(
            dir.path().join("dated").display().to_string(),
            phrase,
            Network::Regtest,
            original.birthday(),
            None,
        )
        .unwrap();
        assert_eq!(dated.birthday(), Some(42));
        assert_eq!(dated.wallet.address(), original.wallet.address());
        assert!(seed_of("not a recovery phrase").is_err());
        assert!(seed_of(&Mnemonic::from_entropy(&[1; 16]).unwrap().to_string()).is_err());
    }

    #[test]
    fn a_bundle_restores_identity_scan_state_and_contacts_without_resetting_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let original = wallet(&dir.path().join("original"));
        original.wallet.watch_silent(None, 100).unwrap();
        let bundle = dir.path().join("backup.tar");
        // Caught mid-write, its contacts would not restore: no bundle.
        fs::write(original.dir.join("contacts.json"), b"[{\"na").unwrap();
        assert!(original.bundle(&bundle).is_err());
        assert!(!bundle.exists());
        fs::write(original.dir.join("contacts.json"), b"[]").unwrap();
        // A contract of hers, which one snapshot of the wallet's file holds
        // beside the notes, and her consent to exit it, given on this phone
        // only.
        licash_contracts::holder::seed_contract(
            &wallet_file(&original.dir),
            "http://hub.example",
            &licash_api::Fees::default(),
        )
        .unwrap();
        let address = original.bitcoin_address().unwrap();
        let wrong = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx".to_owned();
        assert!(original.consent_to_exit(wrong, 2_016).is_err());
        assert!(original.consent_to_exit("not one".into(), 2_016).is_err());
        assert!(original.consent_to_exit(address.clone(), 0).is_err());
        let given = original.consent_to_exit(address.clone(), 2_016).unwrap();
        assert_eq!(
            (given.address.as_str(), given.exit_margin),
            (address.as_str(), 2_016)
        );
        original.bundle(&bundle).unwrap();
        assert_eq!(original.wallet.silent_next().unwrap(), Some(100));
        // Never over a file there already.
        assert!(original.bundle(&bundle).is_err());
        // A wallet's making that failed left its birthday only: set aside.
        let leftover = dir.path().join("restored");
        fs::create_dir(&leftover).unwrap();
        fs::write(kept_file(&leftover), b"{\"birthday\":7}").unwrap();
        let restored = Wallet::from_state_bundle(
            dir.path().join("restored").display().to_string(),
            bundle.display().to_string(),
            None,
        )
        .unwrap();
        assert_eq!(restored.wallet.address(), original.wallet.address());
        assert_eq!(restored.wallet.silent_next().unwrap(), Some(100));
        assert_eq!(fs::read(restored.dir.join("contacts.json")).unwrap(), b"[]");
        assert_eq!(restored.birthday(), Some(42));
        let kept = licash_contracts::holder::list(restored.wallet.path()).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].hub, "http://hub.example");
        assert_eq!(restored.exit_consent().unwrap(), None);
        assert_eq!(original.exit_consent().unwrap(), Some(given));
        let aside = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".wallet-leftover-")
            })
            .unwrap();
        assert_eq!(
            fs::read(kept_file(&aside.path().join("wallet"))).unwrap(),
            b"{\"birthday\":7}"
        );
    }

    fn archive(path: &Path, format: u32, duplicate: bool, bad_hash: bool) {
        let mut archive = tar::Builder::new(File::create(path).unwrap());
        let bytes = b"not opened until checks pass";
        for _ in 0..if duplicate { 2 } else { 1 } {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            archive
                .append_data(&mut header, "wallet.sqlite", bytes.as_slice())
                .unwrap();
        }
        let mut entry = hash(&mut bytes.as_slice(), &mut std::io::sink()).unwrap();
        if bad_hash {
            entry.sha256 = "bad".into();
        }
        let manifest = Manifest {
            kind: "licash-state-bundle".into(),
            format,
            network: "regtest".into(),
            files: BTreeMap::from([("wallet.sqlite".into(), entry)]),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive
            .append_data(&mut header, "manifest.json", bytes.as_slice())
            .unwrap();
        archive.finish().unwrap();
    }

    #[test]
    fn bad_checksums_duplicates_and_future_formats_never_install_partial_state() {
        // Of the format before this one, which kept the contracts in a file
        // of their own, and of one after it.
        for (format, duplicate, bad_hash) in [
            (2, false, true),
            (2, true, false),
            (1, false, false),
            (3, false, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let bundle = dir.path().join("input.tar");
            archive(&bundle, format, duplicate, bad_hash);
            let dest = dir.path().join("wallet");
            assert!(Wallet::from_state_bundle(
                dest.display().to_string(),
                bundle.display().to_string(),
                None
            )
            .is_err());
            assert!(!dest.exists());
        }
    }

    #[test]
    fn restore_never_overwrites_existing_state_and_rejects_unsafe_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(wallet_file(dir.path()), b"keep").unwrap();
        assert!(Wallet::from_state_bundle(
            dir.path().display().to_string(),
            "missing.tar".into(),
            None
        )
        .is_err());
        assert_eq!(fs::read(wallet_file(dir.path())).unwrap(), b"keep");
        for path in [
            "../wallet.sqlite",
            "/wallet.sqlite",
            "contracts/../../wallet.sqlite",
            "tor/state",
            "wallet.sqlite-wal",
            "contracts.sqlite",
        ] {
            assert!(!allowed(Path::new(path)), "{path}");
        }
        assert!(allowed(Path::new(&format!(
            "contracts/{}/1-2",
            "a".repeat(64)
        ))));
    }

    #[test]
    fn archive_links_and_corrupt_databases_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("input.tar");
        let mut archive = tar::Builder::new(File::create(&bundle).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o600);
        header.set_link_name("../outside").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "wallet.sqlite", std::io::empty())
            .unwrap();
        archive.finish().unwrap();
        drop(archive);
        let staged = dir.path().join("staged");
        fs::create_dir(&staged).unwrap();
        assert!(extract(&bundle, &staged).is_err());
        assert!(!staged.join("wallet.sqlite").exists());
        let broken = dir.path().join("broken.sqlite");
        fs::write(&broken, b"not a database").unwrap();
        assert!(validate_database(&broken).is_err());
    }

    #[test]
    fn explicit_scan_reset_keeps_keys_and_non_chain_recovery_files() {
        /// A light client at block `.0`, if asked: with the birthday known,
        /// nothing is.
        struct NoChain(Option<u32>);
        impl ChainSource for NoChain {
            fn height(&self) -> Result<u32, crate::ChainError> {
                Ok(self.0.unwrap())
            }
            fn hash(&self, _: u32) -> Result<Vec<u8>, crate::ChainError> {
                unreachable!()
            }
            fn time(&self, _: u32) -> Result<i64, crate::ChainError> {
                unreachable!()
            }
            fn header(&self, _: u32) -> Result<Vec<u8>, crate::ChainError> {
                unreachable!()
            }
            fn filter(&self, _: Vec<u8>) -> Result<Vec<u8>, crate::ChainError> {
                unreachable!()
            }
            fn block(&self, _: Vec<u8>) -> Result<Vec<u8>, crate::ChainError> {
                unreachable!()
            }
            fn broadcast(&self, _: Vec<u8>) -> Result<(), crate::ChainError> {
                unreachable!()
            }
            fn unconfirmed(&self, _: Vec<u8>) -> Result<Vec<u8>, crate::ChainError> {
                unreachable!()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let wallet = wallet(&dir.path().join("wallet"));
        // Never looked: nothing to look at again.
        assert_eq!(wallet.rescan_chain(Arc::new(NoChain(None))).unwrap(), None);
        assert_eq!(wallet.wallet.silent_next().unwrap(), None);
        wallet.wallet.watch_silent(None, 100).unwrap();
        let seed = wallet.wallet.backup_seed().unwrap();
        fs::create_dir(wallet.dir.join("contracts")).unwrap();
        fs::write(wallet.dir.join("contracts/pinned-proof"), b"must stay").unwrap();
        // Its birthday unknown, a light client behind where it looked
        // could not tell its first block: nothing changes.
        let kept = fs::read(kept_file(&wallet.dir)).unwrap();
        fs::remove_file(kept_file(&wallet.dir)).unwrap();
        assert!(wallet.rescan_chain(Arc::new(NoChain(Some(10)))).is_err());
        assert_eq!(wallet.wallet.silent_next().unwrap(), Some(100));
        fs::write(kept_file(&wallet.dir), kept).unwrap();
        // From its birthday, block 42.
        assert_eq!(
            wallet.rescan_chain(Arc::new(NoChain(None))).unwrap(),
            Some(42)
        );
        assert_eq!(wallet.wallet.silent_next().unwrap(), Some(42));
        assert_eq!(wallet.wallet.backup_seed().unwrap(), seed);
        assert_eq!(
            fs::read(wallet.dir.join("contracts/pinned-proof")).unwrap(),
            b"must stay"
        );
    }
}
