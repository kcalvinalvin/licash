use std::path::Path;

use licash::spend::ChainProfile;
use licash_dashboard::{Panel, Report, Tone, Value};
use licash_swaps::{Config, Provider};
use licash_wallet::{Response, Transport, Wallet};
use rusqlite::{params, Connection};

use super::*;

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

struct Offline;

impl Transport for Offline {
    fn request(
        &self,
        _: &str,
        _: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        std::future::ready(Err("offline".into()))
    }
}

fn dashboard(dir: &Path) -> Report {
    let records = read(dir).unwrap();
    pages::report(&records, NOW, licash_dashboard::unasked(), dir)
}

fn check_screens(report: &Report) {
    for page in 0..report.pages.len() {
        for (width, height) in [(80, 30), (140, 44)] {
            if std::env::var_os("DASHBOARD_SCREEN").is_some() {
                println!("SWAPS page {page} {width}x{height}");
            }
            let screen = licash_dashboard::testing::screen(report, page, width, height);
            assert!(screen.contains("q quit"), "{screen}");
            assert!(screen.contains("? help"), "{screen}");
            assert!(
                screen.lines().next().unwrap().contains("regtest"),
                "{screen}"
            );
        }
    }
}

fn summed(report: &Report, series: &str, label: &str) -> (usize, i64) {
    let series = report.series.iter().find(|s| s.id == series).unwrap();
    let Some(label) = series.labels.iter().position(|l| l == label) else {
        return (0, 0);
    };
    let facts = series.facts.iter().filter(|f| f.label as usize == label);
    facts.fold((0, 0), |(n, sum), f| (n + 1, sum + f.value))
}

fn panels(report: &Report) -> impl Iterator<Item = &Panel> {
    report.pages.iter().flat_map(|p| p.rows.iter().flatten())
}

fn check(report: &Report, name: &str) -> (Tone, String) {
    let checks = panels(report).find_map(|p| match p {
        Panel::Health(checks) => Some(checks),
        _ => None,
    });
    let check = checks.unwrap().iter().find(|c| c.name == name).unwrap();
    (check.tone, check.detail.clone())
}

/// The value of the card `title` shows now.
fn now_card(report: &Report, title: &str) -> (Option<i64>, String) {
    let cards = panels(report).flat_map(|p| match p {
        Panel::Cards(cards) => &cards[..],
        _ => &[],
    });
    match cards
        .map(|c| (&c.title, &c.value))
        .find(|(t, _)| *t == title)
    {
        Some((_, Value::Now { value, detail, .. })) => (*value, detail.clone()),
        _ => panic!("no card {title} of a value now"),
    }
}

fn table<'a>(report: &'a Report, title: &str) -> &'a licash_dashboard::Table {
    let tables = panels(report).filter_map(|p| match p {
        Panel::Table(t) => Some(t),
        _ => None,
    });
    tables.into_iter().find(|t| t.title == title).unwrap()
}

/// What the files hold, byte for byte, to show none was written.
fn contents(dir: &Path) -> Vec<Vec<u8>> {
    ["swaps.sqlite", "wallet.sqlite"]
        .iter()
        .map(|f| std::fs::read(dir.join(f)).unwrap())
        .collect()
}

fn version(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// A hub's Info, following Bitcoin at `height` unless none.
fn info(height: Option<u32>) -> String {
    let fixture = include_str!("../../../../crates/licash-wallet/tests/fixtures/info.json");
    let mut info: serde_json::Value = serde_json::from_str(fixture).unwrap();
    info["locks"] = match height {
        Some(height) => serde_json::json!({
            "checkpoint": "00", "height": height, "window": 1000, "reaction": 6,
            "min_depth": 1, "max_headers": 2000,
        }),
        None => serde_json::Value::Null,
    };
    info.to_string()
}

/// A provider as this version creates it, of two hubs, `a.test` and
/// `b.test`, both at height 1000, `b.test` not heard from in two hours.
fn provider(dir: &Path) -> (Connection, Connection) {
    let wallet = Wallet::create(
        &dir.join("wallet.sqlite"),
        &[5; 32],
        ChainProfile::Core30DefaultRegtest,
        Offline,
    )
    .unwrap();
    drop(Provider::create(&dir.join("swaps.sqlite"), wallet, Config::default()).unwrap());
    let wallet = Connection::open(dir.join("wallet.sqlite")).unwrap();
    for (id, url, heard) in [
        (1, "http://a.test", NOW - 60),
        (2, "http://b.test/", NOW - 2 * HOUR),
    ] {
        wallet
            .execute(
                "INSERT INTO hubs (id, info, updated_at, trusted) VALUES (?1, ?2, ?3, 1)",
                params![id, info(Some(1000)), heard],
            )
            .unwrap();
        wallet
            .execute(
                "INSERT INTO hub_urls (url, hub) VALUES (?1, ?2)",
                params![url, id],
            )
            .unwrap();
        wallet
            .execute(
                "INSERT INTO keysets (id, hub, bytes) VALUES (?1, ?2, X'00')",
                params![vec![id as u8; 32], id],
            )
            .unwrap();
    }
    let swaps = Connection::open(dir.join("swaps.sqlite")).unwrap();
    (swaps, wallet)
}

/// A note of `denom` at hub `hub`, the only one of it there, spent by
/// `spent_by` or gone at `gone`.
fn note(
    wallet: &Connection,
    hub: u8,
    denom: i64,
    added: i64,
    spent_by: Option<[u8; 32]>,
    gone: Option<i64>,
    unlinked: i64,
) {
    wallet
        .execute(
            "INSERT INTO notes (y, keyset, denom, note, proposal, ephemeral, branch, position,
                spent_by, unlinked, gone_at, added_at)
             VALUES (?1, ?2, ?3, X'00', X'00', X'00', 0, 0, ?4, ?5, ?6, ?7)",
            params![
                format!("{hub} {denom}").into_bytes(),
                vec![hub; 32],
                denom,
                spent_by.map(|s| s.to_vec()),
                unlinked,
                gone,
                added
            ],
        )
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn swap(
    swaps: &Connection,
    n: u8,
    kind: &str,
    to: &str,
    amount: i64,
    fee: i64,
    expiry_from: i64,
    state: &str,
    reason: Option<&str>,
    created: i64,
    updated: i64,
) {
    let from = if to == "http://a.test" {
        "http://b.test/"
    } else {
        "http://a.test"
    };
    swaps
        .execute(
            "INSERT INTO swaps (id, hash, from_url, to_url, address, amount, fee, paid,
                expiry_from, expiry, state, reason, created_at, updated_at, kind)
             VALUES (?1, ?2, ?3, ?4, X'02', ?5, ?6, ?5 + ?6, ?7, 1100, ?8, ?9, ?10, ?11, ?12)",
            params![
                vec![n; 32],
                vec![n + 100; 32],
                from,
                to,
                amount,
                fee,
                expiry_from,
                state,
                reason,
                created,
                updated,
                kind
            ],
        )
        .unwrap();
}

#[test]
fn the_dashboard_reads_swaps_fees_losses_and_notes_from_the_records() {
    let dir = tempfile::tempdir().unwrap();
    let (swaps, wallet) = provider(dir.path());
    // At a.test: 1024 held, unlinked; 512 spent by a send two days ago;
    // 256 gone, spent by a copy of the wallet. At b.test: 64 held.
    let send = [9; 32];
    wallet
        .execute(
            "INSERT INTO sends (id, hub, payee, amount, fee, offer, created_at)
             VALUES (?1, 1, X'00', 512, 0, '{}', ?2)",
            params![send.to_vec(), NOW - 2 * DAY],
        )
        .unwrap();
    note(&wallet, 1, 1024, NOW - 5 * DAY, None, None, 1);
    note(&wallet, 1, 512, NOW - 5 * DAY, Some(send), None, 0);
    note(&wallet, 1, 256, NOW - 4 * DAY, None, Some(NOW - 3 * DAY), 0);
    note(&wallet, 2, 64, NOW - DAY, None, None, 0);
    let (a, b, chain) = ("http://a.test", "http://b.test/", "bitcoin:regtest");
    let lost = Some("the client's lock was not claimed: expired");
    let refused = Some("its hub refused the lock: too big");
    swap(
        &swaps,
        1,
        "notes",
        b,
        1000,
        10,
        1050,
        "done",
        None,
        NOW - 3 * DAY,
        NOW - 3 * DAY + 60,
    );
    swap(
        &swaps,
        2,
        "chain",
        chain,
        5000,
        50,
        1050,
        "done",
        None,
        NOW - 2 * DAY,
        NOW - 2 * DAY,
    );
    swap(
        &swaps,
        3,
        "notes",
        b,
        300,
        3,
        1050,
        "refunded",
        None,
        NOW - 2 * DAY,
        NOW - DAY,
    );
    swap(
        &swaps,
        4,
        "notes",
        a,
        200,
        2,
        1050,
        "failed",
        refused,
        NOW - HOUR,
        NOW - HOUR,
    );
    swap(
        &swaps,
        5,
        "notes",
        b,
        400,
        4,
        1050,
        "lost",
        lost,
        NOW - 3 * DAY,
        NOW - 2 * DAY,
    );
    // Paid: the client's lock at a.test to claim within 3 blocks.
    swap(
        &swaps,
        6,
        "notes",
        b,
        100,
        1,
        1003,
        "paid",
        None,
        NOW - 600,
        NOW - 600,
    );
    // Taken two hours ago, locking 50 at a.test.
    swap(
        &swaps,
        7,
        "notes",
        a,
        50,
        1,
        1050,
        "accepted",
        None,
        NOW - 2 * HOUR,
        NOW - 2 * HOUR,
    );
    swaps
        .execute(
            "INSERT INTO events (at, kind, json) VALUES (?1, 'alert', ?2)",
            params![
                NOW - HOUR,
                r#"{"kind":"alert","message":"No swaps to b.test for now"}"#
            ],
        )
        .unwrap();
    drop((swaps, wallet));
    let before = contents(dir.path());

    let report = dashboard(dir.path());
    check_screens(&report);
    let route = "a.test → b.test";
    assert_eq!(summed(&report, "volume", route), (1, 1000));
    assert_eq!(summed(&report, "fees", route), (1, 10));
    assert_eq!(summed(&report, "volume", "a.test → on-chain"), (1, 5000));
    assert_eq!(summed(&report, "fees", "a.test → on-chain"), (1, 50));
    assert_eq!(summed(&report, "lost", route), (1, 400));
    assert_eq!(summed(&report, "taken", route), (4, 1800));
    assert_eq!(summed(&report, "taken", "b.test → a.test"), (2, 250));
    assert_eq!(summed(&report, "failures", "never claimed"), (1, 300));
    assert_eq!(
        summed(&report, "failures", "its hub refused the lock"),
        (1, 200)
    );
    assert_eq!(
        summed(&report, "failures", "the client's lock was not claimed"),
        (1, 400)
    );
    // Every note came and went: what is left is what the wallet holds.
    assert_eq!(summed(&report, "stock", "a.test"), (5, 1024));
    assert_eq!(summed(&report, "stock", "b.test"), (1, 64));
    assert_eq!(now_card(&report, "Reserved").0, Some(50));
    assert_eq!(now_card(&report, "Free").0, Some(1024 - 50 + 64));
    assert_eq!(
        now_card(&report, "Unlinked"),
        (Some(1), "50% of the 2 notes held".into())
    );
    assert_eq!(
        now_card(&report, "On chain"),
        (Some(0), "0 coins unspent".into())
    );
    assert_eq!(
        now_card(&report, "Under way"),
        (Some(2), "1 paid, claiming".into())
    );
    let under_way = table(&report, "Under way");
    assert_eq!(under_way.rows.len(), 2);
    assert_eq!(under_way.rows[0].tone, Tone::Bad);
    assert_eq!(table(&report, "Failed swaps").rows.len(), 3);

    assert_eq!(check(&report, "Swaps").0, Tone::Bad);
    assert_eq!(
        check(&report, "Stock"),
        (Tone::Good, "notes free at every hub".into())
    );
    let (tone, hubs) = check(&report, "Hubs");
    assert_eq!(tone, Tone::Warn);
    assert!(hubs.contains("b.test for 2h"), "{hubs}");
    assert_eq!(
        check(&report, "Losses"),
        (Tone::Bad, "1 in the last week, 400 sat".into())
    );
    assert_eq!(
        check(&report, "Failures"),
        (Tone::Warn, "1 of 1 in the last day".into())
    );
    assert_eq!(check(&report, "Alerts").0, Tone::Warn);
    let pages: Vec<_> = report.pages.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(pages, ["Overview", "Swaps", "Liquidity", "Fees", "Health"]);
    assert_eq!(contents(dir.path()), before);
}

/// The files of each older version, read as they are and left so: no
/// migration, nothing written.
#[test]
fn files_of_older_versions_are_read_as_they_are() {
    let wallets = [
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v1.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v2.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v3.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v4.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v5.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v6.sql"),
        include_str!("../../../../crates/licash-wallet/tests/fixtures/wallet-v7.sql"),
    ];
    for (i, fixture) in wallets.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let swaps = include_str!("../../../../crates/licash-swaps/tests/fixtures/swaps-v1.sql");
        Connection::open(dir.path().join("swaps.sqlite"))
            .unwrap()
            .execute_batch(swaps)
            .unwrap();
        Connection::open(dir.path().join("wallet.sqlite"))
            .unwrap()
            .execute_batch(fixture)
            .unwrap();
        let before = contents(dir.path());

        let report = dashboard(dir.path());
        // The one swap of version 1, of notes, taken and not locked yet.
        assert_eq!(summed(&report, "taken", "a.test → b.test"), (1, 1000));
        assert_eq!(table(&report, "Under way").rows.len(), 1);
        assert_eq!(
            texts(&table(&report, "Recent events").rows[0]),
            ["taken", "a.test → b.test", "1000", "fee 11 sat (1.1%)"]
        );
        let stock = report.series.iter().find(|s| s.id == "stock").unwrap();
        assert_eq!(stock.labels, ["hub.test"], "wallet v{}", i + 1);
        let on_chain = now_card(&report, "On chain").0;
        assert_eq!(on_chain, (i > 0).then_some(0), "wallet v{}", i + 1);
        assert_eq!(contents(dir.path()), before, "wallet v{}", i + 1);
        assert_eq!(version(&dir.path().join("swaps.sqlite")), 1);
        assert_eq!(version(&dir.path().join("wallet.sqlite")), i as i64 + 1);
    }
}

/// A row's cells but its time, as text: amounts in sat.
fn texts(row: &licash_dashboard::Row) -> Vec<String> {
    use licash_dashboard::Cell;
    let text = |c: &Cell| match c {
        Cell::Text(s) => s.clone(),
        Cell::Sat(v) | Cell::Int(v) | Cell::Time(v) | Cell::Blocks(v) => v.to_string(),
        Cell::Empty => String::new(),
    };
    row.cells.iter().skip(1).map(text).collect()
}

fn event(swaps: &Connection, at: i64, json: &str) {
    let kind: serde_json::Value = serde_json::from_str(json).unwrap();
    swaps
        .execute(
            "INSERT INTO events (at, kind, json) VALUES (?1, ?2, ?3)",
            params![at, kind["kind"].as_str().unwrap(), json],
        )
        .unwrap();
}

#[test]
fn recent_events_read_the_notices_newest_first_with_their_swaps() {
    let dir = tempfile::tempdir().unwrap();
    let (swaps, wallet) = provider(dir.path());
    let (a, b) = ("http://a.test", "http://b.test/");
    swap(
        &swaps,
        1,
        "notes",
        b,
        1000,
        10,
        1050,
        "done",
        None,
        NOW - DAY,
        NOW - DAY + 60,
    );
    let refused = Some("its hub refused the lock: too big");
    swap(
        &swaps,
        4,
        "notes",
        a,
        200,
        2,
        1050,
        "failed",
        refused,
        NOW - HOUR,
        NOW - HOUR,
    );
    let (one, four) = (hex::encode([1; 32]), hex::encode([4; 32]));
    event(
        &swaps,
        NOW - DAY,
        &format!(
            r#"{{"kind":"accepted","id":"{one}","amount":1000,"fee":10,"from":"{a}","to":"{b}"}}"#
        ),
    );
    event(
        &swaps,
        NOW - DAY + 60,
        &format!(r#"{{"kind":"done","id":"{one}","amount":1000,"fee":10}}"#),
    );
    event(
        &swaps,
        NOW - HOUR,
        &format!(
            r#"{{"kind":"failed","id":"{four}","reason":"its hub refused the lock: too big"}}"#
        ),
    );
    // Of a swap not recorded, and of a newer provider.
    event(
        &swaps,
        NOW - 60,
        r#"{"kind":"lost","id":"00","amount":5,"reason":"expired"}"#,
    );
    event(&swaps, NOW - 30, r#"{"kind":"swap_paused","id":"00"}"#);
    event(
        &swaps,
        NOW,
        r#"{"kind":"alert","message":"No swaps to b.test for now"}"#,
    );
    drop((swaps, wallet));

    let report = dashboard(dir.path());
    let recent = table(&report, "Recent events");
    assert!(recent.in_range);
    let rows: Vec<_> = recent
        .rows
        .iter()
        .map(|r| (r.at, r.tone, texts(r)))
        .collect();
    assert_eq!(
        rows,
        [
            (
                Some(NOW),
                Tone::Warn,
                vec!["alert", "", "", "No swaps to b.test for now"]
            ),
            (Some(NOW - 30), Tone::Muted, vec!["swap paused", "", "", ""]),
            (Some(NOW - 60), Tone::Bad, vec!["lost", "", "5", "expired"]),
            (
                Some(NOW - HOUR),
                Tone::Warn,
                vec![
                    "failed",
                    "b.test → a.test",
                    "200",
                    "its hub refused the lock: too big"
                ]
            ),
            (
                Some(NOW - DAY + 60),
                Tone::Good,
                vec!["done", "a.test → b.test", "1000", "earned 10 sat (1.0%)"]
            ),
            (
                Some(NOW - DAY),
                Tone::Normal,
                vec!["taken", "a.test → b.test", "1000", "fee 10 sat (1.0%)"]
            ),
        ]
        .map(|(at, tone, cells)| (at, tone, cells.into_iter().map(String::from).collect()))
    );
    // On the overview, below the rest.
    let overview = &report.pages[0];
    assert!(matches!(
        overview.rows.last().unwrap()[..],
        [Panel::Table(ref t)] if t.title == "Recent events"
    ));
}

#[test]
fn recent_events_are_the_latest_only() {
    let dir = tempfile::tempdir().unwrap();
    let (swaps, wallet) = provider(dir.path());
    for i in 0..RECENT as i64 + 5 {
        event(&swaps, NOW - 1000 + i, r#"{"kind":"alert","message":"x"}"#);
    }
    drop((swaps, wallet));

    let report = dashboard(dir.path());
    let recent = &table(&report, "Recent events").rows;
    assert_eq!(recent.len(), RECENT);
    assert_eq!(recent[0].at, Some(NOW - 1000 + RECENT as i64 + 4));
    assert_eq!(recent[RECENT - 1].at, Some(NOW - 1000 + 5));
}

#[test]
fn files_of_newer_versions_or_none_are_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let err = read(dir.path()).err().unwrap().to_string();
    assert!(err.starts_with("no swaps file at"), "{err}");
    drop(provider(dir.path()));
    let conn = Connection::open(dir.path().join("swaps.sqlite")).unwrap();
    conn.pragma_update(None, "user_version", 4).unwrap();
    let err = read(dir.path()).err().unwrap().to_string();
    assert!(
        err.ends_with("is of version 4, newer than this provider reads"),
        "{err}"
    );
}

#[test]
fn hubs_read_as_their_hosts() {
    assert_eq!(short("http://a.test"), "a.test");
    assert_eq!(
        short("https://hub.example.com:8338/"),
        "hub.example.com:8338"
    );
    assert_eq!(short("bitcoin:regtest"), "on-chain");
    let onion = format!("http://{}.onion", "abcdefghij".repeat(5) + "klmnop");
    assert_eq!(short(&onion), "abcdefgh….onion");
}
