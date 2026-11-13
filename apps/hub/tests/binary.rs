//! The `licash-hub` binary end to end: create, run, watch and stop a hub.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

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

    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let mut running = Running(
        hub(
            &data,
            &["run", "--listen", &listen, "--spend-fee-base", "3"],
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
