//! Hubs in process, behind URLs, with requests that fail on demand.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{Output, Proposal},
    unlink::Nonce,
};
use licash_api::{
    Check, Checked, Code, Fee, Fees, Info, Offer, Offered, Payment, Problem, Release, Released,
    Unlink, Unlinked,
};
use licash_hub::{Error, Hub, Policy, Refusal};
use licash_wallet::{Response, Transport};

pub const MAX_POW: u32 = 6;
pub const HUB: &str = "http://hub.test";

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    /// The request never reaches the hub.
    Lost,
    /// The hub serves it, but its answer is lost.
    Dropped,
    /// The hub does not serve it, and answers with this problem.
    Refused(Code),
}

struct Site {
    hub: Option<Arc<Hub>>,
    seed: PathBuf,
    db: PathBuf,
    policy: Policy,
}

#[derive(Default)]
pub struct Net {
    sites: Mutex<HashMap<String, Site>>,
    faults: Mutex<Vec<(&'static str, Fault)>>,
    /// Fees the hubs advertise instead of their own.
    pub stale: Mutex<Option<Fee>>,
    dirs: Mutex<Vec<tempfile::TempDir>>,
}

impl Net {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// A new hub at `url`, charging `fee` on spends.
    pub fn start(&self, url: &str, fee: Fee) {
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, MAX_POW).unwrap();
        let policy = Policy {
            spend: fee,
            ..Policy::default()
        };
        let hub = Some(Arc::new(Hub::open(&seed, &db, policy).unwrap()));
        self.dirs.lock().unwrap().push(dir);
        let site = Site {
            hub,
            seed,
            db,
            policy,
        };
        self.sites.lock().unwrap().insert(url.into(), site);
    }

    /// `url` serves the hub at `at` too.
    pub fn alias(&self, url: &str, at: &str) {
        let mut sites = self.sites.lock().unwrap();
        let site = &sites[at];
        let alias = Site {
            hub: site.hub.clone(),
            seed: site.seed.clone(),
            db: site.db.clone(),
            policy: site.policy,
        };
        sites.insert(url.into(), alias);
    }

    /// Stop the hub at `url` and start it again on its files.
    pub fn restart(&self, url: &str) {
        let mut sites = self.sites.lock().unwrap();
        let site = sites.get_mut(url).unwrap();
        drop(site.hub.take());
        site.hub = Some(Arc::new(
            Hub::open(&site.seed, &site.db, site.policy).unwrap(),
        ));
    }

    /// The next request to `path` fails so.
    pub fn fail(&self, path: &'static str, fault: Fault) {
        self.faults.lock().unwrap().push((path, fault));
    }

    pub fn hub(&self, url: &str) -> Arc<Hub> {
        self.sites.lock().unwrap()[url].hub.clone().unwrap()
    }

    /// The issuer keys of the hub at `url`, from its seed file.
    fn issuer(&self, url: &str) -> (IssuerKeys, Keyset) {
        let text = std::fs::read_to_string(&self.sites.lock().unwrap()[url].seed).unwrap();
        let mut seed = [0; 32];
        hex::decode_to_slice(text.trim(), &mut seed).unwrap();
        let keys = IssuerKeys::from_seed(&seed, 0, MAX_POW).unwrap();
        let keyset = keys.keyset().unwrap();
        (keys, keyset)
    }

    fn serve(&self, url: &str, body: Option<Vec<u8>>) -> Result<Response, String> {
        let at = url.find("/v1/").ok_or("no such path")?;
        let (site, path) = url.split_at(at);
        let fault = {
            let mut faults = self.faults.lock().unwrap();
            let found = faults.iter().position(|(p, _)| *p == path);
            found.map(|i| faults.remove(i).1)
        };
        match fault {
            Some(Fault::Lost) => return Err("connection refused".into()),
            Some(Fault::Refused(code)) => {
                let problem = Problem::new(code, "refused as asked");
                return Ok(Response {
                    status: code.status(),
                    body: serde_json::to_vec(&problem).unwrap(),
                });
            }
            _ => {}
        }
        let hub = self
            .sites
            .lock()
            .unwrap()
            .get(site)
            .and_then(|s| s.hub.clone());
        let hub = hub.ok_or("no such host")?;
        let answer = match (path, body) {
            (licash_api::INFO, None) => self.info(&hub),
            (path, Some(body)) => post(&hub, path, &body),
            _ => Err(Problem::invalid("no such route")),
        };
        if let Some(Fault::Dropped) = fault {
            return Err("connection reset".into());
        }
        Ok(match answer {
            Ok(body) => Response { status: 200, body },
            Err(problem) => Response {
                status: problem.code.status(),
                body: serde_json::to_vec(&problem).unwrap(),
            },
        })
    }

    fn info(&self, hub: &Hub) -> Result<Vec<u8>, Problem> {
        let info = hub.info();
        let spend = self.stale.lock().unwrap().unwrap_or(info.policy.spend);
        let fees = Fees {
            spend,
            lock: info.policy.lock,
            refund: info.policy.refund,
        };
        Ok(serde_json::to_vec(&Info::new(&info.keysets, info.address, fees, None)).unwrap())
    }
}

fn post(hub: &Hub, path: &str, body: &[u8]) -> Result<Vec<u8>, Problem> {
    let json = |e: serde_json::Error| Problem::invalid(e.to_string());
    let lookup = |id: &[u8; 32]| hub.keyset(id);
    let answer = match path {
        licash_api::OFFER => {
            let offer: Offer = serde_json::from_slice(body).map_err(json)?;
            let (proposal, notes) = offer.decode(lookup)?;
            let grant = hub.offer(&proposal, &notes).map_err(problem)?;
            serde_json::to_vec(&Offered::new(&grant)?)
        }
        licash_api::RELEASE => {
            let release: Release = serde_json::from_slice(body).map_err(json)?;
            let (signed, notes) = release.decode(lookup)?;
            let unlock = hub.release(&signed, &notes).map_err(problem)?;
            serde_json::to_vec(&Released::new(&unlock))
        }
        licash_api::UNLINK => {
            let unlink: Unlink = serde_json::from_slice(body).map_err(json)?;
            let answer = hub.unlink(&unlink.decode()?).map_err(problem)?;
            serde_json::to_vec(&Unlinked::new(&answer))
        }
        licash_api::CHECK => {
            let check: Check = serde_json::from_slice(body).map_err(json)?;
            let spent = hub.spent(&check.decode()?).map_err(problem)?;
            serde_json::to_vec(&Checked { spent })
        }
        _ => return Err(Problem::invalid("no such route")),
    };
    Ok(answer.unwrap())
}

/// As the hub service answers.
fn problem(e: Error) -> Problem {
    let code = match &e {
        Error::Refused(refusal) => match refusal {
            Refusal::UnknownKeyset => Code::UnknownKeyset,
            Refusal::Spent => Code::Spent,
            Refusal::Reissued => Code::Reissued,
            Refusal::Fee { .. } => Code::Fee,
            Refusal::UnknownOffer => Code::UnknownOffer,
            Refusal::NotIssued => Code::NotIssued,
            Refusal::Unlinked => Code::Unlinked,
            Refusal::Expired => Code::Expired,
            _ => Code::Invalid,
        },
        Error::Invalid(_) => Code::Invalid,
        _ => Code::Unavailable,
    };
    Problem::new(code, e.to_string())
}

/// A wallet's way onto the network.
#[derive(Clone)]
pub struct Link(pub Arc<Net>);

impl Transport for Link {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        std::future::ready(self.0.serve(url, body))
    }
}

/// `amount` in powers of two the keysets issue, largest first.
pub fn split(mut amount: u64) -> Vec<u64> {
    let mut denoms = Vec::new();
    for pow in (0..=MAX_POW).rev() {
        while amount >= 1 << pow {
            denoms.push(1 << pow);
            amount -= 1 << pow;
        }
    }
    denoms
}

impl Net {
    /// A payment of `amount` to `to` at the hub at `url`, its fee paid, from
    /// notes its keys made for someone outside any wallet.
    pub fn pay(&self, url: &str, to: &Address, amount: u64) -> Payment {
        let hub = self.hub(url);
        let (keys, keyset) = self.issuer(url);
        let info = hub.info();
        let mut total = amount;
        while amount + info.policy.spend.due(total) != total {
            total = amount + info.policy.spend.due(total);
        }
        let payer = OwnerKeys::from_seed(&[0xfa; 32]).unwrap();
        let inputs: Vec<(Note, Derivation)> = split(total)
            .into_iter()
            .map(|denom| mint(&keys, &keyset, &payer.address(), denom))
            .collect();
        let notes: Vec<Note> = inputs.iter().map(|(note, _)| note.clone()).collect();
        let ephemeral = Ephemeral::random();
        let to_hub = split(total - amount)
            .into_iter()
            .map(|d| (d, &info.address));
        let outputs = split(amount)
            .into_iter()
            .map(|d| (d, to))
            .chain(to_hub)
            .enumerate()
            .map(|(i, (d, to))| {
                Output::from_derivation(&ephemeral.derive(to, d, Branch::Pay, i).unwrap()).unwrap()
            })
            .collect();
        let proposal = Proposal::new(&keyset, &notes, ephemeral.public(), outputs, None).unwrap();
        let grant = hub.offer(&proposal, &notes).unwrap();
        let owners: Vec<_> = inputs
            .iter()
            .map(|(_, d)| d.ownership().owner_secret(payer.spend_secret()).unwrap())
            .collect();
        let signed = SignedProposal::sign(
            &keyset,
            proposal,
            &notes,
            Authorization::Granted(Box::new(grant)),
            &owners,
        )
        .unwrap();
        let unlock = hub.release(&signed, &notes).unwrap();
        Payment::new(url, &signed, &notes, &unlock)
    }
}

/// A note of `denom` to `to`, as issued before the hub's ledger began.
fn mint(keys: &IssuerKeys, keyset: &Keyset, to: &Address, denom: u64) -> (Note, Derivation) {
    let d = Ephemeral::random()
        .derive(to, denom, Branch::Pay, 0)
        .unwrap();
    let output = Output::from_derivation(&d).unwrap();
    let proof = keys.prove_blinded(denom, output.blinded()).unwrap();
    let note = Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, &d).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: Nonce::new(keyset, &output).unwrap().commit().unwrap(),
        },
    )
    .unwrap();
    (note, d)
}
