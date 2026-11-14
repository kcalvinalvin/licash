//! The silent payment tweaks (BIP352) of the chain the hub follows, which
//! light wallets fetch to find what pays their silent payment addresses:
//! indexed from a first block on, chosen once, in a SQLite file of their
//! own. Deleting the file loses nothing but the index.

use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{ensure, Context};
use bitcoin::{hashes::Hash, secp256k1::PublicKey, BlockHash, Script};
use licash::spend::ChainProfile;
use licash_api::{BlockTweaks, Hex};
use licash_chain_client::{Follow, Source};
use licash_onchain::silent;
use rusqlite::{params, Connection, OptionalExtension};

/// The schema this index reads and writes, as SQLite's `user_version`.
const VERSION: i64 = 1;

const SCHEMA: &str = "
-- The first block indexed, chosen once.
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    first INTEGER NOT NULL
) STRICT;
-- The blocks of the best chain from the first on, each with its tweaks:
-- compressed points, one after the other.
CREATE TABLE blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL,
    tweaks BLOB NOT NULL
) STRICT;
";

/// A compressed point.
const POINT: usize = 33;

/// The index, in its file.
pub struct Tweaks(Mutex<Connection>);

impl Tweaks {
    /// The index at `path`, a new file or one a hub wrote.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let tables: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            ensure!(tables == 0, "{} is not a tweak index", path.display());
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else {
            ensure!(
                version == VERSION,
                "tweak index schema {version}, this hub reads {VERSION}"
            );
        }
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The first block indexed, once chosen.
    pub fn first(&self) -> anyhow::Result<Option<u32>> {
        let conn = self.conn();
        let first = conn.query_row("SELECT first FROM meta", [], |r| r.get(0));
        Ok(first.optional()?)
    }

    /// The newest block indexed.
    pub fn top(&self) -> anyhow::Result<Option<(u32, BlockHash)>> {
        let conn = self.conn();
        let top = conn
            .query_row(
                "SELECT height, hash FROM blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get::<_, [u8; 32]>(1)?)),
            )
            .optional()?;
        Ok(top.map(|(height, hash)| (height, BlockHash::from_byte_array(hash))))
    }

    fn hash_at(&self, height: u32) -> anyhow::Result<Option<BlockHash>> {
        let conn = self.conn();
        let hash = conn
            .query_row("SELECT hash FROM blocks WHERE height = ?1", [height], |r| {
                r.get::<_, [u8; 32]>(0)
            })
            .optional()?;
        Ok(hash.map(BlockHash::from_byte_array))
    }

    fn begin(&self, first: u32) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute("INSERT INTO meta (one, first) VALUES (1, ?1)", [first])?;
        Ok(())
    }

    fn add(&self, height: u32, hash: &BlockHash, tweaks: &[PublicKey]) -> anyhow::Result<()> {
        let points: Vec<u8> = tweaks.iter().flat_map(|t| t.serialize()).collect();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO blocks (height, hash, tweaks) VALUES (?1, ?2, ?3)",
            params![height, hash.to_byte_array(), points],
        )?;
        Ok(())
    }

    /// Forget the blocks from `height` up, which left the best chain.
    fn rewind(&self, height: u32) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM blocks WHERE height >= ?1", [height])?;
        Ok(())
    }

    /// The tweaks of the `count` blocks from height `from`, of those
    /// indexed.
    pub fn blocks(&self, from: u32, count: u32) -> anyhow::Result<Vec<BlockTweaks>> {
        let conn = self.conn();
        let mut query = conn.prepare(
            "SELECT height, hash, tweaks FROM blocks
             WHERE height >= ?1 AND height < ?1 + ?2 ORDER BY height",
        )?;
        let rows = query.query_map(params![from, count], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, [u8; 32]>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut blocks = Vec::new();
        for row in rows {
            let (height, hash, points) = row?;
            ensure!(
                points.len() % POINT == 0,
                "block {height}: tweaks of bad length"
            );
            blocks.push(BlockTweaks {
                height,
                hash: BlockHash::from_byte_array(hash).to_string(),
                tweaks: points.chunks(POINT).map(|p| Hex(p.to_vec())).collect(),
            });
        }
        Ok(blocks)
    }
}

/// Index the tweaks of `source`'s best chain into `tweaks`, a round every
/// `every`, for good, if it follows `network`: from block `from` on, or
/// from the tip, once, when the index is new.
pub async fn index(
    tweaks: Arc<Tweaks>,
    source: Source,
    network: ChainProfile,
    from: Option<u32>,
    every: Duration,
) {
    let profile = loop {
        match source.profile().await {
            Ok(profile) => break profile,
            Err(e) => tracing::debug!(chain = %source, "indexing tweaks: {e:#}"),
        }
        tokio::time::sleep(every).await;
    };
    if profile != network {
        // The scan alerts of it.
        return tracing::error!(chain = %source, %profile, %network, "not indexing tweaks of another network");
    }
    tracing::info!(chain = %source, "indexing silent payment tweaks");
    match (tweaks.first(), from) {
        (Ok(Some(first)), Some(from)) if first != from => tracing::warn!(
            first,
            from,
            "indexing tweaks from the first block chosen: delete tweaks.sqlite to index from another"
        ),
        _ => {}
    }
    let mut failing: Option<String> = None;
    loop {
        match step(&tweaks, &source, from).await {
            Ok(()) => {
                if failing.take().is_some() {
                    tracing::info!(chain = %source, "indexing tweaks again");
                }
            }
            Err(e) => {
                let text = format!("{e:#}");
                if failing.as_ref() != Some(&text) {
                    tracing::warn!(chain = %source, "indexing tweaks: {text}");
                }
                failing = Some(text);
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// One round: index what `source`'s best chain has beyond the index, up
/// to its tip, rewinding first where it left the index. A new index
/// starts at block `from`, else at the tip.
pub async fn step(tweaks: &Tweaks, source: &Source, from: Option<u32>) -> anyhow::Result<()> {
    let tip = source.tip().await.context("the chain's tip")?;
    let first = match tweaks.first()? {
        Some(first) => first,
        None => {
            let first = from.unwrap_or(tip.height);
            tweaks.begin(first)?;
            tracing::info!(first, "indexing silent payment tweaks");
            first
        }
    };
    loop {
        let top = tweaks.top()?;
        let (next, block) = match source.follow(top, first, |h| tweaks.hash_at(h)).await? {
            Follow::Next(next, block) => (next, block),
            Follow::Rewind(height) => {
                tweaks.rewind(height)?;
                continue;
            }
            Follow::AtTip => break,
        };
        let hash = block.block_hash();
        let spends = source.taproot_spends(&block).await?;
        let found: Vec<PublicKey> = block
            .txdata
            .iter()
            .zip(&spends)
            .filter_map(|(tx, spent)| {
                let scripts: Vec<&Script> = spent.as_ref()?.iter().map(|s| s.as_script()).collect();
                silent::tweak(tx, &scripts)
            })
            .collect();
        tweaks.add(next, &hash, &found)?;
        tracing::debug!(height = next, tweaks = found.len(), "indexed tweaks");
    }
    Ok(())
}
