//! What the wallet did and what it cost, for the Stats screen: one flat
//! record for each thing that happened, with its own fee and time and
//! nothing grouped by period (the app does that, in its own time zone).
//!
//! The history's lines are the one definition of an amount and a fee, so
//! the two never disagree; this adds what the history hides (renewals and
//! exact-note payments, which cost fees), what it cannot say (the coins
//! paid on Bitcoin that are spent now, when each operation ended, the
//! parts of a fee) and the balance at past times, replayed from the
//! records. Nothing here writes, and nothing is stored: a wallet from an
//! earlier version has all of it at once.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, PoisonError},
};

use bitcoin::Txid;
use licash_wallet::{Journal, LockState};

use super::Wallet;
use crate::{Activity, ChainSource, Kind, LicashError, State};

/// How a payment was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Way {
    /// In notes.
    Licash,
    /// Over Lightning, through a hub or a swap provider.
    Lightning,
    /// On Bitcoin.
    Bitcoin,
}

/// What a thing was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Flow {
    /// Money that came to the wallet.
    Received,
    /// Money that left it to someone else.
    Sent,
    /// Money the wallet moved between its own places: Bitcoin into notes, or
    /// notes from one hub to another.
    Moved,
    /// Notes reminted at a hub to a better shape.
    Optimized,
    /// Notes renewed, since their hub restarted and no longer unlinks them.
    Renewed,
    /// Notes paid to the wallet itself for the amounts a payment needed.
    Normalized,
    /// A recovery contract bought.
    Protection,
}

/// One thing that happened.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StatEvent {
    /// Its id in the history, in hex, or the coin's transaction.
    pub id: String,
    pub flow: Flow,
    pub way: Way,
    pub state: State,
    /// When it began, or came, in seconds since 1970: the history's time.
    /// None for a coin paid on Bitcoin whose block's time is not known.
    pub at: Option<i64>,
    /// Seconds from its beginning to its end, for what the wallet carries
    /// out itself and that was done: none for what waits for another.
    pub took: Option<u64>,
    /// In sat, fees aside: what was paid or received, the notes reminted,
    /// or what a contract protects at most.
    pub amount: u64,
    /// What it cost the wallet, in sat, once settled: none while under way
    /// and for what failed, but a Lightning payment refunded, which kept the
    /// cost of its refund.
    pub fee: u64,
    /// Of `fee`, the hub's, as recorded: none if not told apart.
    pub hub_fee: Option<u64>,
    /// Of `fee`, what a swap provider took, its miners' fee included.
    pub provider_fee: Option<u64>,
    /// Of `fee`, the claim's on Bitcoin, as budgeted.
    pub network_fee: Option<u64>,
}

/// Notes that finished unlinking.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Unlink {
    /// When, in seconds since 1970.
    pub at: i64,
    /// Seconds from when the notes came.
    pub waited: u64,
    pub amount: u64,
}

/// All of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct Stats {
    pub events: Vec<StatEvent>,
    pub unlinks: Vec<Unlink>,
}

/// What the wallet held at a time, in sat.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BalancePoint {
    pub at: i64,
    /// In notes, at every hub, under way included.
    pub licash: u64,
    /// In coins on Bitcoin.
    pub bitcoin: u64,
    /// Of `licash`, at each hub, by URL.
    pub hubs: Vec<HubAmount>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HubAmount {
    pub hub: String,
    pub amount: u64,
}

/// What is known of the operations the history's lines come from.
struct Records {
    journal: Journal,
    locks: HashMap<String, licash_wallet::Lock>,
    payouts: HashMap<String, licash_wallet::Payout>,
    swaps: HashMap<String, licash_wallet::Swap>,
    deposits: HashMap<String, licash_wallet::Deposit>,
    /// Transactions of the wallet's own coins: spending a coin of its own.
    spends: HashSet<Txid>,
    /// Contracts, by id: what each protects and costs.
    contracts: HashMap<String, (u64, u64)>,
    /// The ids of the receipts that are no payment to the wallet: the
    /// notes a swap brought it, and the notes a contract's hub issued it
    /// of her own coin. They are the money of another event.
    own: HashSet<String>,
}

/// Statistics.
#[uniffi::export]
impl Wallet {
    /// Everything that happened, newest first as the history is.
    /// Blocks' times are asked of `chain` where not known yet.
    pub fn stats(&self, chain: Arc<dyn ChainSource>) -> Result<Stats, LicashError> {
        let _timed = crate::log::timed("stats");
        let contracts = self.contracts()?;
        let records = Records {
            journal: self.wallet.journal()?,
            locks: self
                .wallet
                .locks()?
                .into_iter()
                .map(|l| (l.id.clone(), l))
                .collect(),
            payouts: self
                .wallet
                .payouts()?
                .into_iter()
                .map(|p| (p.hash.clone(), p))
                .collect(),
            swaps: self
                .wallet
                .swaps()?
                .into_iter()
                .map(|s| (s.hash.clone(), s))
                .collect(),
            deposits: self
                .wallet
                .deposits()?
                .into_iter()
                .map(|d| (d.hash.clone(), d))
                .collect(),
            spends: HashSet::new(),
            contracts: contracts
                .iter()
                .map(|c| (c.id.clone(), (c.cap, c.price)))
                .collect(),
            own: HashSet::new(),
        };
        let spends = records
            .journal
            .coins
            .iter()
            .filter_map(|c| c.spent_by)
            .collect();
        let history = self.history()?;
        let mut own: HashSet<String> = records
            .swaps
            .values()
            .filter_map(|s| s.theirs.clone())
            .collect();
        own.extend(issued_receipts(&history, &contracts));
        let records = Records {
            spends,
            own,
            ..records
        };
        let mut events = Vec::new();
        for line in history {
            if let Some(event) = line_event(&line, &records) {
                events.push(event);
            }
        }
        events.extend(self.hidden_sends(&records)?);
        events.extend(self.coin_events(&records, chain.as_ref())?);
        let unlinks = records
            .journal
            .notes
            .iter()
            .filter_map(|n| {
                let at = n.unlinked_at?;
                Some(Unlink {
                    at,
                    waited: at.saturating_sub(n.added_at).max(0) as u64,
                    amount: n.amount,
                })
            })
            .collect();
        Ok(Stats { events, unlinks })
    }

    /// What the wallet held at each of `instants` (seconds since 1970),
    /// replayed from when its notes came and were spent and its coins were
    /// found and spent. A note counts from its arrival until what spent
    /// it ended; a coin from the time of its block until the block that
    /// spent it. A moment between a coin's spend and the notes it made is
    /// in neither: about, not exact.
    pub fn balance_over(
        &self,
        instants: Vec<i64>,
        chain: Arc<dyn ChainSource>,
    ) -> Result<Vec<BalancePoint>, LicashError> {
        let journal = self.wallet.journal()?;
        // A coin spent on a move is no longer a coin once its notes came,
        // whether or not the scan has seen its spend in a block yet.
        let mut moved_at: HashMap<Txid, i64> = HashMap::new();
        for deposit in self.wallet.deposits()? {
            let paid = deposit.funding.map(|(paid, _)| paid.txid);
            let came = journal.ended.get(&deposit.hash);
            if let Some((txid, at)) = paid.zip(came) {
                moved_at.insert(txid, *at);
            }
        }
        // Each coin from its block's time until the first of its spend's
        // block and its move's notes; one whose block's time is not known
        // is not placed.
        let mut coins = Vec::new();
        for coin in &journal.coins {
            let Some(from) = self.block_time(chain.as_ref(), coin.height) else {
                continue;
            };
            let spent = coin
                .spent_height
                .and_then(|h| self.block_time(chain.as_ref(), h));
            let moved = coin.spent_by.and_then(|by| moved_at.get(&by).copied());
            if coin.spent_height.is_some() && spent.is_none() && moved.is_none() {
                continue;
            }
            let until = spent.into_iter().chain(moved).min();
            coins.push((coin.value, from, until));
        }
        Ok(instants
            .into_iter()
            .map(|at| {
                let mut hubs: BTreeMap<&str, u64> = BTreeMap::new();
                for n in &journal.notes {
                    let held = n.added_at <= at
                        && n.spent_at.is_none_or(|s| s > at)
                        && n.gone_at.is_none_or(|g| g > at);
                    if held {
                        *hubs.entry(&n.hub).or_default() += n.amount;
                    }
                }
                let bitcoin = coins
                    .iter()
                    .filter(|(_, from, until)| *from <= at && until.is_none_or(|u| u > at))
                    .map(|(value, ..)| value)
                    .sum();
                BalancePoint {
                    at,
                    licash: hubs.values().sum(),
                    bitcoin,
                    hubs: hubs
                        .into_iter()
                        .map(|(hub, amount)| HubAmount {
                            hub: hub.to_owned(),
                            amount,
                        })
                        .collect(),
                }
            })
            .collect())
    }
}

impl Wallet {
    /// The time of the block at `height`, as known or as `chain` tells.
    fn block_time(&self, chain: &dyn ChainSource, height: u32) -> Option<i64> {
        let mut times = self.times.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(time) = times.get(&height) {
            return Some(*time);
        }
        let time = chain.time(height).ok()?;
        times.insert(height, time);
        Some(time)
    }

    /// The payments to itself the history hides: renewals and notes paid
    /// for the amounts a payment needs. Each costs its hub's fee.
    fn hidden_sends(&self, records: &Records) -> Result<Vec<StatEvent>, LicashError> {
        let me = self.wallet.address();
        let mut events = Vec::new();
        for sent in self.wallet.sends()? {
            if sent.payee != me || sent.is_optimization(&me) {
                continue;
            }
            let flow = match records.journal.renewals.contains(&sent.id) {
                true => Flow::Renewed,
                false => Flow::Normalized,
            };
            let state = match sent.state {
                licash_wallet::State::Done => State::Done,
                licash_wallet::State::Abandoned(_) => State::Failed,
                _ => State::Pending,
            };
            let fee = if state == State::Done { sent.fee } else { 0 };
            events.push(StatEvent {
                took: took(state, sent.created_at, records.journal.ended.get(&sent.id)),
                id: sent.id,
                flow,
                way: Way::Licash,
                state,
                at: Some(sent.created_at),
                amount: sent.amount,
                fee,
                hub_fee: Some(fee),
                provider_fee: None,
                network_fee: None,
            });
        }
        Ok(events)
    }

    /// The coins paid to the wallet on Bitcoin, spent or not, but its own
    /// change and what its payouts and contracts paid back to it.
    fn coin_events(
        &self,
        records: &Records,
        chain: &dyn ChainSource,
    ) -> Result<Vec<StatEvent>, LicashError> {
        let mut own: HashSet<Txid> = records.payouts.values().filter_map(|p| p.claim).collect();
        own.extend(self.fundings()?);
        let mut events = Vec::new();
        for coin in &records.journal.coins {
            if coin.change || own.contains(&coin.txid) {
                continue;
            }
            events.push(StatEvent {
                id: coin.txid.to_string(),
                flow: Flow::Received,
                way: Way::Bitcoin,
                state: State::Done,
                at: self.block_time(chain, coin.height),
                took: None,
                amount: coin.value,
                fee: 0,
                hub_fee: None,
                provider_fee: None,
                network_fee: None,
            });
        }
        Ok(events)
    }
}

/// The receipts among `history`'s lines that are the notes a contract's hub
/// issued the wallet of the coin it funded the contract with: its funded
/// part, back as notes. Nothing the wallet keeps says which receipt it is,
/// so each contract takes the earliest after it began that is of its
/// hub and of exactly its amount, none twice.
fn issued_receipts(history: &[Activity], contracts: &[crate::Contract]) -> HashSet<String> {
    let mut taken = HashSet::new();
    for c in contracts
        .iter()
        .filter(|c| c.funded > 0 && c.funded_received)
    {
        let hub = c.hub.trim_end_matches('/');
        let found = history
            .iter()
            .filter(|a| a.kind == Kind::Received && a.state == State::Done)
            .filter(|a| a.amount == c.funded && a.detail.trim_end_matches('/') == hub)
            .filter(|a| a.at.is_some_and(|at| at >= c.created_at))
            .filter(|a| !taken.contains(&a.id))
            .min_by_key(|a| a.at);
        if let Some(found) = found {
            taken.insert(found.id.clone());
        }
    }
    taken
}

/// Seconds from `began` to `ended`, of what is `state`: only if done.
fn took(state: State, began: i64, ended: Option<&i64>) -> Option<u64> {
    match (state, ended) {
        (State::Done, Some(end)) => Some(end.saturating_sub(began).max(0) as u64),
        _ => None,
    }
}

/// The event a history line is, if it is one: a coin on Bitcoin is not
/// (the journal has them all).
fn line_event(line: &Activity, records: &Records) -> Option<StatEvent> {
    let ended = records.journal.ended.get(&line.id);
    let settled = match (line.state, line.kind) {
        (State::Done, _) | (State::Failed, Kind::Lightning) => line.fee,
        _ => 0,
    };
    let mut event = StatEvent {
        id: line.id.clone(),
        flow: Flow::Sent,
        way: Way::Licash,
        state: line.state,
        at: line.at,
        took: None,
        amount: line.amount,
        fee: settled,
        hub_fee: None,
        provider_fee: None,
        network_fee: None,
    };
    let driven = |event: &mut StatEvent| {
        if let Some(at) = line.at {
            event.took = took(line.state, at, ended);
        }
    };
    match line.kind {
        Kind::Onchain => return None,
        Kind::Sent => {
            event.hub_fee = Some(settled);
            driven(&mut event);
        }
        Kind::Optimized => {
            event.flow = Flow::Optimized;
            event.hub_fee = Some(settled);
            driven(&mut event);
        }
        Kind::Received if records.own.contains(&line.id) => return None,
        Kind::Received => event.flow = Flow::Received,
        Kind::Invoice => {
            event.flow = Flow::Received;
            event.way = Way::Lightning;
        }
        Kind::Lightning => {
            event.way = Way::Lightning;
            driven(&mut event);
            let lock = records.locks.get(&line.id);
            match lock.and_then(|l| l.via.as_ref().map(|_| l.fee)) {
                // Through a provider: the hub's fee on the lock, and the
                // provider's the rest.
                Some(hub) => {
                    let hub = hub.min(settled);
                    event.hub_fee = Some(hub);
                    event.provider_fee = Some(settled - hub);
                }
                // Through the hub's gateway, all of it is the hub's.
                None => event.hub_fee = Some(settled),
            }
            if matches!(lock.map(|l| &l.state), Some(LockState::Refunded)) {
                event.took = None;
            }
        }
        Kind::Payout => {
            event.way = Way::Bitcoin;
            driven(&mut event);
            if let Some(p) = records.payouts.get(&line.id) {
                if line.state == State::Done {
                    let hub = lock_fee(records, p.lock.as_ref());
                    event.hub_fee = Some(hub);
                    event.provider_fee = Some(p.fee);
                    event.network_fee = Some(p.budget);
                }
            }
        }
        Kind::Swap => {
            event.flow = Flow::Moved;
            driven(&mut event);
            if let Some(s) = records.swaps.get(&line.id) {
                if line.state == State::Done {
                    event.hub_fee = Some(lock_fee(records, s.lock.as_ref()));
                    event.provider_fee = Some(s.fee);
                }
            }
        }
        Kind::Moved => {
            // Paid from a coin of the wallet's own, it is the coin turned
            // into notes (the coin is what came); otherwise, what came.
            let own = records
                .deposits
                .get(&line.id)
                .and_then(|d| d.funding)
                .is_some_and(|(paid, _)| records.spends.contains(&paid.txid));
            event.flow = if own { Flow::Moved } else { Flow::Received };
            event.way = Way::Bitcoin;
            event.hub_fee = Some(settled);
        }
        Kind::Contract => {
            event.flow = Flow::Protection;
            event.way = Way::Bitcoin;
            // Its price is all that is known: the hub's fee and her
            // reserve, which comes back, are not told apart.
            let (cap, price) = records.contracts.get(&line.id).copied()?;
            event.amount = cap;
            event.fee = if line.state == State::Done { price } else { 0 };
        }
    }
    Some(event)
}

/// What the hub's lock fee of the operation with lock `id` was, if known.
fn lock_fee(records: &Records, id: Option<&String>) -> u64 {
    id.and_then(|id| records.locks.get(id)).map_or(0, |l| l.fee)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Contract, ContractState};

    fn line(id: &str, kind: Kind, state: State, amount: u64, fee: u64, at: i64) -> Activity {
        Activity {
            id: id.into(),
            kind,
            state,
            amount,
            fee,
            detail: "http://hub".into(),
            at: Some(at),
            memo: None,
            from: None,
        }
    }

    fn contract(id: &str, funded: u64, created_at: i64) -> Contract {
        Contract {
            id: id.into(),
            hub: "http://hub/".into(),
            state: ContractState::Protecting,
            mock: false,
            cap: 1_000,
            covers: 0,
            price: 50,
            funded,
            funded_received: true,
            paid_from: 0,
            protects_until: 0,
            closes_at: 0,
            funding: None,
            created_at,
            step: None,
            transfer: None,
        }
    }

    fn records() -> Records {
        Records {
            journal: Journal::default(),
            locks: HashMap::new(),
            payouts: HashMap::new(),
            swaps: HashMap::new(),
            deposits: HashMap::new(),
            spends: HashSet::new(),
            contracts: HashMap::new(),
            own: HashSet::new(),
        }
    }

    #[test]
    fn a_contracts_funded_notes_are_the_earliest_receipt_of_their_amount_after_it_began() {
        let history = vec![
            line("late", Kind::Received, State::Done, 700, 0, 300),
            line("early", Kind::Received, State::Done, 700, 0, 200),
            line("before", Kind::Received, State::Done, 700, 0, 50),
            line("other", Kind::Received, State::Done, 701, 0, 150),
            line("sent", Kind::Sent, State::Done, 700, 1, 160),
        ];
        let one = [contract("a", 700, 100)];
        let taken = issued_receipts(&history, &one);
        assert_eq!(taken, HashSet::from(["early".to_owned()]));
        // Two contracts of one amount take two, none twice; one with none
        // funded, or not yet received, takes none.
        let mut unreceived = contract("c", 700, 100);
        unreceived.funded_received = false;
        let many = [contract("a", 700, 100), contract("b", 700, 100)];
        let taken = issued_receipts(&history, &many);
        assert_eq!(
            taken,
            HashSet::from(["early".to_owned(), "late".to_owned()])
        );
        let none = [unreceived, contract("d", 0, 100)];
        assert!(issued_receipts(&history, &none).is_empty());
    }

    #[test]
    fn only_a_settled_fee_is_a_cost() {
        let records = records();
        let at =
            |kind, state, fee| line_event(&line("x", kind, state, 100, fee, 10), &records).unwrap();
        assert_eq!(at(Kind::Sent, State::Done, 3).fee, 3);
        assert_eq!(at(Kind::Sent, State::Done, 3).hub_fee, Some(3));
        // Never paid, or not yet.
        assert_eq!(at(Kind::Sent, State::Failed, 3).fee, 0);
        assert_eq!(at(Kind::Sent, State::Pending, 3).fee, 0);
        assert_eq!(at(Kind::Moved, State::Failed, 100).fee, 0);
        // A refunded Lightning payment kept the cost of its refund.
        assert_eq!(at(Kind::Lightning, State::Failed, 1).fee, 1);
        assert_eq!(at(Kind::Lightning, State::Pending, 2).fee, 0);
        // What is paid to the wallet costs it nothing here.
        assert_eq!(at(Kind::Received, State::Done, 0).fee, 0);
        // A coin is the journal's, not the history's.
        assert!(line_event(&line("c", Kind::Onchain, State::Done, 5, 0, 1), &records).is_none());
    }

    #[test]
    fn an_operation_took_from_its_start_to_its_end_once_done_and_only_then() {
        let mut records = records();
        records.journal.ended.insert("x".into(), 25);
        let of = |state| {
            line_event(&line("x", Kind::Sent, state, 100, 1, 10), &records)
                .unwrap()
                .took
        };
        assert_eq!(of(State::Done), Some(15));
        assert_eq!(of(State::Failed), None);
        assert_eq!(of(State::Pending), None);
        // What waits for another is not timed.
        records.journal.ended.insert("r".into(), 99);
        let got = line_event(&line("r", Kind::Invoice, State::Done, 1, 0, 10), &records);
        assert_eq!(got.unwrap().took, None);
    }

    #[test]
    fn a_swaps_notes_and_a_contracts_funded_notes_are_not_payments_received() {
        let mut records = records();
        records.own.insert("theirs".into());
        let got = line_event(
            &line("theirs", Kind::Received, State::Done, 300, 0, 1),
            &records,
        );
        assert!(got.is_none());
        let got = line_event(
            &line("real", Kind::Received, State::Done, 300, 0, 1),
            &records,
        );
        assert_eq!(got.unwrap().flow, Flow::Received);
    }
}
