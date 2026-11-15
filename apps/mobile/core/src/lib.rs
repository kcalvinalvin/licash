//! The Licash wallet of the mobile apps: [`Wallet`], over the SDK's wallet,
//! with blocking methods for the apps to call off their main thread, and
//! Bitcoin read through the app's light client ([`ChainSource`]).
//!
//! What a user sees is notes at hubs, and coins paid on Bitcoin to the
//! wallet's silent payment address, which [`Wallet::move_to_licash`] turns
//! into notes. One payment code ([`Wallet::payment_code`]) takes both.

mod chain;
mod deliveries;
mod log;
mod wallet;

pub use chain::{ChainError, ChainSource};
pub use log::{set_logger, Logger};
pub use wallet::{wallet_exists, Wallet};

uniffi::setup_scaffolding!();

/// The networks a wallet can be of. Every one but Bitcoin is for testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Network {
    Bitcoin,
    Signet,
    Mutinynet,
    Regtest,
}

impl Network {
    fn profile(self) -> licash::spend::ChainProfile {
        use licash::spend::ChainProfile::*;
        match self {
            Self::Bitcoin => Core30Mainnet,
            Self::Signet => Core30Signet,
            Self::Mutinynet => Mutinynet,
            Self::Regtest => Core30DefaultRegtest,
        }
    }

    fn of(profile: licash::spend::ChainProfile) -> Self {
        use licash::spend::ChainProfile::*;
        match profile {
            Core30Mainnet => Self::Bitcoin,
            Core30Signet => Self::Signet,
            Mutinynet => Self::Mutinynet,
            Core30DefaultRegtest => Self::Regtest,
        }
    }
}

/// The name the light client knows `network` by.
#[uniffi::export]
pub fn chain_name(network: Network) -> String {
    match network {
        Network::Bitcoin => "bitcoin",
        Network::Signet => "signet",
        Network::Mutinynet => "mutinynet",
        Network::Regtest => "regtest",
    }
    .into()
}

/// What went wrong, as the app tells its user.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum LicashError {
    /// Not something the wallet can do: as the user asked it, never.
    #[error("{reason}")]
    Invalid { reason: String },
    /// A hub, relay or the chain could not be reached: later, perhaps.
    #[error("{reason}")]
    Unavailable { reason: String },
    /// Under way, and finished by [`Wallet::resume`].
    #[error("{reason}")]
    Pending { reason: String },
    #[error("{needed} sat needed, {available} sat available")]
    Insufficient { needed: u64, available: u64 },
    /// A hub at an onion address, and the wallet opened without Tor.
    #[error("an onion hub is reached through Tor: turn it on")]
    NeedsTor,
    /// A payment of notes at the hub at `url`, which the wallet does not
    /// trust: [`Wallet::receive`] it trusting the hub to take it.
    #[error("a payment at hub {url}, not trusted yet")]
    UnknownHub { url: String },
    /// The wallet's own storage failed.
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<licash_wallet::Error> for LicashError {
    fn from(e: licash_wallet::Error) -> Self {
        use licash_wallet::Error as E;
        let reason = e.to_string();
        match e {
            E::Unavailable { .. } => Self::Unavailable { reason },
            E::Pending { .. } | E::NoteTaken => Self::Pending { reason },
            E::Insufficient { needed, available } => Self::Insufficient { needed, available },
            E::UnknownHub(url) => Self::UnknownHub { url },
            E::Setup(_) | E::Storage(_) => Self::Failed { reason },
            _ => Self::Invalid { reason },
        }
    }
}

impl LicashError {
    /// `e`, a wallet's error if it is one, else as `other`.
    fn from_any(e: anyhow::Error, other: fn(String) -> Self) -> Self {
        match e.downcast::<licash_wallet::Error>() {
            Ok(e) => e.into(),
            Err(e) => other(format!("{e:#}")),
        }
    }

    fn invalid(reason: impl ToString) -> Self {
        Self::Invalid {
            reason: reason.to_string(),
        }
    }

    fn failed(reason: impl ToString) -> Self {
        Self::Failed {
            reason: reason.to_string(),
        }
    }
}

/// A hub the wallet trusts.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Hub {
    pub url: String,
    /// Its fee on a payment: `base` plus `ppm` millionths of it.
    pub fee_base: u64,
    pub fee_ppm: u64,
    /// The least and most it turns into notes from Bitcoin, if it does.
    pub deposits: Option<Range>,
    /// Whether it serves what finds silent payments.
    pub silent: bool,
    /// Whether it names relays for mailboxes.
    pub mailbox: bool,
    /// The most a Lightning invoice it pays may ask, in sat, if it pays
    /// them.
    pub lightning_pay: Option<u64>,
    /// Its fees on a lock, which holds notes until a condition is met (as
    /// paying a Lightning invoice does), and on refunding one.
    pub lock_fee: Rate,
    pub refund_fee: Rate,
    /// On top of the notes moving Bitcoin into them makes, if it does.
    pub deposit_fee: Option<Rate>,
    /// Paying and being paid over Lightning, if it does.
    pub lightning: Option<LightningTerms>,
    /// The recovery contracts it opens now, if it does.
    pub contracts: Option<ContractTerms>,
}

/// The recovery contracts a hub opens, as its info says. A contract locks
/// the hub's collateral on Bitcoin behind the wallet's notes there, up to
/// `cap`: if the hub fails them, the wallet recovers them from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ContractTerms {
    /// The most of the notes at the hub one protects, in sat.
    pub cap: u64,
    /// What opening one costs, in sat, paid from a coin on Bitcoin.
    pub price: u64,
    /// Blocks from opening one to its close, at most.
    pub blocks: u32,
    /// Under the mock authority, for regtest only: such a contract
    /// protects nothing.
    pub mock: bool,
}

/// A fee, in sat: `base` plus `ppm` millionths of what it is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Rate {
    pub base: u64,
    pub ppm: u64,
}

/// What a hub charges over Lightning, and what it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LightningTerms {
    /// On top of an invoice it pays, routing included.
    pub pay: Rate,
    /// On top of what an invoice it makes pays the wallet: the payer's.
    pub receive: Rate,
    /// What an invoice it makes may pay the wallet: at least and at most.
    pub receive_range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Range {
    pub min: u64,
    pub max: u64,
}

/// A hub announced on Nostr, not used yet.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FoundHub {
    /// The name it announced itself under, if any.
    pub name: Option<String>,
    /// What it says of itself, at the URL it answered at.
    pub hub: Hub,
}

/// What looking for hubs on Nostr found ([`Wallet::find_hubs`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HubsFound {
    /// Those answering, not used yet.
    pub hubs: Vec<FoundHub>,
    /// How many others were announced that answered at none of their URLs:
    /// down, refusing the route, or reached only through Tor.
    pub silent: u32,
    /// How many of those are onion services only, reached through Tor only.
    pub silent_onion: u32,
}

/// The relays to look for hubs and swap providers on besides those the
/// hubs used name: relays that serve readers over Tor, the onion services
/// among them only if `tor`. None on regtest, whose services are local.
#[uniffi::export]
pub fn default_relays(network: Network, tor: bool) -> Vec<String> {
    licash_nostr::default_relays(network.profile().name(), tor)
}

/// What the wallet holds, in sat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct Balance {
    /// In notes, to pay with.
    pub notes: u64,
    /// In notes under way: change of a payment not finished.
    pub notes_pending: u64,
    /// Paid on Bitcoin, confirmed, to move to notes but `kept`.
    pub onchain: u64,
    /// Paid on Bitcoin and being moved to notes.
    pub moving: u64,
    /// Of `onchain`, what the wallet's own payouts paid it, and the change
    /// of its contracts' funding: cashed out to the wallet, and not moved
    /// back to notes unless asked.
    pub kept: u64,
    /// On Bitcoin, in the funding of contracts being opened: what comes
    /// back to the wallet once it confirms, their price paid.
    pub opening: u64,
    /// Of `notes` and `notes_pending`, what contracts protect: at each
    /// hub, up to the caps of those live there. The rest is on credit.
    pub covered: u64,
}

/// A recovery contract with a hub, as the wallet keeps it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Contract {
    /// Its id, in hex.
    pub id: String,
    pub hub: String,
    pub state: ContractState,
    /// Under the mock authority, for regtest only: it protects nothing.
    pub mock: bool,
    /// The most of the notes at its hub it protects, in sat.
    pub cap: u64,
    /// What opening it cost, in sat, from the coin it was paid from.
    pub price: u64,
    /// That coin's value, in sat: the rest came back as change.
    pub paid_from: u64,
    /// The block from which it protects nothing: the last a recovery
    /// can start by.
    pub protects_until: u32,
    /// The block from which its collateral returns to the hub: closed.
    pub closes_at: u32,
    /// The transaction funding it, once known, its id in hex.
    pub funding: Option<String>,
    /// When it was asked for, in seconds since the Unix epoch.
    pub created_at: i64,
}

/// How far a payout has come, step by step, as the wallet last saw it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PayoutSteps {
    /// The wallet's notes locked for the provider at the hub: none before.
    pub lock: Option<LockStep>,
    /// The block from which the hub gives the locked notes back unpaid.
    pub lock_expiry: Option<u32>,
    /// The block from which the provider may take its coin back.
    pub timeout: Option<u32>,
    /// The last block the wallet claims the provider's coin at: past it,
    /// the payout fails and the notes come back once the lock expires.
    pub claim_by: Option<u32>,
    /// The provider's coin on Bitcoin, as found or as the provider told
    /// of it, maybe not confirmed yet; its block once in one.
    pub coin: Option<String>,
    pub coin_height: Option<u32>,
    /// The wallet's claim of the coin, paying the address, once made; its
    /// block once in one.
    pub claim: Option<String>,
    pub claim_height: Option<u32>,
    /// The confirmations awaited: of the coin before it is claimed, of the
    /// claim before the payout is done.
    pub depth: u32,
    /// The chain's height as the wallet last saw it: 0 if never.
    pub tip: u32,
}

/// Where the notes a payout locked stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LockStep {
    /// Asked of the hub, not certified yet.
    Requested,
    /// Certified: the provider is paid once the wallet claims its coin.
    Locked,
    /// Paid to the provider.
    Paid,
    /// Given back to the wallet, unpaid.
    Refunded,
    /// Never made: the notes stayed the wallet's.
    Abandoned,
}

/// Where a contract stands.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ContractState {
    /// Being opened with its hub.
    Opening,
    /// Signed and broadcast, its funding not confirmed yet.
    Confirming,
    /// Protecting the notes at its hub, up to its cap.
    Protecting,
    /// Past [`Contract::protects_until`]: protecting nothing, its
    /// collateral back to its hub at its close.
    Ending,
    /// Closed: its collateral back to its hub.
    Closed,
    /// Never opened, so nothing paid: `reason`.
    Failed { reason: String },
}

/// What opening a contract at a hub costs now, and what pays it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ContractQuote {
    pub hub: String,
    /// In sat, from one coin of the wallet's on Bitcoin.
    pub price: u64,
    /// The most of the notes at the hub it protects, in sat.
    pub cap: u64,
    /// Blocks from now to its close.
    pub blocks: u32,
    /// Blocks from now it protects for: until its last block to recover in.
    pub protects_for: u32,
    /// Under the mock authority, for regtest only: it protects nothing.
    pub mock: bool,
    /// The value of the coin that would pay it, if one does: the rest
    /// comes back as change.
    pub coin: Option<u64>,
    /// What opening it downloads and keeps, in bytes, about.
    pub download: u64,
}

/// What the wallet holds at one hub it trusts, in sat.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Holding {
    pub hub: String,
    /// To pay with.
    pub available: u64,
    /// Held by payments under way.
    pub pending: u64,
}

/// How many notes of one denomination the wallet can pay with at a hub.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Notes {
    pub hub: String,
    pub denomination: u64,
    pub count: u64,
}

/// A Lightning invoice to pay through `hub`: `amount` it asks, and `fee`,
/// all paying it costs on top, with the notes held now.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct InvoiceQuote {
    pub hub: String,
    pub amount: u64,
    pub fee: u64,
    pub description: Option<String>,
}

/// Notes paid out to `address`, on Bitcoin, through a swap provider: what
/// the address gets, `amount`, from notes at `hub`, and `fee`, all it costs
/// on top: the miners of the claim of the provider's coin, the provider's
/// fee, its coin's miners in it, and the hub's ([`Wallet::payout_quote`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PayoutQuote {
    pub address: String,
    pub amount: u64,
    pub fee: u64,
    pub hub: String,
    /// The provider, by its URL, and the name it announced, if any.
    pub provider: String,
    pub provider_name: Option<String>,
    /// The blocks confirming the provider's coin before the wallet claims
    /// it: the claim reaches the address after.
    pub depth: u32,
}

/// What moving notes from one of the wallet's hubs to another costs
/// through `provider` ([`Wallet::move_offers`]): `amount` reaching `to`,
/// and `fee` on top, the provider's and `lock_fee`, the hub's at `from`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MoveQuote {
    pub from: String,
    pub to: String,
    pub amount: u64,
    pub fee: u64,
    pub lock_fee: u64,
    /// Blocks past the usual the notes stay locked at `from` should the
    /// provider never lock to the wallet at `to`.
    pub margin: u32,
    /// The provider, by its URL, and the name it announced, if any.
    pub provider: String,
    pub provider_name: Option<String>,
}

/// How a payment code is paid in notes ([`Wallet::pay_quote`]): `amount`
/// at `hub`, with `fee`, all it costs on top: the hub's, and on a swap
/// bringing notes there first, `swapped` from `swap_from`, the provider's
/// and the lock's.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PayQuote {
    pub hub: String,
    pub amount: u64,
    pub fee: u64,
    pub swap_from: Option<String>,
    pub swapped: u64,
    /// Whether the wallet trusts `hub`, else only visited it, the payee's
    /// hub proving it is the one their code names: what the swap brings
    /// past the payment stays there, apart ([`Wallet::visited`]).
    pub trusted: bool,
}

/// A way of paying a payment code ([`Wallet::pay_offers`]): through the
/// swap provider at `provider`, if a swap brings what the wallet lacks at
/// the payee's hub first.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PayOffer {
    pub provider: Option<String>,
    /// The name it announced itself under, if any.
    pub provider_name: Option<String>,
    /// Whether the wallet swapped through it before.
    pub known: bool,
    pub quote: PayQuote,
}

/// A payment code to show: `text` to share, `qr` to draw, `address`, the
/// plain address on Bitcoin in it, for payers that take no codes, and
/// `silent`, its silent payment address, if a hub trusted finds payments
/// to it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Code {
    pub text: String,
    pub qr: String,
    pub address: Option<String>,
    pub silent: Option<String>,
}

/// What a text scanned or pasted is.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Scanned {
    /// A payment code: to pay, `amount` if it says.
    Code {
        amount: Option<u64>,
        /// Whether it names a hub the wallet trusts, to pay in notes.
        notes: bool,
    },
    /// A payment code naming no hub the wallet trusts that says where to
    /// reach its hubs: to pay in notes at one, visited, proving it is the
    /// one named, if the owner agrees ([`Wallet::pay_quote`]), else on
    /// Bitcoin at `address`, if it names one; `amount` if it says.
    Visit {
        amount: Option<u64>,
        address: Option<String>,
    },
    /// An address on Bitcoin, or a code naming one and no hub the wallet
    /// trusts: to pay out to from notes, through a swap provider
    /// ([`Wallet::payout_quote`]), `amount` if it says.
    Bitcoin {
        address: String,
        amount: Option<u64>,
    },
    /// A payment handed over, to [`Wallet::receive`].
    Payment,
    /// A Lightning invoice, to pay through a hub: what it asks.
    Invoice {
        amount: u64,
        description: Option<String>,
    },
}

/// A Lightning invoice the wallet asked a hub for, to be paid with
/// ([`Wallet::ask_invoice`]), and where it stands.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LightningInvoice {
    /// Its payment hash, in hex: to follow it by.
    pub hash: String,
    pub hub: String,
    /// What it pays the wallet, in notes.
    pub amount: u64,
    /// The BOLT11 invoice to hand the payer, once its hub made it, and
    /// what it asks of them: `amount` and the hub's fee.
    pub request: Option<String>,
    pub asks: Option<u64>,
    /// Done once its notes are taken.
    pub state: State,
    /// Why it was canceled, or why it waits on its hub, if it does.
    pub reason: Option<String>,
}

/// The notes one line of the history spent, and those it made the
/// wallet's ([`Wallet::notes_of`]), the largest first, and whom those spent
/// paid.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct NotesOf {
    pub spent: Vec<NoteLine>,
    pub made: Vec<NoteLine>,
    /// What those spent pay, part by part: its payee, its hub's fee, and
    /// what the wallet keeps, the notes kept among `made` once made.
    pub paid: Vec<NotesPaid>,
}

/// Part of what the notes a line of the history spent pay.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NotesPaid {
    pub to: PaidTo,
    pub hub: String,
    pub amount: u64,
    /// The amounts of the notes made for it, the largest first.
    pub notes: Vec<u64>,
}

/// Whom a part of a payment pays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PaidTo {
    /// Its payee: a person, a provider, or the hub paying an invoice.
    Payee,
    /// The wallet itself, as payee: notes of the amounts it needs.
    Itself,
    /// Its hub, as its fee.
    Hub,
    /// The wallet, as its change or what a refund returned.
    Kept,
}

/// A note the wallet holds or held.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NoteLine {
    /// Its identifier at its hub, in hex: the same once unlinked.
    pub id: String,
    pub hub: String,
    pub amount: u64,
    /// The payment, lock or receipt that made it, in hex.
    pub made_by: String,
    /// Made by a payment of the wallet to itself, for notes of the
    /// amounts it needed: not a line of the history.
    pub made_by_itself: bool,
    /// The payment or lock that spends it, in hex, if any.
    pub spent_by: Option<String>,
    pub linking: Linking,
    /// Spent by a copy of the wallet, as its hub showed: never here.
    pub gone: bool,
    /// When the wallet got it, in seconds since the Unix epoch.
    pub added_at: i64,
}

/// Whether a note's hub can tell which payment it came from when it is
/// spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Linking {
    /// As issued: its hub can tell. The wallet unlinks it next time.
    Linked,
    /// Signed again blindly by its hub: it cannot tell.
    Unlinked,
    /// Its hub refused to unlink it: it stays linked.
    StaysLinked,
}

/// A payment made.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Paid {
    pub id: String,
    pub amount: u64,
    pub hub: String,
    /// Sent to the payee's mailbox.
    pub delivered: bool,
    /// Else the payment, for the payee to receive by hand.
    pub handover: Option<String>,
    /// Why it was not delivered.
    pub undelivered: Option<String>,
}

/// One line of the wallet's history.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Activity {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    /// In sat, fees aside.
    pub amount: u64,
    pub fee: u64,
    /// The hub, or why it failed.
    pub detail: String,
    /// When, in seconds since 1970, if known.
    pub at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Kind {
    Sent,
    Received,
    /// Bitcoin moved to notes.
    Moved,
    /// A coin paid on Bitcoin, not moved yet.
    Onchain,
    /// A Lightning invoice paid through a hub.
    Lightning,
    /// Paid over Lightning: an invoice the wallet asked a hub for.
    Invoice,
    /// Notes paid out to an address on Bitcoin, through a swap provider.
    Payout,
    /// A recovery contract opened at a hub: its price, paid on Bitcoin
    /// from a coin of the wallet's, the rest back to it.
    Contract,
    /// Notes moved from one of the wallet's hubs to another, through a
    /// swap provider: `amount` reaching the other hub, `fee` on top.
    Swap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum State {
    Done,
    Pending,
    Failed,
}

/// More of one line of the history ([`Wallet::details`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Details {
    /// A payment in notes, at `hub`.
    Notes { hub: String },
    /// Bitcoin moved to notes: a deposit at `hub`, by its `hash`.
    Moved {
        hub: String,
        hash: String,
        /// What to pay on Bitcoin, the hub's fee in it, and where, once
        /// the hub answered.
        pays: Option<u64>,
        address: Option<String>,
        /// The payment, once made or seen.
        payment: Option<Onchain>,
        /// The confirmations the hub waits for, as it says now.
        depth: Option<u32>,
        /// The block its payment must confirm by, and the one after which
        /// a payment whose notes were never claimed is the wallet's again.
        fund_by: Option<u32>,
        timeout: Option<u32>,
        /// The transaction taking it back, if one did.
        refund: Option<String>,
    },
    /// A Lightning invoice paid through `hub`: the `preimage` its payment
    /// bought once paid, or why it was not.
    Lightning {
        hub: String,
        invoice: String,
        preimage: Option<String>,
        failure: Option<String>,
    },
    /// Paid over Lightning, through `hub`: its `invoice`, once the hub
    /// made it, and what it `asks` of the payer, the hub's fee on top; why
    /// it was canceled, if it was.
    Invoice {
        hub: String,
        invoice: Option<String>,
        asks: Option<u64>,
        failure: Option<String>,
    },
    /// Notes moved from hub `from` to hub `to` through `provider`: the
    /// wallet's lock at `from` once made, the provider's at `to` once made;
    /// why it failed, if it did, the notes locked back once the lock
    /// expires.
    Swap {
        from: String,
        to: String,
        provider: String,
        lock: Option<String>,
        theirs: Option<String>,
        failure: Option<String>,
    },
    /// Notes at `hub` paid out to `address`, on Bitcoin, through
    /// `provider`: the transaction of its coin, once the wallet found it,
    /// and of the wallet's claim paying the address, once made; why it
    /// failed, if it did, its notes back once the lock expires. `progress`
    /// tells each step.
    Payout {
        hub: String,
        provider: String,
        address: String,
        coin: Option<String>,
        claim: Option<String>,
        failure: Option<String>,
        progress: PayoutSteps,
    },
    /// A recovery contract opened at `hub`, by its `funding`'s id.
    Contract { hub: String, funding: String },
    /// A coin on Bitcoin, output `vout` of `tx`: `change` of the wallet's
    /// own, and `moving`, the transaction spending it, if one is.
    Coin {
        tx: Onchain,
        vout: u32,
        change: bool,
        moving: Option<String>,
    },
}

/// A transaction on Bitcoin, as far as the wallet looked.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Onchain {
    pub txid: String,
    /// Paid to its miners, if the wallet knows.
    pub fee: Option<u64>,
    /// The block it is in, if in one.
    pub height: Option<u32>,
    /// How many blocks confirm it: 0 while in none.
    pub confirmations: u32,
}

/// The block explorer of `network`, to show a transaction at
/// `{explorer}/tx/{txid}`: none for regtest.
#[uniffi::export]
pub fn explorer(network: Network) -> Option<String> {
    match network {
        Network::Bitcoin => Some("https://mempool.space".into()),
        Network::Signet => Some("https://mempool.space/signet".into()),
        Network::Mutinynet => Some("https://mutinynet.com".into()),
        Network::Regtest => None,
    }
}

/// How far looking for payments on Bitcoin got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Scan {
    /// The next block to look in.
    pub next: u32,
    /// The chain's height, as far as it could look.
    pub height: u32,
    /// The coins found, and their worth.
    pub found: u32,
    pub found_value: u64,
}
