//! The relay: each target gets the outbox in order, from its own place.

use std::{
    future::Future,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{body::Bytes, http::StatusCode};
use licash_notify::{Item, Message, Outbox, Priority, Relay, Target, Timing};
use serde_json::Value;

/// An outbox in memory.
#[derive(Default)]
struct Fake {
    items: Mutex<Vec<Item>>,
}

impl Fake {
    fn push(&self, kind: &str) -> u64 {
        let mut items = self.items.lock().unwrap();
        let seq = items.len() as u64 + 1;
        let message = Message::new(Priority::Low, format!("event {seq}"), kind);
        items.push(Item {
            seq,
            kind: kind.to_owned(),
            message,
        });
        seq
    }

    /// As if the database were replaced by an older copy.
    fn truncate(&self, len: usize) {
        self.items.lock().unwrap().truncate(len);
    }
}

impl Outbox for Fake {
    fn after(&self, after: u64, limit: u32) -> anyhow::Result<Vec<Item>> {
        let items = self.items.lock().unwrap();
        Ok(items
            .iter()
            .filter(|i| i.seq > after)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn last(&self) -> anyhow::Result<u64> {
        Ok(self.items.lock().unwrap().len() as u64)
    }
}

/// A JSON webhook that records the titles it took.
struct Hook {
    addr: SocketAddr,
    titles: Arc<Mutex<Vec<String>>>,
    types: Arc<Mutex<Vec<String>>>,
    /// Requests to answer 500 before taking any.
    fail: Arc<Mutex<usize>>,
}

impl Hook {
    async fn start() -> Self {
        let titles = Arc::new(Mutex::new(Vec::new()));
        let types = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(Mutex::new(0));
        let state = (titles.clone(), types.clone(), fail.clone());
        let app = axum::Router::new().fallback(move |body: Bytes| {
            let (titles, types, fail) = state.clone();
            async move {
                {
                    let mut fail = fail.lock().unwrap();
                    if *fail > 0 {
                        *fail -= 1;
                        return StatusCode::SERVICE_UNAVAILABLE;
                    }
                }
                let body: Value = serde_json::from_slice(&body).unwrap();
                titles
                    .lock()
                    .unwrap()
                    .push(body["title"].as_str().unwrap().to_owned());
                types
                    .lock()
                    .unwrap()
                    .push(body["type"].as_str().unwrap().to_owned());
                StatusCode::OK
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            titles,
            types,
            fail,
        }
    }

    fn target(&self) -> Target {
        format!("json://{}/hook", self.addr).parse().unwrap()
    }

    fn titles(&self) -> Vec<String> {
        self.titles.lock().unwrap().clone()
    }

    fn fail(&self, times: usize) {
        *self.fail.lock().unwrap() = times;
    }
}

fn titles(seqs: impl IntoIterator<Item = u64>) -> Vec<String> {
    seqs.into_iter().map(|s| format!("event {s}")).collect()
}

const FAST: Timing = Timing {
    poll: Duration::from_millis(10),
    retry: Duration::from_millis(10),
    max_retry: Duration::from_millis(40),
};

fn relay(outbox: &Arc<Fake>, targets: Vec<Target>, cursors: &Path) -> Relay {
    Relay::new(outbox.clone(), targets, cursors)
        .unwrap()
        .timing(FAST)
}

/// Runs a relay until dropped.
struct Running(tokio::task::JoinHandle<()>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn run(relay: Relay) -> Running {
    Running(tokio::spawn(relay.run()))
}

/// The place recorded for `target`.
fn cursor(cursors: &Path, target: &Target) -> Option<u64> {
    let file: Value = serde_json::from_slice(&std::fs::read(cursors).ok()?).ok()?;
    assert_eq!(file["version"], 1);
    file["targets"][target.id()].as_u64()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn settle<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

#[tokio::test]
async fn a_new_target_hears_what_happens_from_then_on_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    outbox.push("started");
    outbox.push("released");
    let hook = Hook::start().await;
    let _running = run(relay(&outbox, vec![hook.target()], &cursors));
    until("start recorded", || {
        cursor(&cursors, &hook.target()) == Some(2)
    })
    .await;
    for _ in 0..3 {
        outbox.push("released");
    }
    until("delivered", || hook.titles().len() == 3).await;
    assert_eq!(hook.titles(), titles(3..=5));
    until("recorded", || cursor(&cursors, &hook.target()) == Some(5)).await;
}

#[tokio::test]
async fn new_targets_can_start_earlier_but_never_past_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    for _ in 0..3 {
        outbox.push("released");
    }
    let (early, late) = (Hook::start().await, Hook::start().await);
    let _early = run(relay(&outbox, vec![early.target()], &cursors).new_targets_after(1));
    until("delivered", || early.titles().len() == 2).await;
    assert_eq!(early.titles(), titles(2..=3));
    let _late = run(
        relay(&outbox, vec![late.target()], &dir.path().join("late.json")).new_targets_after(9),
    );
    until("started", || {
        cursor(&dir.path().join("late.json"), &late.target()) == Some(3)
    })
    .await;
    outbox.push("released");
    until("delivered", || late.titles().len() == 1).await;
    assert_eq!(late.titles(), titles([4]));
}

#[tokio::test]
async fn a_failed_notification_is_sent_again_before_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let hook = Hook::start().await;
    let _running = run(relay(&outbox, vec![hook.target()], &cursors));
    until("started", || cursor(&cursors, &hook.target()) == Some(0)).await;
    hook.fail(4);
    outbox.push("released");
    outbox.push("double_spend");
    until("delivered", || hook.titles().len() == 2).await;
    assert_eq!(hook.titles(), titles(1..=2));
    assert_eq!(*hook.fail.lock().unwrap(), 0);
}

#[tokio::test]
async fn each_target_keeps_its_own_place() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let (up, down) = (Hook::start().await, Hook::start().await);
    let _running = run(relay(&outbox, vec![up.target(), down.target()], &cursors));
    until("started", || cursor(&cursors, &down.target()) == Some(0)).await;
    down.fail(usize::MAX);
    for _ in 0..3 {
        outbox.push("released");
    }
    until("up delivered", || cursor(&cursors, &up.target()) == Some(3)).await;
    assert_eq!(up.titles(), titles(1..=3));
    assert!(down.titles().is_empty());
    assert_eq!(cursor(&cursors, &down.target()), Some(0));
    down.fail(0);
    until("down delivered", || down.titles().len() == 3).await;
    assert_eq!(down.titles(), titles(1..=3));
}

#[tokio::test]
async fn a_restart_resumes_each_target_and_starts_new_ones_at_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let (old, new) = (Hook::start().await, Hook::start().await);
    let running = run(relay(&outbox, vec![old.target()], &cursors));
    until("started", || cursor(&cursors, &old.target()) == Some(0)).await;
    outbox.push("released");
    outbox.push("released");
    until("recorded", || cursor(&cursors, &old.target()) == Some(2)).await;
    drop(running);
    // While stopped.
    outbox.push("released");
    let _running = run(relay(&outbox, vec![old.target(), new.target()], &cursors));
    until("new started", || cursor(&cursors, &new.target()) == Some(3)).await;
    outbox.push("double_spend");
    until("delivered", || {
        old.titles().len() == 4 && new.titles().len() == 1
    })
    .await;
    assert_eq!(old.titles(), titles(1..=4));
    assert_eq!(new.titles(), titles([4]));
}

#[tokio::test]
async fn events_that_went_back_are_reported_then_followed() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let hook = Hook::start().await;
    let _running = run(relay(&outbox, vec![hook.target()], &cursors));
    until("started", || cursor(&cursors, &hook.target()) == Some(0)).await;
    for _ in 0..3 {
        outbox.push("released");
    }
    until("delivered", || hook.titles().len() == 3).await;
    outbox.truncate(1);
    until("reported", || hook.titles().len() == 4).await;
    assert_eq!(hook.titles()[3], "Events went back");
    assert_eq!(hook.types.lock().unwrap()[3], "failure");
    until("reset", || cursor(&cursors, &hook.target()) == Some(1)).await;
    assert_eq!(outbox.push("started"), 2);
    until("followed", || hook.titles().len() == 5).await;
    assert_eq!(hook.titles()[4], "event 2");
}

#[tokio::test]
async fn only_the_kinds_asked_for_are_sent() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let hook = Hook::start().await;
    let relay = relay(&outbox, vec![hook.target()], &cursors).only(["double_spend".to_owned()]);
    let _running = run(relay);
    until("started", || cursor(&cursors, &hook.target()) == Some(0)).await;
    for kind in ["started", "double_spend", "released", "released"] {
        outbox.push(kind);
    }
    // The events passed over at the end are recorded too.
    until("recorded", || cursor(&cursors, &hook.target()) == Some(4)).await;
    assert_eq!(hook.titles(), titles([2]));
}

#[tokio::test]
async fn kinds_passed_over_are_not_sent_and_the_rest_are() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox = Arc::new(Fake::default());
    let hook = Hook::start().await;
    let relay = relay(&outbox, vec![hook.target()], &cursors).except(["locked".to_owned()]);
    let _running = run(relay);
    until("started", || cursor(&cursors, &hook.target()) == Some(0)).await;
    // A kind no version knows yet is sent too.
    for kind in ["locked", "settled", "minted", "locked"] {
        outbox.push(kind);
    }
    until("recorded", || cursor(&cursors, &hook.target()) == Some(4)).await;
    assert_eq!(hook.titles(), titles([2, 3]));
}

#[tokio::test]
async fn alerts_go_to_every_target_and_give_up() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Arc::new(Fake::default());
    let (up, down) = (Hook::start().await, Hook::start().await);
    down.fail(usize::MAX);
    let relay = relay(
        &outbox,
        vec![up.target(), down.target()],
        &dir.path().join("n.json"),
    );
    let halted = Message::new(Priority::High, "Hub halted", "Restart it.");
    settle(relay.alert(&halted)).await;
    assert_eq!(up.titles(), ["Hub halted"]);
    assert!(down.titles().is_empty());
    // Alerts record nothing.
    assert!(!dir.path().join("n.json").exists());
}

#[test]
fn a_damaged_or_foreign_cursor_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cursors = dir.path().join("notify.json");
    let outbox: Arc<Fake> = Arc::default();
    let target: Target = "json://127.0.0.1:1/hook".parse().unwrap();
    for (content, refusal) in [
        ("{\"version\":1,", "damaged"),
        ("{\"version\":2,\"targets\":{}}", "version 2"),
    ] {
        std::fs::write(&cursors, content).unwrap();
        let e = Relay::new(outbox.clone(), vec![target.clone()], &cursors)
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains(refusal), "{e:#}");
    }
    std::fs::write(&cursors, "{\"version\":1,\"targets\":{}}").unwrap();
    assert_eq!(
        Relay::new(outbox, vec![target], &cursors)
            .unwrap()
            .targets()
            .len(),
        1
    );
}
