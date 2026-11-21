//! A wallet's extra codes: codes to be paid in notes at, each of its own
//! keys, derived from the wallet's seed and its number, so that no two of
//! them, nor any of them and the wallet's own code, show one owner. One
//! for each pseudonym, say.
//!
//! An extra code names no mailbox: it is paid at its hubs' inboxes only,
//! where a payment waits [`licash_api::inbox::KEPT_FOR`] at most. Each is
//! kept by a wallet of its own, an ordinary wallet file beside this one
//! (`<file>.codes/<number>.sqlite`) of a seed derived from this wallet's,
//! which collects what is paid to it ([`Wallet::collect_codes`]) and pays
//! it at once to this wallet's own code, less its hub's fee: so this
//! wallet's notes are all of its own keys, as every build of it takes
//! them, and a build without extra codes never opens their files.
//!
//! The codes made are listed in this wallet's file (`codes`, version
//! [`VERSION`]), made as the first is: a file of another version of it has
//! no extra codes for this build. A code listed whose file is missing (the
//! wallet restored from a backup, say) has it made again, its hubs' inboxes
//! looked through from their start.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use licash::{
    address::{Address, OwnerKeys},
    crypto::tagged_hash,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::{hub_url, Error, Paid, PaymentCode, PaymentNote, Received, Sent, Transport, Wallet};

/// The most characters of a code's label.
pub const MAX_LABEL: usize = 40;

const COMPONENT: &str = "codes";
const VERSION: i64 = 1;

/// No foreign keys, indexes or triggers: nothing an earlier build writing
/// the wallet's tables could ever trip over.
const SCHEMA: &str = "
-- The wallet's extra codes, by number, from 1: what its owner calls each.
CREATE TABLE codes (
    idx INTEGER PRIMARY KEY,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
";

/// An extra code of the wallet ([`Wallet::add_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraCode {
    /// Its number, from 1: the same code again, from the same seed.
    pub index: u32,
    pub label: String,
    pub created_at: i64,
    /// The address it is paid at, which no other code of the wallet shares.
    pub address: Address,
}

/// What [`Wallet::collect_codes`] did.
#[derive(Debug, Default)]
pub struct CodesCollected {
    /// The payments taken at each code, by its number.
    pub received: Vec<(u32, Received)>,
    /// What each code paid this wallet of what it held, by its number.
    pub swept: Vec<(u32, Paid)>,
    /// The codes not looked through, or whose notes were not paid on, and
    /// why: looked at again next time.
    pub failed: Vec<(u32, String)>,
}

/// What happened at an extra code ([`Wallet::code_activity`]).
#[derive(Clone, Debug)]
pub struct CodeActivity {
    pub code: ExtraCode,
    /// The payments taken there, newest first.
    pub received: Vec<Received>,
    /// What their notes say, by payment id in hex.
    pub notes: BTreeMap<String, PaymentNote>,
    /// Its payments to this wallet, of what it was paid: each, by its id,
    /// taken by this wallet as a payment of that id.
    pub swept: Vec<Sent>,
}

impl<T: Transport> Wallet<T> {
    /// The wallet's extra codes, by number.
    pub fn codes(&self) -> Result<Vec<ExtraCode>, Error> {
        let rows: Vec<(i64, String, i64)> = self.store.read(|c| {
            if component_version(c)? != Some(VERSION) {
                return Ok(Vec::new());
            }
            let mut stmt = c.prepare("SELECT idx, label, created_at FROM codes ORDER BY idx")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        rows.into_iter()
            .map(|(index, label, created_at)| {
                let index = u32::try_from(index).map_err(crate::invalid)?;
                Ok(ExtraCode {
                    index,
                    label,
                    created_at,
                    address: self.code_keys(index)?.address(),
                })
            })
            .collect()
    }

    /// A new extra code, called `label` (its words one space apart, at most
    /// [`MAX_LABEL`] characters): the next number's, its wallet made and
    /// at this wallet's hubs, their inboxes looked through from where this
    /// wallet's are, as nothing was paid to it before.
    pub async fn add_code(&self, label: &str) -> Result<ExtraCode, Error> {
        let label = code_label(label)?;
        let index = self.store.write(|tx| {
            if !writable(tx)? {
                return Err(Error::Invalid(
                    "extra codes, the wallet's file keeping them as a later version does".into(),
                ));
            }
            let last: i64 =
                tx.query_row("SELECT COALESCE(MAX(idx), 0) FROM codes", [], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO codes (idx, label, created_at) VALUES (?1, ?2, ?3)",
                params![last + 1, label, licash_store::now()],
            )?;
            u32::try_from(last + 1).map_err(crate::invalid)
        })?;
        let wallet = self.code_wallet(index)?;
        for (url, _) in self.hubs()? {
            if let Err(e) = wallet.add_hub(&url).await {
                tracing::warn!(%url, index, "a hub of an extra code not added yet: {e}");
            }
        }
        // Looked through from where this wallet is: nothing paid it before.
        for (url, _) in wallet.hubs()? {
            if let Some(after) = self.inbox_after(&url)? {
                wallet.start_inbox_at(&url, after)?;
            }
        }
        let code = self.codes()?.into_iter().find(|c| c.index == index);
        code.ok_or_else(|| Error::Setup("an extra code made, then gone".into()))
    }

    /// The payment code of extra code `index` at the first
    /// [`crate::CODE_HUBS`] of `hubs`, as [`Wallet::payment_code`] says,
    /// naming no mailbox.
    pub fn extra_code(&self, index: u32, hubs: &[String]) -> Result<PaymentCode, Error> {
        let code = self.codes()?.into_iter().find(|c| c.index == index);
        let code = code.ok_or_else(|| Error::Invalid(format!("no extra code {index}")))?;
        self.code_of(code.address, false, hubs)
    }

    /// Collect what was paid to each extra code at its hubs, this wallet's,
    /// and pay what each holds to this wallet, at its hub, as a payment it
    /// collects there: [`Wallet::collect_inbox`] does, then takes it. Its hub's fee is taken from it: what holds no more than the
    /// fee waits for more. Once at a time: none while another collects.
    pub async fn collect_codes(&self) -> Result<CodesCollected, Error> {
        let mut collected = CodesCollected::default();
        let Some(_once) = Once::take(&self.collecting_codes) else {
            return Ok(collected);
        };
        let hubs = self.hubs()?;
        for code in self.codes()? {
            let index = code.index;
            if let Err(e) = self.collect_code(index, &hubs, &mut collected).await {
                tracing::warn!(index, "extra code not collected: {e}");
                collected.failed.push((index, e.to_string()));
            }
        }
        Ok(collected)
    }

    async fn collect_code(
        &self,
        index: u32,
        hubs: &[(String, licash_api::Info)],
        collected: &mut CodesCollected,
    ) -> Result<(), Error> {
        let wallet = self.code_wallet(index)?;
        // Its payments to this wallet under way, first: their notes held
        // until done.
        wallet.resume().await?;
        let added: Vec<String> = wallet.hubs()?.into_iter().map(|(url, _)| url).collect();
        for (url, _) in hubs {
            if !added.contains(url) {
                if let Err(e) = wallet.add_hub(url).await {
                    tracing::warn!(%url, index, "a hub of an extra code not added yet: {e}");
                }
            }
        }
        let inboxed = wallet.collect_own_inbox().await?;
        for (url, reason) in inboxed.failed {
            collected.failed.push((index, format!("{url}: {reason}")));
        }
        let taken = !inboxed.received.is_empty();
        collected
            .received
            .extend(inboxed.received.into_iter().map(|r| (index, r)));
        if taken {
            if let Err(e) = wallet.unlink().await {
                tracing::info!(index, "notes of an extra code not unlinked: {e}");
            }
        }
        for balance in wallet.balance()? {
            let url = balance.hub;
            let Some(amount) = sweepable(&wallet, &url, balance.available) else {
                continue;
            };
            let code = self.code_of(self.address(), false, std::slice::from_ref(&url))?;
            let code = code.licash.ok_or_else(|| Error::Setup("no code".into()))?;
            match wallet.pay_code(&url, &code, amount).await {
                Ok(paid) => {
                    // Not kept at the hub: taken here, as no inbox has it.
                    if !paid.kept {
                        match self.receive(&paid.payment) {
                            Ok(_) | Err(Error::AlreadyReceived) => {}
                            Err(e) => tracing::warn!(index, "an extra code's payment: {e}"),
                        }
                    }
                    collected.swept.push((index, paid));
                }
                Err(e) => collected.failed.push((index, format!("{url}: {e}"))),
            }
        }
        Ok(())
    }

    /// What happened at each extra code: the payments taken there, what
    /// their notes say, and what it paid this wallet.
    pub fn code_activity(&self) -> Result<Vec<CodeActivity>, Error> {
        let mut activity = Vec::new();
        for code in self.codes()? {
            if !self.code_file(code.index).exists() {
                activity.push(CodeActivity {
                    code,
                    received: Vec::new(),
                    notes: BTreeMap::new(),
                    swept: Vec::new(),
                });
                continue;
            }
            let wallet = self.code_wallet(code.index)?;
            let address = self.address();
            let swept = wallet.sends()?.into_iter();
            activity.push(CodeActivity {
                received: wallet.receipts()?,
                notes: wallet.payment_notes()?,
                swept: swept.filter(|s| s.payee == address).collect(),
                code,
            });
        }
        Ok(activity)
    }

    /// The owner's keys of extra code `index`.
    fn code_keys(&self, index: u32) -> Result<OwnerKeys, Error> {
        Ok(OwnerKeys::from_seed(&self.code_seed(index))?)
    }

    fn code_seed(&self, index: u32) -> [u8; 32] {
        tagged_hash(
            "licash/code/seed",
            &[&self.keys.codes, &index.to_be_bytes()],
        )
    }

    /// The owner's keys of the extra code at `address`, if one of this
    /// wallet's is.
    pub(crate) fn code_owner(&self, address: &Address) -> Result<Option<OwnerKeys>, Error> {
        let code = self.codes()?.into_iter().find(|c| c.address == *address);
        code.map(|c| self.code_keys(c.index)).transpose()
    }

    /// The file of the wallet of extra code `index`.
    fn code_file(&self, index: u32) -> PathBuf {
        let mut dir = self.store.path().as_os_str().to_owned();
        dir.push(".codes");
        PathBuf::from(dir).join(format!("{index}.sqlite"))
    }

    /// The wallet of extra code `index`, reaching its hubs by this
    /// wallet's transport: made if missing.
    fn code_wallet(&self, index: u32) -> Result<Wallet<Lent<'_, T>>, Error> {
        let path = self.code_file(index);
        let seed = self.code_seed(index);
        if path.exists() {
            return Wallet::open_with_seed(&path, &seed, Lent(&self.transport));
        }
        if let Some(dir) = path.parent() {
            private_dir(dir)?;
        }
        Wallet::create_without_seed(&path, &seed, self.network, Lent(&self.transport))
    }

    /// The last entry of the inbox of the hub at `url` the wallet looked
    /// through, if any.
    fn inbox_after(&self, url: &str) -> Result<Option<i64>, Error> {
        let url = hub_url(url)?;
        self.store.read(|c| {
            Ok(c.query_row(
                "SELECT h.inbox_after FROM hub_urls u JOIN hubs h ON h.id = u.hub
                 WHERE u.url = ?1",
                [url],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
        })
    }

    /// Look through the inbox of the hub at `url` after entry `after`, if
    /// the wallet never looked through it.
    fn start_inbox_at(&self, url: &str, after: i64) -> Result<(), Error> {
        let url = hub_url(url)?;
        self.store.write(|tx| {
            tx.execute(
                "UPDATE hubs SET inbox_after = ?2
                 WHERE inbox_after IS NULL AND id = (SELECT hub FROM hub_urls WHERE url = ?1)",
                params![url, after],
            )?;
            Ok(())
        })
    }
}

/// The most of `available` notes at the hub at `url` the wallet of `T` can
/// pay, its hub's fee on top: none if it cannot pay anything.
fn sweepable<T: Transport>(wallet: &Wallet<T>, url: &str, available: u64) -> Option<u64> {
    let fits = |amount: u64| matches!(wallet.send_fee(url, amount), Ok(fee) if amount.saturating_add(fee) <= available);
    let (mut low, mut high) = (0u64, available);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    (low > 0).then_some(low)
}

/// `label` as kept: its words one space apart, at most [`MAX_LABEL`]
/// characters, not empty.
pub(crate) fn code_label(label: &str) -> Result<String, Error> {
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        return Err(Error::Invalid("a code's label says nothing".into()));
    }
    if label.chars().count() > MAX_LABEL {
        return Err(Error::Invalid(format!(
            "a code's label of more than {MAX_LABEL} characters"
        )));
    }
    Ok(label)
}

/// Taken while held: once at a time.
struct Once<'a>(&'a AtomicBool);

impl<'a> Once<'a> {
    fn take(flag: &'a AtomicBool) -> Option<Self> {
        let free = flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        free.ok().map(|_| Self(flag))
    }
}

impl Drop for Once<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// `dir`, made readable by its owner only, if missing.
fn private_dir(dir: &std::path::Path) -> Result<(), Error> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| Error::Setup(format!("{}: {e}", dir.display())))
}

fn component_version(c: &Connection) -> Result<Option<i64>, Error> {
    licash_store::component_version(c, COMPONENT).map_err(stored)
}

fn stored(e: licash_store::Error) -> Error {
    match e {
        licash_store::Error::Sqlite(e) => Error::Storage(e),
        e => Error::Setup(e.to_string()),
    }
}

/// Make the codes' table in `tx`, if none: whether this build writes it.
fn writable(tx: &Transaction) -> Result<bool, Error> {
    match component_version(tx)? {
        Some(VERSION) => Ok(true),
        Some(_) => Ok(false),
        None => {
            licash_store::component_in(tx, COMPONENT, VERSION, SCHEMA).map_err(stored)?;
            Ok(true)
        }
    }
}

/// A wallet's transport lent: as the wallet of an extra code reaches its
/// hubs, by its wallet's ([`Wallet::collect_codes`]). Private, rather than
/// `Transport for &T`, which would claim the impl for every borrowed
/// transport downstream.
pub(crate) struct Lent<'a, T>(&'a T);

impl<T: Transport> Transport for Lent<'_, T> {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<crate::Response, String>> + Send {
        self.0.request(url, body)
    }

    fn download(
        &self,
        url: &str,
        max: usize,
    ) -> impl std::future::Future<Output = Result<crate::Response, String>> + Send {
        self.0.download(url, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_its_words_one_space_apart_and_says_something() {
        assert_eq!(code_label("  Alice \n online ").unwrap(), "Alice online");
        assert!(code_label(" \t ").is_err());
        assert!(code_label(&"x".repeat(MAX_LABEL)).is_ok());
        assert!(code_label(&"é".repeat(MAX_LABEL + 1)).is_err());
    }
}
