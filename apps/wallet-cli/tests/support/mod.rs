//! A hub service on a local port that can go down and come back, following
//! a regtest chain of its own, and a payer outside any wallet.

#[path = "../../../hub/tests/chain/mod.rs"]
mod chain;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{Output as SpendOutput, Proposal},
    unlink::Nonce,
};
use licash_api::{Fee, Payment};
use licash_hub::{Hub, Policy, Timing};

const MAX_POW: u32 = 6;

/// The timing of the hub's locks: short, to expire them in a test.
const TIMING: Timing = Timing {
    window: 20,
    reaction: 2,
    min_depth: 1,
    max_headers: 4032,
    bury: 6,
    max_lag: 120,
};

pub struct Server {
    pub hub: Arc<Hub>,
    chain: Mutex<chain::Chain>,
    addr: SocketAddr,
    seed: PathBuf,
    running: Option<(tokio::sync::oneshot::Sender<()>, JoinHandle<()>)>,
    _dir: tempfile::TempDir,
}

impl Server {
    /// A hub charging `fee` on spends and in each branch of a lock, at
    /// height 3 of its chain.
    pub fn start(fee: Fee) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, MAX_POW).unwrap();
        let policy = Policy {
            spend: fee,
            lock: fee,
            refund: fee,
            timing: TIMING,
        };
        let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
        let chain = chain::Chain::new();
        hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
        let mut server = Self {
            hub,
            chain: Mutex::new(chain),
            addr: "127.0.0.1:0".parse().unwrap(),
            seed,
            running: None,
            _dir: dir,
        };
        server.mine(3, None);
        server.up();
        server
    }

    /// Mine `blocks`, the first with an activation of `secret` if given,
    /// which the hub scans.
    pub fn mine(&self, blocks: u32, mut secret: Option<[u8; 32]>) {
        let mut chain = self.chain.lock().unwrap();
        for _ in 0..blocks {
            let block = chain.mine(secret.take()).clone();
            self.hub.observe(chain.height(), &block).unwrap();
        }
        assert!(self.hub.synced(chain.height(), &chain.tip()).unwrap());
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Serve again, at the same address.
    pub fn up(&mut self) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (bound, addr) = std::sync::mpsc::channel();
        let (hub, at) = (self.hub.clone(), self.addr);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                socket.bind(at).unwrap();
                let listener = socket.listen(64).unwrap();
                bound.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, licash_hub_service::router(hub))
                    .with_graceful_shutdown(async {
                        stopped.await.ok();
                    })
                    .await
                    .unwrap();
            });
        });
        self.addr = addr.recv().unwrap();
        self.running = Some((stop, thread));
    }

    pub fn down(&mut self) {
        let (stop, thread) = self.running.take().unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
    }

    /// A payment of `amount` to `to`, its fee paid, from notes the hub's
    /// keys made for someone outside any wallet.
    pub fn pay(&self, to: &Address, amount: u64) -> Payment {
        let text = std::fs::read_to_string(&self.seed).unwrap();
        let mut seed = [0; 32];
        hex::decode_to_slice(text.trim(), &mut seed).unwrap();
        let keys = IssuerKeys::from_seed(&seed, 0, MAX_POW).unwrap();
        let keyset = keys.keyset().unwrap();
        let info = self.hub.info();
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
                let derivation = ephemeral.derive(to, d, Branch::Pay, i).unwrap();
                SpendOutput::from_derivation(&derivation).unwrap()
            })
            .collect();
        let proposal = Proposal::new(&keyset, &notes, ephemeral.public(), outputs, None).unwrap();
        let grant = self.hub.offer(&proposal, &notes).unwrap();
        let owners: Vec<_> = inputs
            .iter()
            .map(|(_, d)| d.ownership().owner_secret(payer.spend_secret()).unwrap())
            .collect();
        let grant = Authorization::Granted(Box::new(grant));
        let signed = SignedProposal::sign(&keyset, proposal, &notes, grant, &owners).unwrap();
        let unlock = self.hub.release(&signed, &notes).unwrap();
        Payment::new(&self.url(), &signed, &notes, &unlock)
    }
}

fn split(mut amount: u64) -> Vec<u64> {
    let mut denoms = Vec::new();
    for pow in (0..=MAX_POW).rev() {
        while amount >= 1 << pow {
            denoms.push(1 << pow);
            amount -= 1 << pow;
        }
    }
    denoms
}

fn mint(keys: &IssuerKeys, keyset: &Keyset, to: &Address, denom: u64) -> (Note, Derivation) {
    let d = Ephemeral::random()
        .derive(to, denom, Branch::Pay, 0)
        .unwrap();
    let output = SpendOutput::from_derivation(&d).unwrap();
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

/// `licash-wallet --wallet <wallet> <args>`, `input` on its stdin.
pub fn wallet(wallet: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_licash-wallet"));
    command
        .arg("--wallet")
        .arg(wallet)
        .args(args)
        .env_remove("LICASH_WALLET_PROXY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    if let Some(input) = input {
        use std::io::Write;
        let mut stdin = stdin;
        stdin.write_all(input.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}
