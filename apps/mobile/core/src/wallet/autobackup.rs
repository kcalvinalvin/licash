//! Automatic backups: the wallet written, encrypted, to a folder the user
//! chose, which a sync tool (Syncthing, Nextcloud) carries off the phone.
//!
//! Every file is written once under a name of its own and never changed,
//! so a sync tool never sees a file replaced in part nor two versions of
//! one:
//!
//! - `s-<generation>-<writer>.lcb`: a snapshot, all the wallet holds but
//!   the contract states of 64 KiB or more, which it names. Generations
//!   rise; the newest three of this install are kept.
//! - `b-<name>.lcb`: one such state, named by a keyed hash of its bytes,
//!   written once however many snapshots name it. A contract's state is
//!   megabytes that do not compress; the rest is what changes.
//!
//! Each is sealed with XChaCha20-Poly1305 under a key of the seed and the
//! network, its header (magic, version, kind, folder id, generation,
//! writer) authenticated with it: a file cut short, of another wallet or
//! renamed is refused whole. The folder is `Licash-<id>`, `id` of the seed
//! and network too, telling nothing of the wallet.
//!
//! The app copies what it is handed from an outbox into the folder, then
//! deletes what it is told: this side alone decides, and records what it
//! wrote in `autobackup.json` beside the wallet's files before any file is
//! written. No record (a wallet backed up for the first time, or restored)
//! is a new writer, which deletes nothing it did not write.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, ensure, Context as _, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    backup::{self, failed, seed_of, Entry},
    wallet_file, Wallet,
};
use crate::{LicashError, Network, TorClient};

const MAGIC: &[u8; 4] = b"LCAB";
const VERSION: u8 = 1;
const SNAPSHOT: u8 = 1;
const BLOB: u8 = 2;
const HEADER: usize = 4 + 1 + 1 + 8 + 8 + 4;
const NONCE: usize = 24;
const TAG: usize = 16;
/// A contract state this long or longer is a blob of its own.
const DETACH_AT: usize = 64 * 1024;
/// The most bytes one file holds, sealed in one piece.
const MOST: usize = 64 * 1024 * 1024;
/// Snapshots of this install kept in the folder.
const KEEP: usize = 3;
/// A backup unchanged this long is written again: proof it is current.
const REFRESH_SECS: u64 = 24 * 3600;
const RECORD: &str = "autobackup.json";
const KIND: &str = "licash-auto-backup";
const FORMAT: u32 = 1;
/// Rows that change as Bitcoin is looked through, and are found again by
/// looking again: a change of theirs alone is no reason for a backup.
const SCANNING: [&str; 2] = ["silent_scan", "silent_blocks"];
/// Tables of contract states (`state`, by `id`), detached when large.
const DETACHABLE: [&str; 2] = ["contracts", "contract_recoveries"];

/// What to do about an automatic backup now.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum BackupStep {
    /// Nothing changed since the last one published: nothing to write.
    Unchanged,
    /// Contract work is under way: try again soon, the change still due.
    Busy,
    /// Copy `write` from the outbox into the folder, then delete `delete`
    /// from it, then call [`Wallet::backup_published`].
    Write { run: BackupRun },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BackupRun {
    /// The folder's name, in the folder the user chose.
    pub folder: String,
    pub generation: u64,
    /// File names in the outbox, to write under the same names, any file of
    /// that name there replaced: blobs first, the snapshot last.
    pub write: Vec<String>,
    /// File names to delete from the folder once all are written.
    pub delete: Vec<String>,
}

/// A file in the backup folder as the app lists it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FolderFile {
    pub name: String,
    /// Its length, if the folder tells.
    pub bytes: Option<u64>,
}

/// The newest backup a folder holds that restores whole.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BackupFound {
    pub generation: u64,
    /// When it was made, in seconds since the epoch.
    pub made_at: u64,
    /// Newer snapshots there that do not restore, perhaps not yet synced
    /// in full or of a newer app: the user is told before an older one is
    /// used.
    pub newer_unusable: u32,
    /// The phones whose snapshots are there: more than one, the newest
    /// need not be the fullest.
    pub writers: u32,
}

/// The name of the folder the backups of the wallet of `phrase` on
/// `network` are in, inside the one the user chose.
#[uniffi::export]
pub fn backup_folder_name(phrase: String, network: Network) -> Result<String, LicashError> {
    let seed = seed_of(&phrase).map_err(failed)?;
    Ok(Keys::of(&seed, network.profile().name()).folder())
}

/// The newest backup in `files`, a copy of the folder's files, of the
/// wallet of `phrase` on `network`: none if none restores.
#[uniffi::export]
pub fn find_backup(
    phrase: String,
    network: Network,
    files: String,
) -> Result<Option<BackupFound>, LicashError> {
    let seed = seed_of(&phrase).map_err(failed)?;
    let keys = Keys::of(&seed, network.profile().name());
    let files = Path::new(&files);
    let mut newer_unusable = 0;
    let found = snapshots(&keys, files).map_err(failed)?;
    let writers = found.iter().map(|s| s.2).collect::<BTreeSet<_>>().len() as u32;
    for (generation, name, _) in found {
        let scratch = tempfile::Builder::new()
            .prefix(".restore-check-")
            .tempdir_in(files)
            .map_err(LicashError::failed)?;
        match unpacked(&keys, files, &name, network, scratch.path()) {
            Ok(manifest) => {
                return Ok(Some(BackupFound {
                    generation,
                    made_at: manifest.made_at,
                    newer_unusable,
                    writers,
                }));
            }
            Err(e) => {
                tracing::warn!(name, "a backup that does not restore: {e:#}");
                newer_unusable += 1;
            }
        }
    }
    Ok(None)
}

#[uniffi::export]
impl Wallet {
    /// The folder its backups are in, inside the one the user chose.
    pub fn backup_folder(&self) -> Result<String, LicashError> {
        let seed = self.wallet.backup_seed()?;
        Ok(Keys::of(&seed, self.wallet.network().name()).folder())
    }

    /// What changes whenever the wallet's files do, cheap to read: when it
    /// differs from the last, a backup may be due. Not all changes make
    /// one ([`Wallet::prepare_backup`] tells).
    pub fn backup_stamp(&self) -> String {
        let mut stamp = String::new();
        for name in [
            "wallet.sqlite",
            "wallet.sqlite-wal",
            "app.json",
            "contacts.json",
            "contracts-change",
        ] {
            if let Ok(m) = fs::metadata(self.dir.join(name)) {
                let at = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                stamp.push_str(&format!("{name}:{}:{at};", m.len()));
            }
        }
        stamp
    }

    /// An automatic backup made in `outbox` (emptied first) if the wallet
    /// changed since the last published one, or that one is a day old, or
    /// if `force`; `present` is what the folder holds now.
    pub fn prepare_backup(
        &self,
        outbox: String,
        present: Vec<FolderFile>,
        force: bool,
    ) -> Result<BackupStep, LicashError> {
        let Ok(_contracts) = self.contracting.try_lock() else {
            return Ok(BackupStep::Busy);
        };
        self.prepare(Path::new(&outbox), &present, force, now())
            .map_err(failed)
    }

    /// The backup of `generation` is in the folder whole: the next is made
    /// only once the wallet changes.
    pub fn backup_published(&self, generation: u64) -> Result<(), LicashError> {
        let mut record = Record::load(&self.dir).map_err(failed)?;
        let Some(pending) = record.pending.take() else {
            return Err(LicashError::invalid("no backup under way"));
        };
        if pending.generation != generation {
            return Err(LicashError::invalid("not the backup under way"));
        }
        record.digest = Some(pending.digest);
        record.published_at = pending.made_at;
        for s in &mut record.snapshots {
            if s.generation == generation {
                s.published = true;
            }
        }
        let published = record.snapshots.iter().filter(|s| s.published).count();
        let mut drop = published.saturating_sub(KEEP);
        record.snapshots.retain(|s| {
            let gone = s.published && drop > 0;
            drop -= usize::from(gone);
            !gone
        });
        record.save(&self.dir).map_err(failed)?;
        tracing::info!(generation, "backup published");
        Ok(())
    }

    /// Install the backup of `generation` (as `find_backup` found it) from
    /// `files`, a copy of its folder's files, into `dir`, a wallet of
    /// `phrase` on `network`: never over a wallet there. The caller must warn against
    /// keeping two wallet copies live.
    #[uniffi::constructor]
    pub fn from_backup(
        dir: String,
        phrase: String,
        network: Network,
        files: String,
        generation: u64,
        tor: Option<Arc<TorClient>>,
    ) -> Result<Arc<Self>, LicashError> {
        let dir = std::path::PathBuf::from(dir);
        let install = || -> Result<()> {
            let seed = seed_of(&phrase)?;
            let keys = Keys::of(&seed, network.profile().name());
            let files = Path::new(&files);
            let (_, name, _) = snapshots(&keys, files)?
                .into_iter()
                .find(|(g, _, _)| *g == generation)
                .context("no backup of that generation there")?;
            let staged = backup::staging(&dir)?;
            // Sealed under a key of the seed: this wallet's.
            let manifest = unpacked(&keys, files, &name, network, staged.path())?;
            backup::install(&dir, staged, &manifest.network)?;
            tracing::info!(
                generation,
                made_at = manifest.made_at,
                "wallet restored from a backup"
            );
            Ok(())
        };
        install().map_err(failed)?;
        Self::open(dir.display().to_string(), tor)
    }
}

impl Wallet {
    fn prepare(
        &self,
        outbox: &Path,
        present: &[FolderFile],
        force: bool,
        now_ms: u64,
    ) -> Result<BackupStep> {
        let seed = self.wallet.backup_seed()?;
        let network = self.wallet.network().name();
        let keys = Keys::of(&seed, network);
        let mut record = Record::load(&self.dir)?;
        let writer = record.writer()?;
        let present: BTreeMap<&str, Option<u64>> =
            present.iter().map(|f| (f.name.as_str(), f.bytes)).collect();

        let snap = tempfile::Builder::new()
            .prefix(".wallet-autobackup-")
            .tempdir_in(self.dir.parent().context("no wallet parent")?)?;
        let snapshot = Snapshot::take(self, snap.path())?;
        let digest = snapshot.digest()?;
        let fresh = record.published_at / 1000 + REFRESH_SECS > now_ms / 1000;
        // The last published still there whole, as far as the folder tells:
        // a folder emptied or changed is written to again.
        let held = record
            .snapshots
            .iter()
            .rev()
            .find(|s| s.published)
            .is_some_and(|s| {
                present.contains_key(s.name.as_str())
                    && s.blobs.iter().all(|b| present.contains_key(b.as_str()))
            });
        if !force && record.digest.as_deref() == Some(digest.as_str()) && fresh && held {
            return Ok(BackupStep::Unchanged);
        }

        let highest = present
            .keys()
            .filter_map(|n| parse_snapshot(n))
            .map(|(g, _)| g)
            .max()
            .unwrap_or(0);
        let generation = now_ms.max(record.generation + 1).max(highest + 1);
        let name = snapshot_name(generation, &writer);
        let mut blobs = Vec::new();
        let mut write = Vec::new();
        for d in &snapshot.detached {
            let sha: [u8; 32] = hex::decode(&d.sha256)?
                .try_into()
                .map_err(|_| anyhow!("a digest"))?;
            let blob = keys.blob_name(&sha);
            // Of a length unknown, taken as whole: never deleted to be
            // written again, which a failure would leave missing.
            let whole = match present.get(blob.as_str()) {
                Some(Some(bytes)) => *bytes == sealed_len(d.bytes),
                Some(None) => true,
                None => false,
            };
            if !whole {
                write.push(blob.clone());
                record.wrote.insert(blob.clone());
            }
            blobs.push(blob);
        }
        // Recorded before any file is written: an interrupted run never
        // reuses its generation, and what it wrote is known as its own.
        record.generation = generation;
        record.pending = Some(Pending {
            generation,
            digest: digest.clone(),
            made_at: now_ms,
        });
        record.snapshots.retain(|s| s.published);
        record.snapshots.push(Written {
            generation,
            name: name.clone(),
            blobs: blobs.clone(),
            published: false,
        });
        record.save(&self.dir)?;

        fs::create_dir_all(outbox)?;
        for entry in fs::read_dir(outbox)? {
            let path = entry?.path();
            if path.is_file() {
                fs::remove_file(path)?;
            }
        }
        for (d, blob) in snapshot.detached.iter().zip(&blobs) {
            if write.contains(blob) {
                let sealed = keys.seal(BLOB, 0, [0; 4], &snapshot.states[&d.key()])?;
                fs::write(outbox.join(blob), sealed)?;
            }
        }
        let plain = snapshot.archive(network, generation, now_ms / 1000)?;
        fs::write(
            outbox.join(&name),
            keys.seal(SNAPSHOT, generation, writer, &plain)?,
        )?;
        write.push(name.clone());

        // Kept: the newest published snapshots of this install, and this.
        let kept: Vec<&Written> = record.snapshots.iter().rev().take(KEEP).collect();
        let named: BTreeSet<&str> = kept
            .iter()
            .flat_map(|s| s.blobs.iter().map(String::as_str))
            .collect();
        let kept_names: BTreeSet<&str> = kept.iter().map(|s| s.name.as_str()).collect();
        let mine = hex::encode(writer);
        let mut delete = Vec::new();
        for file in present.keys() {
            let stale_snapshot =
                parse_snapshot(file).is_some_and(|(_, w)| w == mine && !kept_names.contains(file));
            let stale_blob = record.wrote.contains(*file) && !named.contains(file);
            if stale_snapshot || stale_blob {
                delete.push((*file).to_owned());
            }
        }
        // What it wrote and is gone from the folder is no longer its to
        // delete.
        record
            .wrote
            .retain(|b| present.contains_key(b.as_str()) || write.contains(b));
        record.save(&self.dir)?;
        Ok(BackupStep::Write {
            run: BackupRun {
                folder: keys.folder(),
                generation,
                write,
                delete,
            },
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn tagged(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let t = Sha256::digest(tag.as_bytes());
    let mut h = Sha256::new();
    h.update(t);
    h.update(t);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The keys of a wallet's backups, of its seed and network.
struct Keys {
    id: [u8; 8],
    key: [u8; 32],
}

/// A file opened: its header's kind and generation, and its contents.
struct Opened {
    kind: u8,
    generation: u64,
    plain: Vec<u8>,
}

impl Keys {
    fn of(seed: &[u8; 32], network: &str) -> Self {
        let id = tagged("licash/backup/folder", &[seed, network.as_bytes()]);
        Self {
            id: id[..8].try_into().expect("8 bytes"),
            key: tagged("licash/backup/key", &[seed, network.as_bytes()]),
        }
    }

    fn folder(&self) -> String {
        format!("Licash-{}", hex::encode(self.id))
    }

    fn blob_name(&self, sha256: &[u8; 32]) -> String {
        let name = tagged("licash/backup/blob", &[&self.key, sha256]);
        format!("b-{}.lcb", hex::encode(&name[..16]))
    }

    fn header(&self, kind: u8, generation: u64, writer: [u8; 4]) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER + NONCE);
        h.extend_from_slice(MAGIC);
        h.push(VERSION);
        h.push(kind);
        h.extend_from_slice(&self.id);
        h.extend_from_slice(&generation.to_be_bytes());
        h.extend_from_slice(&writer);
        h
    }

    fn seal(&self, kind: u8, generation: u64, writer: [u8; 4], plain: &[u8]) -> Result<Vec<u8>> {
        ensure!(plain.len() <= MOST, "too large to back up");
        let mut out = self.header(kind, generation, writer);
        let mut nonce = [0u8; NONCE];
        OsRng.fill_bytes(&mut nonce);
        let sealed = XChaCha20Poly1305::new((&self.key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: &out,
                },
            )
            .map_err(|_| anyhow!("sealing a backup"))?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// The header of `bytes` if a file of these keys', unchecked else.
    fn peek(&self, bytes: &[u8]) -> Option<(u8, u64, [u8; 4])> {
        if bytes.len() < HEADER + NONCE + TAG || &bytes[..4] != MAGIC || bytes[6..14] != self.id {
            return None;
        }
        let generation = u64::from_be_bytes(bytes[14..22].try_into().ok()?);
        Some((bytes[5], generation, bytes[22..26].try_into().ok()?))
    }

    fn open(&self, bytes: &[u8]) -> Result<Opened> {
        let (kind, generation, _) = self.peek(bytes).context("not a backup of this wallet")?;
        ensure!(
            bytes[4] == VERSION,
            "a backup of a newer version of the app"
        );
        let (header, rest) = bytes.split_at(HEADER);
        let (nonce, sealed) = rest.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into()?;
        let plain = XChaCha20Poly1305::new((&self.key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: sealed,
                    aad: header,
                },
            )
            .map_err(|_| anyhow!("a backup file cut short or altered"))?;
        Ok(Opened {
            kind,
            generation,
            plain,
        })
    }
}

fn sealed_len(plain: u64) -> u64 {
    (HEADER + NONCE + TAG) as u64 + plain
}

fn snapshot_name(generation: u64, writer: &[u8; 4]) -> String {
    format!("s-{generation:020}-{}.lcb", hex::encode(writer))
}

fn parse_snapshot(name: &str) -> Option<(u64, &str)> {
    let rest = name.strip_prefix("s-")?.strip_suffix(".lcb")?;
    let (generation, writer) = rest.split_once('-')?;
    (writer.len() == 8).then_some(())?;
    Some((generation.parse().ok()?, writer))
}

/// The snapshots of `keys` in `files`, newest first, by their headers
/// whatever their names: generation, name and writer.
fn snapshots(keys: &Keys, files: &Path) -> Result<Vec<(u64, String, [u8; 4])>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(files)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let mut head = [0u8; HEADER + NONCE + TAG];
        let Ok(mut file) = fs::File::open(entry.path()) else {
            continue;
        };
        if file.read_exact(&mut head).is_err() {
            continue;
        }
        if let Some((SNAPSHOT, generation, writer)) = keys.peek(&head) {
            if let Some(name) = entry.file_name().to_str() {
                found.push((generation, name.to_owned(), writer));
            }
        }
    }
    found.sort_by(|a, b| b.cmp(a));
    Ok(found)
}

/// The snapshot `name` in `files` unpacked `to`, its contract states put
/// back from their blobs: an error unless all of it holds.
fn unpacked(
    keys: &Keys,
    files: &Path,
    name: &str,
    network: Network,
    to: &Path,
) -> Result<Manifest> {
    let bytes = read_most(&files.join(name))?;
    let opened = keys.open(&bytes)?;
    ensure!(opened.kind == SNAPSHOT, "not a snapshot");
    let tar = flate2::read::GzDecoder::new(opened.plain.as_slice());
    let (manifest, actual) = backup::unpack(tar.take(backup::MAX_BYTES), to)?;
    let manifest: Manifest = serde_json::from_slice(&manifest)?;
    ensure!(
        manifest.kind == KIND && manifest.format == FORMAT,
        "a backup of a newer version of the app"
    );
    ensure!(
        manifest.network == network.profile().name() && manifest.generation == opened.generation,
        "a snapshot whose header and contents disagree"
    );
    ensure!(
        actual.contains_key("wallet.sqlite") && manifest.files == actual,
        "a snapshot's files are not as it lists them"
    );
    let db = Connection::open(wallet_file(to))?;
    for d in &manifest.detached {
        ensure!(DETACHABLE.contains(&d.table.as_str()), "an unknown table");
        let sha: [u8; 32] = hex::decode(&d.sha256)?
            .try_into()
            .map_err(|_| anyhow!("a digest"))?;
        let blob = keys.open(&read_most(&files.join(keys.blob_name(&sha)))?)?;
        ensure!(
            blob.kind == BLOB
                && blob.plain.len() as u64 == d.bytes
                && Sha256::digest(&blob.plain)[..] == sha[..],
            "a contract's state that is not the one named"
        );
        let changed = db.execute(
            &format!("UPDATE {} SET state = ?1 WHERE id = ?2", d.table),
            rusqlite::params![blob.plain, hex::decode(&d.id)?],
        )?;
        ensure!(changed == 1, "a contract's state of no contract");
    }
    drop(db);
    Ok(manifest)
}

fn read_most(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("{} missing", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MOST + HEADER + NONCE + TAG + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MOST + HEADER + NONCE + TAG, "too large");
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    format: u32,
    network: String,
    generation: u64,
    /// When made, in seconds since the epoch.
    made_at: u64,
    files: BTreeMap<String, Entry>,
    detached: Vec<Detached>,
}

/// A contract state kept as a blob: emptied in the snapshot's database.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Detached {
    table: String,
    /// The row's `id`, in hex.
    id: String,
    sha256: String,
    bytes: u64,
}

impl Detached {
    fn key(&self) -> (String, String) {
        (self.table.clone(), self.id.clone())
    }
}

/// The wallet's files copied, its large contract states set apart.
struct Snapshot<'a> {
    dir: &'a Path,
    /// Each file but the database, by its name in the archive.
    files: Vec<String>,
    detached: Vec<Detached>,
    states: BTreeMap<(String, String), Vec<u8>>,
}

impl<'a> Snapshot<'a> {
    fn take(wallet: &Wallet, dir: &'a Path) -> Result<Self> {
        wallet.wallet.backup_to(&wallet_file(dir))?;
        let mut files = Vec::new();
        for name in ["app.json", "contacts.json", "contracts-change"] {
            let source = wallet.dir.join(name);
            if !source.exists() {
                continue;
            }
            ensure!(
                fs::symlink_metadata(&source)?.is_file(),
                "not a regular wallet state file"
            );
            fs::copy(&source, dir.join(name))?;
            if name.ends_with(".json") {
                serde_json::from_slice::<serde_json::Value>(&fs::read(dir.join(name))?)
                    .with_context(|| format!("{name} changed during backup"))?;
            }
            files.push(name.to_owned());
        }
        let mut legacy = Vec::new();
        backup::collect_contracts(&wallet.dir, Path::new("contracts"), &mut legacy)?;
        for name in legacy {
            let to = dir.join(&name);
            fs::create_dir_all(to.parent().context("no parent")?)?;
            fs::copy(wallet.dir.join(&name), to)?;
            files.push(name);
        }

        let db = Connection::open(wallet_file(dir))?;
        let (mut detached, mut states) = (Vec::new(), BTreeMap::new());
        for table in DETACHABLE {
            let exists: bool = db.query_row(
                "SELECT count(*) > 0 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )?;
            if !exists {
                continue;
            }
            let mut rows = db.prepare(&format!(
                "SELECT id, state FROM {table} WHERE length(state) >= ?1 ORDER BY id"
            ))?;
            let large = rows
                .query_map([DETACH_AT as i64], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(rows);
            for (id, state) in large {
                ensure!(
                    state.len() <= MOST,
                    "a contract's state too large to back up"
                );
                let d = Detached {
                    table: table.to_owned(),
                    id: hex::encode(&id),
                    sha256: hex::encode(Sha256::digest(&state)),
                    bytes: state.len() as u64,
                };
                db.execute(
                    &format!("UPDATE {table} SET state = x'' WHERE id = ?1"),
                    [&id],
                )?;
                states.insert(d.key(), state);
                detached.push(d);
            }
        }
        Ok(Self {
            dir,
            files,
            detached,
            states,
        })
    }

    /// Of all that is worth a backup: every row but those of looking
    /// through Bitcoin, in no order of storage, and the other files.
    fn digest(&self) -> Result<String> {
        let db =
            Connection::open_with_flags(wallet_file(self.dir), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut names = db.prepare(
            "SELECT name FROM sqlite_schema WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let mut tables = names
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(names);
        tables.retain(|t| !SCANNING.contains(&t.as_str()));
        let mut all = Sha256::new();
        for table in tables {
            let mut stmt =
                db.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
            let columns = stmt.column_count();
            let mut rows = stmt.query([])?;
            let mut hashes = Vec::new();
            while let Some(row) = rows.next()? {
                let mut h = Sha256::new();
                for i in 0..columns {
                    match row.get_ref(i)? {
                        ValueRef::Null => h.update([0]),
                        ValueRef::Integer(n) => {
                            h.update([1]);
                            h.update(n.to_le_bytes());
                        }
                        ValueRef::Real(f) => {
                            h.update([2]);
                            h.update(f.to_bits().to_le_bytes());
                        }
                        ValueRef::Text(b) | ValueRef::Blob(b) => {
                            h.update([3]);
                            h.update((b.len() as u64).to_le_bytes());
                            h.update(b);
                        }
                    }
                }
                hashes.push(<[u8; 32]>::from(h.finalize()));
            }
            hashes.sort_unstable();
            all.update((table.len() as u64).to_le_bytes());
            all.update(table.as_bytes());
            all.update((hashes.len() as u64).to_le_bytes());
            for h in hashes {
                all.update(h);
            }
        }
        all.update(serde_json::to_vec(&self.detached)?);
        for name in &self.files {
            all.update((name.len() as u64).to_le_bytes());
            all.update(name.as_bytes());
            all.update(Sha256::digest(fs::read(self.dir.join(name))?));
        }
        Ok(hex::encode(all.finalize()))
    }

    /// The snapshot as a gzipped tar, its manifest in it.
    fn archive(&self, network: &str, generation: u64, made_at: u64) -> Result<Vec<u8>> {
        // What the copy holds of rows deleted since is not backed up.
        Connection::open(wallet_file(self.dir))?.execute_batch("VACUUM")?;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut listed = BTreeMap::new();
        {
            let mut tar = tar::Builder::new(&mut gz);
            let mut names = vec!["wallet.sqlite".to_owned()];
            names.extend(self.files.iter().cloned());
            for name in names {
                let bytes = fs::read(self.dir.join(&name))?;
                listed.insert(
                    name.clone(),
                    Entry {
                        bytes: bytes.len() as u64,
                        sha256: hex::encode(Sha256::digest(&bytes)),
                    },
                );
                append(&mut tar, &name, &bytes)?;
            }
            let manifest = serde_json::to_vec(&Manifest {
                kind: KIND.into(),
                format: FORMAT,
                network: network.into(),
                generation,
                made_at,
                files: listed,
                detached: self.detached.clone(),
            })?;
            append(&mut tar, "manifest.json", &manifest)?;
            tar.finish()?;
        }
        Ok(gz.finish()?)
    }
}

fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)?;
    Ok(())
}

/// What this install recorded of its automatic backups.
#[derive(Default, Serialize, Deserialize)]
struct Record {
    /// Its 4 random bytes, in hex, naming the snapshots it writes.
    #[serde(default)]
    writer: String,
    /// The last generation it began.
    #[serde(default)]
    generation: u64,
    /// What the last snapshot published holds, and when it was made (ms).
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    published_at: u64,
    /// Its snapshots kept and the one under way, oldest first.
    #[serde(default)]
    snapshots: Vec<Written>,
    /// The blobs it wrote, so its own to delete once no snapshot of its
    /// names them.
    #[serde(default)]
    wrote: BTreeSet<String>,
    #[serde(default)]
    pending: Option<Pending>,
}

#[derive(Serialize, Deserialize)]
struct Written {
    generation: u64,
    name: String,
    blobs: Vec<String>,
    published: bool,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    generation: u64,
    digest: String,
    made_at: u64,
}

impl Record {
    /// This install's record; none, or one unreadable, is a new writer's.
    fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(RECORD);
        let record = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("an unreadable backup record, begun anew: {e}");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(record)
    }

    fn writer(&mut self) -> Result<[u8; 4]> {
        if let Ok(Ok(w)) = hex::decode(&self.writer).map(<[u8; 4]>::try_from) {
            return Ok(w);
        }
        let mut w = [0u8; 4];
        OsRng.fill_bytes(&mut w);
        self.writer = hex::encode(w);
        // A new writer deletes nothing another wrote.
        self.snapshots.clear();
        self.wrote.clear();
        Ok(w)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(RECORD);
        let tmp = dir.join(format!("{RECORD}.new"));
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(self)?)?;
        f.sync_all()?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(dir: &Path) -> Arc<Wallet> {
        Wallet::create(dir.display().to_string(), Network::Regtest, 42, None).unwrap()
    }

    /// The folder as the app keeps it: what a run writes copied in, what
    /// it deletes deleted.
    fn publish(w: &Wallet, outbox: &Path, folder: &Path, at: u64) -> Option<BackupRun> {
        let present = listing(folder);
        let BackupStep::Write { run } = w.prepare(outbox, &present, false, at).unwrap() else {
            return None;
        };
        for name in &run.write {
            fs::copy(outbox.join(name), folder.join(name)).unwrap();
        }
        for name in &run.delete {
            fs::remove_file(folder.join(name)).unwrap();
        }
        w.backup_published(run.generation).unwrap();
        Some(run)
    }

    fn listing(folder: &Path) -> Vec<FolderFile> {
        fs::read_dir(folder)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                FolderFile {
                    name: e.file_name().into_string().unwrap(),
                    bytes: Some(e.metadata().unwrap().len()),
                }
            })
            .collect()
    }

    fn names(folder: &Path) -> BTreeSet<String> {
        listing(folder).into_iter().map(|f| f.name).collect()
    }

    const DAY_MS: u64 = REFRESH_SECS * 1000;

    #[test]
    fn a_changed_wallet_is_backed_up_once_and_restores_with_its_contract_states() {
        let dir = tempfile::tempdir().unwrap();
        let (outbox, folder) = (dir.path().join("outbox"), dir.path().join("folder"));
        fs::create_dir_all(&folder).unwrap();
        let original = wallet(&dir.path().join("original"));
        let phrase = original.backup_phrase().unwrap();
        // A contract whose state is large: a blob of its own.
        licash_contracts::holder::seed_contract(
            &wallet_file(&original.dir),
            "http://hub.example",
            &licash_api::Fees::default(),
        )
        .unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7919 % 251) as u8).collect();
        let db = Connection::open(wallet_file(&original.dir)).unwrap();
        db.execute("UPDATE contracts SET state = ?1", [&big])
            .unwrap();
        drop(db);

        let at = 1_800_000_000_000;
        let first = publish(&original, &outbox, &folder, at).unwrap();
        assert_eq!(first.generation, at);
        assert_eq!(first.write.len(), 2, "a blob and the snapshot");
        assert!(first.write[0].starts_with("b-") && first.write[1].starts_with("s-"));
        // The snapshot holds no contract state: the blob does.
        let snapshot = fs::metadata(folder.join(&first.write[1])).unwrap().len();
        assert!(snapshot < 100_000, "{snapshot}");
        // Nothing changed: nothing written; looking through Bitcoin is no change.
        assert!(publish(&original, &outbox, &folder, at + 1).is_none());
        original.wallet.watch_silent(None, 100).unwrap();
        assert!(publish(&original, &outbox, &folder, at + 2).is_none());
        // A day on, written again, the blob not.
        let again = publish(&original, &outbox, &folder, at + DAY_MS).unwrap();
        assert_eq!(again.write.len(), 1);
        // A change: a new snapshot, still with the blob.
        fs::write(original.dir.join("contacts.json"), b"[]").unwrap();
        let changed = publish(&original, &outbox, &folder, at + DAY_MS + 1).unwrap();
        assert_eq!(changed.write.len(), 1);
        assert!(changed.delete.is_empty(), "three kept");
        // A fourth: the oldest snapshot goes, the blob still named stays.
        fs::write(original.dir.join("contacts.json"), b"[ ]").unwrap();
        let fourth = publish(&original, &outbox, &folder, at + DAY_MS + 2).unwrap();
        assert_eq!(fourth.delete, vec![first.write[1].clone()]);
        assert_eq!(names(&folder).len(), 4);

        // Restored from the folder alone, by the phrase.
        let found = find_backup(
            phrase.clone(),
            Network::Regtest,
            folder.display().to_string(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.generation, fourth.generation);
        assert_eq!((found.newer_unusable, found.writers), (0, 1));
        assert_eq!(found.made_at, fourth.generation / 1000);
        let restored = Wallet::from_backup(
            dir.path().join("restored").display().to_string(),
            phrase.clone(),
            Network::Regtest,
            folder.display().to_string(),
            found.generation,
            None,
        )
        .unwrap();
        assert_eq!(restored.wallet.address(), original.wallet.address());
        assert_eq!(
            fs::read(restored.dir.join("contacts.json")).unwrap(),
            b"[ ]"
        );
        let state: Vec<u8> = Connection::open(wallet_file(&restored.dir))
            .unwrap()
            .query_row("SELECT state FROM contracts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, big);
        // The folder's name tells the wallet's, and nothing of it.
        assert_eq!(
            backup_folder_name(phrase.clone(), Network::Regtest).unwrap(),
            first.folder
        );
        assert_eq!(original.backup_folder().unwrap(), first.folder);
        assert!(first.folder.starts_with("Licash-") && first.folder.len() == 7 + 16);
        // Its blob gone, no snapshot restores.
        fs::remove_file(folder.join(&first.write[0])).unwrap();
        assert_eq!(
            find_backup(phrase, Network::Regtest, folder.display().to_string()).unwrap(),
            None
        );
    }

    #[test]
    fn a_snapshot_cut_short_or_missing_its_blob_is_passed_over_for_an_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let (outbox, folder) = (dir.path().join("outbox"), dir.path().join("folder"));
        fs::create_dir_all(&folder).unwrap();
        let original = wallet(&dir.path().join("original"));
        let phrase = original.backup_phrase().unwrap();
        let older = publish(&original, &outbox, &folder, 1_000).unwrap();
        fs::write(original.dir.join("contacts.json"), b"[]").unwrap();
        let newer = publish(&original, &outbox, &folder, 2_000).unwrap();
        let path = folder.join(newer.write.last().unwrap());
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        // And one of a newer app, from another phone: told, not used.
        let keys = Keys::of(&seed_of(&phrase).unwrap(), "regtest");
        let mut future = keys.seal(SNAPSHOT, u64::MAX, [9; 4], b"later").unwrap();
        future[4] = VERSION + 1;
        fs::write(folder.join("s-future.lcb"), future).unwrap();
        let found = find_backup(
            phrase.clone(),
            Network::Regtest,
            folder.display().to_string(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (found.generation, found.newer_unusable, found.writers),
            (older.generation, 2, 2)
        );
        // Of another network, or another wallet: nothing there.
        assert_eq!(
            find_backup(phrase, Network::Signet, folder.display().to_string()).unwrap(),
            None
        );
        let other = wallet(&dir.path().join("other")).backup_phrase().unwrap();
        assert_eq!(
            find_backup(other, Network::Regtest, folder.display().to_string()).unwrap(),
            None
        );
    }

    #[test]
    fn an_interrupted_run_never_reuses_its_generation_nor_counts_as_published() {
        let dir = tempfile::tempdir().unwrap();
        let (outbox, folder) = (dir.path().join("outbox"), dir.path().join("folder"));
        fs::create_dir_all(&folder).unwrap();
        let w = wallet(&dir.path().join("w"));
        let BackupStep::Write { run } = w.prepare(&outbox, &[], false, 5_000).unwrap() else {
            panic!("a first backup");
        };
        // Copied in part, never published: written again, under a new generation.
        let name = run.write.last().unwrap();
        fs::write(folder.join(name), b"LCAB").unwrap();
        let again = publish(&w, &outbox, &folder, 5_000).unwrap();
        assert!(again.generation > run.generation);
        assert_eq!(again.delete, vec![name.clone()], "its own half a file");
        assert!(
            w.backup_published(again.generation).is_err(),
            "published once"
        );
        // A clock gone back: generations still rise past the folder's.
        fs::write(w.dir.join(RECORD), b"not json").unwrap();
        fs::write(w.dir.join("contacts.json"), b"[]").unwrap();
        let back = publish(&w, &outbox, &folder, 1).unwrap();
        assert!(back.generation > again.generation);
        // A new writer, its record lost: deletes nothing it did not write.
        assert!(back.delete.is_empty());
    }

    #[test]
    fn a_folder_that_lost_the_backup_is_written_to_again_one_of_unknown_lengths_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let (outbox, folder) = (dir.path().join("outbox"), dir.path().join("folder"));
        fs::create_dir_all(&folder).unwrap();
        let w = wallet(&dir.path().join("w"));
        licash_contracts::holder::seed_contract(
            &wallet_file(&w.dir),
            "http://hub.example",
            &licash_api::Fees::default(),
        )
        .unwrap();
        let db = Connection::open(wallet_file(&w.dir)).unwrap();
        db.execute("UPDATE contracts SET state = ?1", [vec![5u8; 100_000]])
            .unwrap();
        drop(db);
        let first = publish(&w, &outbox, &folder, 1_000).unwrap();
        // The folder emptied, the wallet unchanged: all written again.
        for name in &first.write {
            fs::remove_file(folder.join(name)).unwrap();
        }
        let again = publish(&w, &outbox, &folder, 2_000).unwrap();
        assert_eq!(again.write.len(), 2);
        // A folder telling no lengths: its blob never rewritten.
        let unknown: Vec<FolderFile> = listing(&folder)
            .into_iter()
            .map(|f| FolderFile { bytes: None, ..f })
            .collect();
        let BackupStep::Write { run } = w.prepare(&outbox, &unknown, true, 3_000).unwrap() else {
            panic!("forced");
        };
        assert_eq!(run.write.len(), 1, "the snapshot alone");
        assert!(run.write[0].starts_with("s-"));
    }

    #[test]
    fn a_wallet_busy_contracting_is_backed_up_later() {
        let dir = tempfile::tempdir().unwrap();
        let w = wallet(&dir.path().join("w"));
        let _held = w.contracting.lock().unwrap();
        let outbox = dir.path().join("outbox").display().to_string();
        assert_eq!(
            w.prepare_backup(outbox, vec![], false).unwrap(),
            BackupStep::Busy
        );
    }

    #[test]
    fn names_and_headers() {
        let w = [1, 2, 3, 4];
        assert_eq!(
            parse_snapshot(&snapshot_name(42, &w)),
            Some((42, "01020304"))
        );
        assert_eq!(parse_snapshot("s-42-0102.lcb"), None);
        assert_eq!(parse_snapshot("b-00.lcb"), None);
        let keys = Keys::of(&[7; 32], "regtest");
        let sealed = keys.seal(SNAPSHOT, 9, w, b"hello").unwrap();
        assert_eq!(sealed.len() as u64, sealed_len(5));
        let opened = keys.open(&sealed).unwrap();
        assert_eq!(
            (opened.kind, opened.generation, opened.plain.as_slice()),
            (SNAPSHOT, 9, &b"hello"[..])
        );
        // A header changed is refused: no renaming a snapshot's generation.
        let mut moved = sealed.clone();
        moved[21] ^= 1;
        assert!(keys.open(&moved).is_err());
        assert!(Keys::of(&[7; 32], "signet").open(&sealed).is_err());
    }
}
