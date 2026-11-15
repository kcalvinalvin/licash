//! The `licash-hub` binary end to end: create, run, watch and stop a hub.

mod chain;

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use licash::{
    address::{Ephemeral, OwnerKeys},
    claim::Branch,
    spend::{self, ChainProfile, TimedTerms},
};
use licash_hub::{Hub, Policy};

fn hub(data: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_licash-hub"));
    command
        .arg("--data")
        .arg(data)
        .args(args)
        .env_remove("LICASH_HUB_SEED")
        .env_remove("LICASH_HUB_LISTEN")
        .env_remove("LICASH_HUB_NOTIFY")
        .env_remove("LICASH_HUB_NOTIFY_KINDS")
        .env_remove("LICASH_HUB_NAME")
        .env_remove("LICASH_HUB_NOSTR_RELAYS")
        .env_remove("LICASH_HUB_ANNOUNCE_URLS")
        .env_remove("LICASH_HUB_ONION")
        .env_remove("LICASH_HUB_SWAPS")
        .env_remove("LICASH_HUB_SWAPS_FROM")
        .env_remove("LICASH_HUB_ALLOW_UNBACKED_LOCKS")
        .env("RUST_LOG", "info");
    command
}

fn output(data: &Path, args: &[&str]) -> Output {
    hub(data, args).output().unwrap()
}

fn stdout(data: &Path, args: &[&str]) -> String {
    let out = output(data, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A running hub, stopped if the test fails.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stats(data: &Path) -> serde_json::Value {
    serde_json::from_str(&stdout(data, &["status", "--json"])).unwrap()
}

#[test]
fn a_hub_is_created_run_watched_and_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("hub");
    stdout(&data, &["init", "--max-pow", "6"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&data.join("seed")), 0o600);
    }
    let again = output(&data, &["init"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("exists"));
    assert_eq!(stats(&data)["offers"], 0);
    assert_eq!(stats(&data)["events"], 0);
    // Read by a person: what it earned first, in sat, then what it did.
    let text = stdout(&data, &["status"]);
    assert!(text.starts_with("income  0 sat\n"), "{text}");
    assert!(
        text.contains("\nnotes   0 issued, 0 spent, 0 unlinked\n"),
        "{text}"
    );

    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let mut running = Running(
        hub(
            &data,
            &[
                "run",
                "--listen",
                &listen,
                "--spend-fee-base",
                "3",
                "--mailbox-relay",
                "wss://one.example,ws://two.example",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let health = until("health", || get(port, "/health"));
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.ends_with("ok"), "{health}");
    let info = get(port, licash_api::INFO).unwrap();
    let body = info.split("\r\n\r\n").nth(1).unwrap();
    let info: licash_api::Info = serde_json::from_str(body).unwrap();
    assert_eq!(info.fees.spend.base, 3);
    assert_eq!(info.keysets().unwrap().len(), 1);
    assert_eq!(info.relays, ["wss://one.example", "ws://two.example"]);

    let second = output(
        &data,
        &["run", "--listen", &format!("127.0.0.1:{}", free_port())],
    );
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another hub"));

    // Watched beside the running hub.
    assert_eq!(stats(&data)["events"], 1);
    let events = stdout(&data, &["events"]);
    let lines: Vec<serde_json::Value> = events
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["seq"], 1);
    assert_eq!(lines[0]["kind"], "started");
    assert!(stdout(&data, &["events", "--after", "1"]).is_empty());

    let pid = running.0.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let status = until("stop", || running.0.try_wait().unwrap());
    assert!(status.success());
    let mut logs = String::new();
    running
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    assert!(
        logs.contains("serving") && logs.contains("stopped"),
        "{logs}"
    );
    assert!(!logs.contains('\x1b'), "colors in logs to a pipe: {logs}");
    // Stopped, it can start again.
    let port = free_port();
    let _running = Running(
        hub(&data, &["run", "--listen", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("restart", || get(port, "/health"));
    assert_eq!(stats(&data)["events"], 2);
}

#[test]
fn failures_exit_nonzero_and_log_as_asked() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none");
    let out = output(&missing, &["--log", "json", "status"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    let line: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(line["level"], "ERROR");
    assert!(line["fields"]["message"]
        .as_str()
        .unwrap()
        .contains("no hub database"));
    // Running needs a hub: it never creates one.
    let out = output(&missing, &["run", "--listen", "127.0.0.1:0"]);
    assert!(!out.status.success());
    assert!(!missing.exists());
    let out = output(&missing, &["init", "--max-pow", "63"]);
    assert!(!out.status.success());
    let out = output(&missing, &["run", "--spend-fee-ppm", "1000001"]);
    assert!(!out.status.success());
    let out = output(
        &missing,
        &["run", "--mailbox-relay", "https://relay.example"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ws:// or wss://"));
    // A chain URL it cannot follow, its password unshown.
    for url in [
        "ftp://user:hunter2@node:8332",
        "http://user:hunter2@node:8332?x=1",
    ] {
        let out = output(&missing, &["run", "--chain", url]);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--chain") && !stderr.contains("hunter2"),
            "{stderr}"
        );
    }
    let poll = ["run", "--chain", "http://node:8332", "--chain-poll", "120"];
    let out = output(&missing, &poll);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--max-lag"));
    // Tweaks are of the chain it follows.
    let out = output(&missing, &["run", "--tweaks"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--tweaks) needs --chain"));
    let out = output(&missing, &["run", "--tweaks-from", "1"]);
    assert_eq!(out.status.code(), Some(2));
    // Deposits are of the chain it follows, claimed to an address on it.
    let sweep = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
    let out = output(&missing, &["run", "--deposit-sweep-address", sweep]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--deposit-sweep-address) needs --chain"),
        "{stderr}"
    );
    let chain = ["run", "--chain", "http://node:8332", "--network", "regtest"];
    let mainnet = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    let out = output(
        &missing,
        &[&chain[..], &["--deposit-sweep-address", mainnet]].concat(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not an address on"), "{stderr}");
    let out = output(
        &missing,
        &[&chain[..], &["--deposit-sweep-address", "bcrt1nope"]].concat(),
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("--deposit-sweep-address"));
    let out = output(&missing, &["run", "--deposit-min-fee-rate", "0"]);
    assert_eq!(out.status.code(), Some(2));
    // An announcement needs relays, URLs wallets can use, and the chain
    // that names its network.
    let relay = ["--nostr-relay", "ws://127.0.0.1:1"];
    for (args, says) in [
        (
            &["--announce-url", "https://hub.example"][..],
            "needs --nostr-relay",
        ),
        (&relay[..], "--announce-url"),
        (
            &[relay[0], relay[1], "--announce-url", "ftp://hub.example"][..],
            "ftp://",
        ),
        (
            &[relay[0], relay[1], "--announce-url", "https://hub.example"][..],
            "needs --chain",
        ),
    ] {
        let out = output(&missing, &[&["run"], args].concat());
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    // Swaps into the hub are of the chain it follows, from hubs it caps.
    let from = ["--swaps-from", "http://a.example=100"];
    for (args, says) in [
        (&from[..], "--swaps-from needs --swaps"),
        (&["--swaps"][..], "--swaps) needs --chain"),
        (
            &["--swaps", "--swaps-from", "http://a.example"][..],
            "URL=CAP",
        ),
        (
            &["--swaps", "--swaps-from", "a.example=1"][..],
            "not a hub URL",
        ),
        (
            &["--swaps", "--swaps-from", "http://a.example=-1"][..],
            "cap",
        ),
        (
            &["--swaps", "--swaps-margin", "2"][..],
            "--swaps-margin must exceed",
        ),
        (
            &["--swaps", "--swaps-min", "2", "--swaps-max", "1"][..],
            "--swaps-min must not exceed",
        ),
    ] {
        let out = output(&missing, &[&["run"], args].concat());
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    assert!(!missing.exists());
}

/// Give the hub at `data` an own lock under `hash`, which it may yet pay,
/// as for a deposit, a Lightning receipt or a swap: the lock's id.
fn own_lock(data: &Path, hash: [u8; 32]) -> String {
    let (seed, db) = (data.join("seed"), data.join("hub.sqlite"));
    let hub = Hub::open(&seed, &db, Policy::default()).unwrap();
    let mut chain = chain::Chain::new();
    hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
    for _ in 0..3 {
        let block = chain.mine(None).clone();
        hub.observe(chain.height(), &block).unwrap();
    }
    assert!(hub.synced(chain.height(), &chain.tip()).unwrap());
    let checkpoint = hub.checkpoint().unwrap().commitment();
    let profile = ChainProfile::Core30DefaultRegtest;
    let terms = TimedTerms::new(profile, checkpoint, chain.height() + 20, 3, hash).unwrap();
    let e = Ephemeral::random();
    let payee = OwnerKeys::from_seed(&[3; 32]).unwrap().address();
    let pay =
        spend::Output::from_derivation(&e.derive(&payee, 4, Branch::Pay, 0).unwrap()).unwrap();
    let keyset = *hub.info().keysets[0].id();
    let lock = hub.lock_own(&keyset, e.public(), vec![pay], terms).unwrap();
    hex::encode(lock.proposal.id().as_bytes())
}

/// What the hub run with `args` logs, once it exited unsuccessfully.
fn refused(data: &Path, args: &[&str]) -> String {
    let mut running = Running(
        hub(data, args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let status = until("the hub exits", || running.0.try_wait().unwrap());
    assert!(!status.success());
    let mut stderr = String::new();
    let mut pipe = running.0.stderr.take().unwrap();
    pipe.read_to_string(&mut stderr).unwrap();
    stderr
}

/// The hub at `data` run with `args`, once it serves at `port`.
fn serving(data: &Path, port: u16, args: &[&str]) -> Running {
    let listen = format!("127.0.0.1:{port}");
    let running = Running(
        hub(data, &[&["run", "--listen", &listen], args].concat())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let health = until("health", || get(port, "/health"));
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    running
}

#[test]
fn a_hub_refuses_to_run_with_own_locks_nothing_backs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("hub");
    stdout(&data, &["init", "--max-pow", "6"]);
    let hash = [5; 32];
    let lock = own_lock(&data, hash);
    // Its deposit, receipt or swap is in no file, as once one was restored
    // older than hub.sqlite: the hub would pay notes nothing backs.
    let listen = format!("127.0.0.1:{}", free_port());
    let stderr = refused(&data, &["run", "--listen", &listen]);
    assert!(
        stderr.contains(&lock) && stderr.contains("--allow-unbacked-locks"),
        "{stderr}"
    );
    for file in ["deposits.sqlite", "lightning.sqlite", "swaps"] {
        assert!(!data.join(file).exists(), "{file}");
    }

    // Allowed, it runs, and alerts the operator.
    let running = serving(&data, free_port(), &["--allow-unbacked-locks"]);
    let events = stdout(&data, &["events"]);
    let alert = events.lines().find(|l| l.contains("\"alert\"")).unwrap();
    assert!(alert.contains(&lock), "{alert}");
    drop(running);

    // A deposit under its hash backs it: the hub runs.
    let deposits = rusqlite::Connection::open(data.join("deposits.sqlite")).unwrap();
    let table = "CREATE TABLE deposits (hash BLOB PRIMARY KEY)";
    deposits.execute(table, []).unwrap();
    let row = "INSERT INTO deposits (hash) VALUES (?1)";
    deposits.execute(row, [hash]).unwrap();
    drop(deposits);
    drop(serving(&data, free_port(), &[]));
}

#[test]
fn a_hub_provides_the_swaps_into_it_from_a_wallet_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("hub");
    stdout(&data, &["init", "--max-pow", "6"]);
    let chain = ["--chain", "http://127.0.0.1:1", "--network", "regtest"];
    let swaps = ["--swaps", "--swaps-from", "http://a.example/=500"];
    let run = |port: u16, args: &[&str]| {
        let listen = format!("127.0.0.1:{port}");
        let args = [&["run", "--listen", &listen][..], &chain[..], args].concat();
        hub(&data, &args)
    };
    let port = free_port();
    let mut running = Running(
        run(port, &[&swaps[..], &["--swaps-fee-base", "7"]].concat())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    until("health", || get(port, "/health"));
    let body = |response: String| response.split("\r\n\r\n").nth(1).unwrap().to_owned();
    let info: licash_api::swaps::Info =
        serde_json::from_str(&body(get(port, licash_api::swaps::SWAPS_INFO).unwrap())).unwrap();
    let hub_info: licash_api::Info =
        serde_json::from_str(&body(get(port, licash_api::INFO).unwrap())).unwrap();
    // A wallet of its own, beside the hub's notes.
    assert_ne!(info.address, hub_info.address);
    assert_eq!(info.fee.base, 7);
    // Following no chain yet, the hub swaps nothing into itself.
    let own = info.hub(&hub_info.address).unwrap();
    assert_eq!(own.max, 0);
    let swaps_dir = data.join("swaps");
    for file in ["wallet.sqlite", "swaps.sqlite"] {
        assert!(swaps_dir.join(file).exists(), "{file}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&swaps_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    let pid = running.0.id().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status();
    assert!(killed.unwrap().success());
    assert!(until("stop", || running.0.try_wait().unwrap()).success());
    let mut logs = String::new();
    let mut stderr = running.0.stderr.take().unwrap();
    stderr.read_to_string(&mut logs).unwrap();
    assert!(logs.contains("swap provider created"), "{logs}");

    // Started again on the same wallet; without --swaps, no swaps served,
    // the operator warned of them.
    let port = free_port();
    let again = Running(
        run(port, &swaps)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("restart", || get(port, "/health"));
    let response = get(port, licash_api::swaps::SWAPS_INFO).unwrap();
    let same: licash_api::swaps::Info = serde_json::from_str(&body(response)).unwrap();
    assert_eq!(same.address, info.address);
    drop(again);
    let port = free_port();
    let mut off = Running(
        run(port, &[])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    until("run without swaps", || get(port, "/health"));
    let response = get(port, licash_api::swaps::SWAPS_INFO).unwrap();
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    off.0.kill().unwrap();
    let mut logs = String::new();
    off.0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    assert!(logs.contains("--swaps is off"), "{logs}");
    // Nor at all while a swap into it is under way: its notes are backed
    // once the provider claims the client's lock.
    let db = rusqlite::Connection::open(swaps_dir.join("swaps.sqlite")).unwrap();
    db.execute(
        "INSERT INTO swaps (id, hash, from_url, to_url, address, amount, fee, paid,
            expiry_from, expiry, state, created_at, updated_at, kind)
         VALUES (x'01', x'02', 'http://a.example', 'http://hub', x'03', 10, 1, 11,
            50, 40, 'locked', 0, 0, 'own')",
        [],
    )
    .unwrap();
    drop(db);
    let out = run(free_port(), &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--swaps is off") && stderr.contains("1 swaps"),
        "{stderr}"
    );

    // Its wallet and its swaps go together.
    std::fs::remove_file(swaps_dir.join("swaps.sqlite")).unwrap();
    let out = run(free_port(), &swaps).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("one is missing"), "{stderr}");
}

#[test]
fn an_onion_service_may_be_the_only_url_announced() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none");
    let onion = ["--nostr-relay", "ws://127.0.0.1:1", "--onion"];
    let chain = ["--chain", "http://node:8332"];
    for (args, says) in [
        (&onion[..], "needs --chain"),
        (
            &[&onion[..], &["--announce-url", "ftp://hub.example"]].concat(),
            "ftp://",
        ),
        // Checked before the hub opens, where this one stops.
        (&[&onion[..], &chain[..]].concat(), "open the hub"),
    ] {
        let out = output(&missing, &[&["run"], args].concat());
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    assert!(!missing.exists());
}

/// A JSON webhook on a thread: its `json://` URL, and the titles it took.
fn webhook() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("json://{}/hook", listener.local_addr().unwrap());
    let titles = Arc::new(Mutex::new(Vec::new()));
    let took = titles.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut reader = BufReader::new(stream.unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            took.lock()
                .unwrap()
                .push(body["title"].as_str().unwrap().to_owned());
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .unwrap();
        }
    });
    (url, titles)
}

#[test]
fn the_operator_is_notified_of_starts_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("hub");
    stdout(&data, &["init", "--max-pow", "6"]);
    let (url, titles) = webhook();
    let taken = || titles.lock().unwrap().clone();

    // Refused without quoting the URL, which holds the token.
    let out = output(&data, &["run", "--notify", "tgram://123:SecretPart"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("notification target 1") && stderr.contains("chat"),
        "{stderr}"
    );
    assert!(!stderr.contains("SecretPart"), "{stderr}");
    let help = hub(&data, &["run", "--help"])
        .env("LICASH_HUB_NOTIFY", "tgram://123:SecretPart/1")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("SecretPart"));
    let out = output(
        &data,
        &["run", "--notify", &url, "--notify-kinds", "minted"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("double_spend"));

    let sent = stdout(&data, &["test-notify", "--notify", &url]);
    let shown = url.replace("/hook", "/***");
    assert_eq!(sent.trim(), format!("sent    {shown}"));
    assert_eq!(taken(), ["licash-hub: Test"]);
    let closed = format!("json://127.0.0.1:{}/hook", free_port());
    let out = output(
        &data,
        &["test-notify", "--notify", &format!("{url},{closed}")],
    );
    assert!(!out.status.success());
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(
        printed.contains(&format!("failed  {}", closed.replace("/hook", "/***"))),
        "{printed}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 of 2 targets failed"));
    titles.lock().unwrap().clear();

    let port = free_port();
    let mut running = Running(
        hub(&data, &["run", "--listen", &format!("127.0.0.1:{port}")])
            .env("LICASH_HUB_NOTIFY", &url)
            .env("LICASH_HUB_NAME", "north")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // A new target hears of the start it was added for.
    until("started", || (!taken().is_empty()).then_some(()));
    assert_eq!(taken(), ["north: Hub started"]);
    assert!(data.join("notify.json").exists());
    let pid = running.0.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    assert!(until("stop", || running.0.try_wait().unwrap()).success());
    assert_eq!(taken(), ["north: Hub started", "north: Hub stopped"]);
}
