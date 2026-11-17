//! Her contracts at a hub as the slots her notes there are shared out to
//! ([`ClaimSlot`]): what [`Wallet::recoverable`] shows they protect
//! ([`super::coverage`]) and what [`Wallet::claim_notes`] takes for one's
//! recovery are shared out alike, of the same slots.

use std::collections::HashMap;

use anyhow::Result;
use licash::claim::MAX_ITEMS;
use licash_contract::opening::{Context, Holder, Opening};
use licash_wallet::{ClaimSlot, Transport, Wallet};

use super::store::{Contract, KeptTerms, Store};

/// How a contract's recovery claims notes: those of the keyset of id
/// `keyset`, `max` of them at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimTerms {
    pub keyset: [u8; 32],
    pub max: usize,
}

impl ClaimTerms {
    /// Of a contract whose claim's notes are of `keyset`, which allows
    /// `authorizations`: a claim's items are as many at most.
    pub fn new(keyset: [u8; 32], authorizations: u32) -> Self {
        let authorizations = usize::try_from(authorizations).unwrap_or(usize::MAX);
        Self {
            keyset,
            max: MAX_ITEMS.min(authorizations),
        }
    }

    pub(super) fn of(opening: &Opening) -> Self {
        let parts = opening.base.parts();
        Self::new(*parts.keyset.id(), parts.limits.max_authorizations)
    }
}

/// The claim terms of `contracts`, of those in `store`, by id: as kept,
/// or read from their state as stored at `tip` (no hub asked), then kept.
/// None of one whose state cannot be read, nor of one opened under a
/// program this build does not trust: its recovery claims nothing.
pub(super) fn terms<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    contracts: &[&Contract],
    tip: u32,
) -> Result<HashMap<[u8; 32], ClaimTerms>> {
    let trust = crate::trust(wallet.network())?;
    let kept = store.claim_terms()?;
    let mut terms = HashMap::with_capacity(contracts.len());
    for c in contracts {
        let id = crate::hex_id(&c.id);
        let read = match kept.get(&c.id) {
            Some(kept) => *kept,
            None => match read(wallet, store, &c.id, tip) {
                Ok(read) => {
                    store.keep_claim_terms(&c.id, &read)?;
                    read
                }
                Err(e) => {
                    tracing::warn!(%id, "a contract's claim terms cannot be read: {e:#}");
                    continue;
                }
            },
        };
        if !trust.mock && read.program != trust.program {
            tracing::warn!(%id, "a contract of an unsupported program claims nothing");
            continue;
        }
        terms.insert(c.id, ClaimTerms::new(read.keyset, read.authorizations));
    }
    Ok(terms)
}

/// The claim terms of contract `id`, as its stored state has them at
/// `tip`.
fn read<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    id: &[u8; 32],
    tip: u32,
) -> Result<KeptTerms> {
    let (state, _) = store.state(id)?;
    let secrets = wallet.contract_secrets();
    let trust = crate::trust(wallet.network())?;
    let holder = Holder::load(&state, &secrets.seed, &Context { tip, trust })?;
    let opening = holder.signed()?;
    let parts = opening.base.parts();
    Ok(KeptTerms {
        keyset: *parts.keyset.id(),
        authorizations: parts.limits.max_authorizations,
        program: parts.recipe.program,
    })
}

/// What each of `contracts` recovers, by id, of the notes at each hub of
/// `hubs` (the URLs each is known by): what `recover` gives the slots of
/// those there ([`claim_slots`]), as their `terms` make them. None of one
/// recovering nothing.
pub fn shared_out<E>(
    contracts: &[&Contract],
    terms: &HashMap<[u8; 32], ClaimTerms>,
    hubs: &[Vec<String>],
    mut recover: impl FnMut(&[ClaimSlot]) -> Result<HashMap<[u8; 32], u64>, E>,
) -> Result<HashMap<[u8; 32], u64>, E> {
    let mut each = HashMap::new();
    for urls in hubs {
        let slots = claim_slots(contracts.iter().copied().filter(|c| c.at(urls)), terms);
        if !slots.is_empty() {
            each.extend(recover(&slots)?.into_iter().filter(|(_, v)| *v > 0));
        }
    }
    Ok(each)
}

/// The slots of `contracts`, each with its `terms`, the one asserting
/// first first (by id among equals): none of one without.
pub fn claim_slots<'a>(
    contracts: impl IntoIterator<Item = &'a Contract>,
    terms: &HashMap<[u8; 32], ClaimTerms>,
) -> Vec<ClaimSlot> {
    let mut slots: Vec<(&Contract, &ClaimTerms)> = (contracts.into_iter())
        .filter_map(|c| Some((c, terms.get(&c.id)?)))
        .collect();
    slots.sort_by_key(|(c, _)| (c.assert_height, c.id));
    (slots.into_iter())
        .map(|(c, terms)| ClaimSlot {
            id: c.id,
            keyset: terms.keyset,
            cap: u64::from(c.cap),
            max: terms.max,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bitcoin::OutPoint;
    use licash_contract::opening::{Fate, Phase};

    use super::*;

    fn contract(id: u8, cap: u32, assert_height: u32) -> Contract {
        Contract {
            id: [id; 32],
            hub: "https://hub.example".into(),
            phase: Phase::Funded,
            mock: true,
            deposit: OutPoint::null(),
            value: 60_000,
            price: 23_000,
            cap,
            assert_height,
            close_height: 500,
            since: 200,
            funding: None,
            fate: Some(Fate::Live),
            abandoned: None,
            created_at: 0,
            folded: None,
        }
    }

    #[test]
    fn a_claim_holds_as_many_notes_as_its_contract_authorizes_32_at_most() {
        assert_eq!(ClaimTerms::new([1; 32], 128).max, 32);
        assert_eq!(ClaimTerms::new([1; 32], 3).max, 3);
        assert_eq!(ClaimTerms::new([1; 32], 0).max, 0);
    }

    #[test]
    fn slots_are_served_the_one_asserting_first_first_and_only_with_terms() {
        let all = [
            contract(3, 100, 400),
            contract(1, 200, 400),
            contract(2, 300, 380),
            contract(4, 400, 300),
        ];
        let terms = HashMap::from([
            ([1; 32], ClaimTerms::new([7; 32], 128)),
            ([2; 32], ClaimTerms::new([7; 32], 2)),
            ([3; 32], ClaimTerms::new([8; 32], 128)),
        ]);
        let slots = claim_slots(&all, &terms);
        let order: Vec<_> = slots.iter().map(|s| (s.id[0], s.cap, s.max)).collect();
        assert_eq!(order, [(2, 300, 2), (1, 200, 32), (3, 100, 32)]);
        assert_eq!(slots[2].keyset, [8; 32]);
        assert!(claim_slots(&all, &HashMap::new()).is_empty());
    }

    #[test]
    fn the_notes_at_each_hub_are_shared_out_among_its_contracts_alone() {
        const OTHER: &str = "https://other.example";
        let keyset = |hub: &str| [if hub == OTHER { 0xbb } else { 0xaa }; 32];
        let notes = |hub, denominations: &[u64]| -> Vec<([u8; 32], u64)> {
            denominations.iter().map(|d| (keyset(hub), *d)).collect()
        };
        // The earliest first by deadline, then by id.
        let there = Contract {
            hub: OTHER.into(),
            ..contract(4, 4_096, 370)
        };
        let all = [
            contract(1, 4_096, 400),
            contract(2, 4_096, 380),
            contract(3, 4_096, 400),
            there,
        ];
        let contracts: Vec<&Contract> = all.iter().collect();
        let terms = (all.iter())
            .map(|c| (c.id, ClaimTerms::new(keyset(&c.hub), 128)))
            .collect();
        // OTHER known by a slash more.
        let hubs = [
            vec!["https://hub.example".into()],
            vec![format!("{OTHER}/")],
        ];
        let shares = |notes: &[([u8; 32], u64)]| {
            let recover = |slots: &[ClaimSlot]| {
                let each = ClaimSlot::recoverable(slots, notes);
                Ok::<_, ()>(slots.iter().map(|s| s.id).zip(each).collect())
            };
            let each = shared_out(&contracts, &terms, &hubs, recover).unwrap();
            [1, 2, 3, 4].map(|id| each.get(&[id; 32]).copied())
        };
        let at_other = notes(OTHER, &[512, 256, 128, 64, 32, 8]);
        let held = notes("https://hub.example", &[4_096, 4_096, 1_024, 512, 256, 16]);
        assert_eq!(
            shares(&[held, at_other.clone()].concat()),
            [Some(4_096), Some(4_096), Some(1_808), Some(1_000)]
        );
        let held = notes("https://hub.example", &[4_096, 512, 256, 128, 8]);
        assert_eq!(
            shares(&[held, at_other].concat()),
            [Some(904), Some(4_096), None, Some(1_000)]
        );
        // One note beyond a cap: claimed for the cap, its excess lost,
        // none left for the others.
        let held = notes("https://hub.example", &[8_192]);
        assert_eq!(shares(&held), [None, Some(4_096), None, None]);
        assert_eq!(shares(&[]), [None; 4]);
    }

    #[test]
    fn a_contract_of_a_program_not_trusted_has_no_terms() {
        use bitcoin::hashes::Hash as _;
        use licash::spend::ChainProfile;

        use super::super::store::New;

        struct Offline;
        impl Transport for Offline {
            async fn request(
                &self,
                _: &str,
                _: Option<Vec<u8>>,
            ) -> Result<licash_wallet::Response, String> {
                Err("no network here".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for id in 1..=3 {
            let deposit = OutPoint {
                txid: bitcoin::Txid::from_byte_array([9; 32]),
                vout: u32::from(id),
            };
            store
                .insert(&New {
                    id: [id; 32],
                    hub: "https://hub.example",
                    mock: false,
                    deposit,
                    value: 60_000,
                    price: 23_000,
                    cap: 4_096,
                    assert_height: 360,
                    close_height: 500,
                    since: 200,
                    state: b"no state",
                })
                .unwrap();
        }
        let kept = |program| KeptTerms {
            keyset: [7; 32],
            authorizations: 128,
            program,
        };
        let genuine = crate::SIGNET;
        store.keep_claim_terms(&[1; 32], &kept([5; 32])).unwrap();
        store.keep_claim_terms(&[2; 32], &kept(genuine)).unwrap();
        let all = [
            contract(1, 4_096, 360),
            contract(2, 4_096, 360),
            contract(3, 4_096, 360),
        ];
        let all: Vec<&Contract> = all.iter().collect();
        let terms_on = |profile, name: &str| {
            let path = dir.path().join(name);
            let wallet = Wallet::create(&path, &[7; 32], profile, Offline).unwrap();
            let terms = terms(&wallet, &store, &all, 300).unwrap();
            let mut ids: Vec<u8> = terms.keys().map(|id| id[0]).collect();
            ids.sort();
            ids
        };
        // On signet, of the genuine program alone; of none whose state
        // cannot be read, kept or not.
        assert_eq!(terms_on(ChainProfile::Core30Signet, "signet.sqlite"), [2]);
        // Under the mock authority, any program.
        let regtest = ChainProfile::Core30DefaultRegtest;
        assert_eq!(terms_on(regtest, "regtest.sqlite"), [1, 2]);
        assert_eq!(store.claim_terms().unwrap().len(), 2);
    }
}
