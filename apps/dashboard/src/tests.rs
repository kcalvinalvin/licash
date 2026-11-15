//! The dashboard over a small report: what the view works out, what the
//! terminal shows, and how keys move it.

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;

use crate::app::{Status, Ui};
use crate::view::{Number, PanelView, View};
use crate::*;

const DAY: i64 = 86_400;
const NOW: i64 = 1_794_700_800 + 13 * 3600; // 2026-11-15 13:00Z

fn report() -> Report {
    let mut volume = Series::flow("volume", "Volume", Unit::Sat);
    let mut fees = Series::flow("fees", "Fees", Unit::Sat);
    let mut failures = Series::flow("failures", "Failures", Unit::Sat);
    for day in 0..60 {
        let at = NOW - day * DAY - 3600;
        volume.add(at, "transfers", 100_000 + day * 1_000);
        fees.add(at, "transfers", 100);
        if day % 2 == 0 {
            volume.add(at, "lightning", 50_000);
            fees.add(at, "lightning", 250);
        }
    }
    failures.add(NOW - DAY, "no route", 20_000);
    failures.add(NOW - 2 * DAY, "no route", 10_000);
    failures.add(NOW - 3 * DAY, "timed out", 5_000);
    let mut balance = Series::level("balance", "Balance", Unit::Sat);
    balance.add(NOW - 50 * DAY, "hub a", 1_000_000);
    balance.add(NOW - 50 * DAY, "hub b", 600_000);
    balance.add(NOW - 10 * DAY, "hub b", -300_000);
    let overview = Page {
        title: "Overview".into(),
        rows: vec![
            vec![Panel::Cards(vec![
                Card {
                    title: "Volume".into(),
                    value: Value::Total {
                        plus: vec![Ref::sum("volume")],
                        minus: vec![],
                    },
                    better: Better::Up,
                },
                Card {
                    title: "Payments".into(),
                    value: Value::Total {
                        plus: vec![Ref::count("volume")],
                        minus: vec![],
                    },
                    better: Better::Up,
                },
                Card {
                    title: "Success".into(),
                    value: Value::Ratio {
                        num: vec![Ref::count("volume")],
                        den: vec![Ref::count("volume"), Ref::count("failures")],
                    },
                    better: Better::Up,
                },
                Card {
                    title: "On chain".into(),
                    value: Value::Now {
                        value: None,
                        unit: Unit::Sat,
                        detail: "no chain to ask".into(),
                        tone: Tone::Normal,
                    },
                    better: Better::Neither,
                },
            ])],
            vec![
                Panel::Chart {
                    title: "Volume".into(),
                    series: "volume".into(),
                    measure: Measure::Sum,
                },
                Panel::Breakdown {
                    title: "Fees by kind".into(),
                    series: "fees".into(),
                    columns: vec![
                        Column::Value {
                            title: "fees".into(),
                            of: Ref::sum("fees"),
                        },
                        Column::Share {
                            title: "share".into(),
                            of: Ref::sum("fees"),
                        },
                    ],
                },
            ],
            vec![
                Panel::Table(Table {
                    title: "Failures".into(),
                    columns: vec!["when".into(), "amount".into(), "reason".into()],
                    rows: vec![
                        Row {
                            at: Some(NOW - DAY),
                            tone: Tone::Bad,
                            cells: vec![
                                Cell::Time(NOW - DAY),
                                Cell::Sat(20_000),
                                Cell::Text("no route".into()),
                            ],
                        },
                        Row {
                            at: Some(NOW - 40 * DAY),
                            tone: Tone::Bad,
                            cells: vec![
                                Cell::Time(NOW - 40 * DAY),
                                Cell::Sat(1),
                                Cell::Text("old".into()),
                            ],
                        },
                    ],
                    in_range: true,
                    empty: "no failures".into(),
                }),
                Panel::Health(vec![
                    Check {
                        name: "Service".into(),
                        tone: Tone::Good,
                        detail: "answers".into(),
                    },
                    Check {
                        name: "Liquidity".into(),
                        tone: Tone::Warn,
                        detail: "hub b is low".into(),
                    },
                ]),
            ],
        ],
    };
    let liquidity = Page {
        title: "Liquidity".into(),
        rows: vec![vec![
            Panel::Chart {
                title: "Balances".into(),
                series: "balance".into(),
                measure: Measure::Level,
            },
            Panel::Breakdown {
                title: "Runway".into(),
                series: "balance".into(),
                columns: vec![
                    Column::Value {
                        title: "balance".into(),
                        of: Ref::level("balance"),
                    },
                    Column::Runway {
                        title: "runway".into(),
                        of: "balance".into(),
                    },
                ],
            },
        ]],
    };
    Report {
        title: "licash hub".into(),
        subtitle: "regtest · /data".into(),
        now: NOW,
        since: NOW - 60 * DAY,
        series: vec![volume, fees, failures, balance],
        pages: vec![overview, liquidity],
    }
}

fn cards(view: &View) -> &[view::CardView] {
    match &view.pages[0].rows[0][0] {
        PanelView::Cards { cards } => cards,
        _ => panic!("cards first"),
    }
}

#[test]
fn a_month_totals_and_compares_with_the_month_before() {
    let view = View::new(&report(), Range::Month);
    let cards = cards(&view);
    // Days 0..30 back: 30 transfers and 15 lightning payments.
    assert_eq!(cards[1].value, Some(45.0));
    assert_eq!(cards[1].previous, Some(45.0));
    let transfers: i64 = (0..30).map(|d| 100_000 + d * 1_000).sum();
    assert_eq!(cards[0].value, Some((transfers + 15 * 50_000) as f64));
    assert_eq!(cards[0].trend.len(), 30);
    assert_eq!(cards[2].value, Some(45.0 / 48.0));
    assert_eq!(cards[3].value, None);
    assert_eq!(view.health, Tone::Warn);
}

#[test]
fn tables_keep_the_range_and_breakdowns_rank_by_their_first_column() {
    let view = View::new(&report(), Range::Month);
    let PanelView::Table(t) = &view.pages[0].rows[2][0] else {
        panic!("a table")
    };
    assert_eq!((t.rows.len(), t.hidden), (1, 1));
    let PanelView::Breakdown(b) = &view.pages[0].rows[1][1] else {
        panic!("a breakdown")
    };
    let labels: Vec<&str> = b.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["lightning", "transfers"]);
    let Number::Share(Some(share)) = b.rows[0].cells[1] else {
        panic!("a share")
    };
    assert!((share - 3_750.0 / 6_750.0).abs() < 1e-9);
}

#[test]
fn a_falling_level_has_a_runway() {
    let view = View::new(&report(), Range::Month);
    let PanelView::Breakdown(b) = &view.pages[1].rows[0][1] else {
        panic!("a breakdown")
    };
    let row = |label: &str| b.rows.iter().find(|r| r.label == label).unwrap();
    // hub b fell 300,000 in 30 days to 300,000: 30 days left.
    let Number::Days(Some(days)) = row("hub b").cells[1] else {
        panic!("days")
    };
    assert!((days - 30.0).abs() < 1.5, "{days}");
    assert!(matches!(row("hub a").cells[1], Number::Days(None)));
}

#[test]
fn the_view_prints_as_json() {
    let json = serde_json::to_value(View::new(&report(), Range::Week)).unwrap();
    assert_eq!(json["range"], "7d");
    assert_eq!(json["pages"][0]["rows"][0][0]["type"], "cards");
    assert_eq!(json["pages"][0]["rows"][1][0]["type"], "chart");
    assert_eq!(
        json["pages"][0]["rows"][1][0]["starts"]
            .as_array()
            .unwrap()
            .len(),
        28
    );
}

fn screen(ui: &Ui, width: u16, height: u16) -> String {
    screen_of(&report(), ui, width, height)
}

fn screen_of(report: &Report, ui: &Ui, width: u16, height: u16) -> String {
    let view = View::new(report, ui.range);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let status = Status {
        read: std::time::Duration::from_secs(3),
        reading: false,
        error: None,
    };
    terminal
        .draw(|f| render::draw(f, &view, ui, &status))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    if std::env::var_os("DASHBOARD_SCREEN").is_some() {
        println!("{out}");
    }
    out
}

#[test]
fn the_overview_draws_its_cards_charts_and_lists() {
    let out = screen(&Ui::new(Range::Month), 160, 44);
    for text in [
        "licash hub",
        " 30d ",
        "attention",
        "Overview",
        "Liquidity",
        "Volume",
        "Success",
        "93.8%",
        "n/a",
        "no chain to ask",
        "Fees by kind · 2",
        "■ lightning",
        "Failures · 1",
        "no route",
        "1 more before 30d",
        "hub b is low",
        "q quit",
    ] {
        assert!(out.contains(text), "{text:?} missing from\n{out}");
    }
    assert!(out.contains('█'), "bars\n{out}");
}

#[test]
fn a_column_of_numbers_aligns_right_though_its_newest_row_has_none() {
    let row = |at: i64, amount: Cell, what: &str| Row {
        at: Some(at),
        tone: Tone::Normal,
        cells: vec![Cell::Time(at), amount, Cell::Text(what.into())],
    };
    let log = Table::in_range(
        "Recent events",
        &["when", "amount", "event"],
        vec![
            row(NOW - 60, Cell::Empty, "alert"),
            row(NOW - 120, Cell::Sat(20_000), "paid"),
        ],
        "nothing happened",
    );
    let report = Report {
        title: "licash hub".into(),
        subtitle: String::new(),
        now: NOW,
        since: NOW - DAY,
        series: vec![],
        pages: vec![Page::new("Overview", vec![vec![Panel::Table(log)]])],
    };
    let out = screen_of(&report, &Ui::new(Range::Day), 60, 10);
    let end = |text: &str| {
        let line = out.lines().find(|l| l.contains(text)).unwrap();
        line.chars().count() - line.split(text).nth(1).unwrap().chars().count()
    };
    assert_eq!(end("amount"), end("20,000 sat"), "\n{out}");
}

#[test]
fn what_needs_attention_comes_first_and_nothing_long_is_cut() {
    let long =
        "The Lightning node can pay 0 sat over its channels, less than the 1,000,000 sat it keeps";
    let log = Table::in_range(
        "Recent events",
        &["when", "event", "detail"],
        vec![Row::new(
            Some(NOW - 60),
            Tone::Warn,
            vec![
                Cell::Time(NOW - 60),
                Cell::Text("alert".into()),
                Cell::Text(long.into()),
            ],
        )],
        "nothing happened",
    );
    let checks = vec![
        Check::new("Service", Tone::Good, "answers"),
        Check::new("Lightning", Tone::Muted, "no gateway ran"),
        Check::new("Alerts", Tone::Warn, long),
    ];
    let report = Report {
        title: "licash hub".into(),
        subtitle: String::new(),
        now: NOW,
        since: NOW - DAY,
        series: vec![],
        pages: vec![Page::new(
            "Overview",
            vec![vec![Panel::Health(checks)], vec![Panel::Table(log)]],
        )],
    };
    let out = screen_of(&report, &Ui::new(Range::Day), 70, 24);
    let at = |text: &str| out.lines().position(|l| l.contains(text));
    // Each state in a glyph, not by its color alone; attention first.
    assert!(out.contains("! attention"), "{out}");
    assert!(at("! Alerts") < at("✓ Service"), "{out}");
    assert!(at("✓ Service") < at("– Lightning"), "{out}");
    // The long detail, whole in the health panel and in the list.
    assert_eq!(out.matches("1,000,000").count(), 2, "{out}");
    assert_eq!(out.matches("it keeps").count(), 2, "{out}");
}

#[test]
fn a_narrow_card_shortens_its_title_and_wraps_its_detail() {
    let card = Card::new(
        "Notes outstanding",
        Value::now(
            Some(2_217_151),
            Unit::Sat,
            "45x the 49,500 sat",
            Tone::Normal,
        ),
        Better::Neither,
    );
    let report = Report {
        title: "licash hub".into(),
        subtitle: String::new(),
        now: NOW,
        since: NOW - DAY,
        series: vec![],
        pages: vec![Page::new("Overview", vec![vec![Panel::Cards(vec![card])]])],
    };
    let out = screen_of(&report, &Ui::new(Range::Day), 15, 12);
    assert!(out.contains(" Notes… "), "{out}");
    assert!(out.contains("45x the"), "{out}");
    assert!(out.contains("49,500 sat"), "{out}");
}

#[test]
fn a_card_drops_what_it_compares_with_rather_than_cut_it() {
    for width in [80, 100, 160] {
        let out = screen(&Ui::new(Range::Month), width, 44);
        assert!(out.contains("▼ 17.7%"), "{out}");
        for line in out.lines() {
            for (i, _) in line.match_indices(" vs") {
                let rest = &line[i..];
                assert!(
                    rest.starts_with(" vs prior 30d") || rest.starts_with(" vs before"),
                    "{line}"
                );
            }
        }
    }
}

#[test]
fn small_terminals_still_draw() {
    for (w, h) in [(40, 12), (80, 24), (20, 6)] {
        screen(&Ui::new(Range::Day), w, h);
    }
    let mut ui = Ui::new(Range::All);
    ui.page = 1;
    let out = screen(&ui, 120, 30);
    assert!(out.contains("Runway"), "{out}");
}

fn press(ui: &mut Ui, code: KeyCode) {
    let view = View::new(&report(), ui.range);
    ui.press(KeyEvent::new(code, KeyModifiers::NONE), &view);
}

#[test]
fn keys_page_range_and_scroll() {
    let mut ui = Ui::new(Range::Month);
    press(&mut ui, KeyCode::Right);
    assert_eq!(ui.page, 1);
    press(&mut ui, KeyCode::Right);
    assert_eq!(ui.page, 0);
    press(&mut ui, KeyCode::Char('2'));
    assert_eq!(ui.page, 1);
    press(&mut ui, KeyCode::Char('9'));
    assert_eq!(ui.page, 1);
    press(&mut ui, KeyCode::Char(']'));
    assert_eq!(ui.range, Range::Quarter);
    for _ in 0..5 {
        press(&mut ui, KeyCode::Char('['));
    }
    assert_eq!(ui.range, Range::Day);
    // The overview's lists: the breakdown, then the failures.
    press(&mut ui, KeyCode::Char('1'));
    ui.range = Range::Month;
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.focus, 1);
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.focus, 0);
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Down);
    assert_eq!(ui.offset(0), 1, "no further than the last row");
    press(&mut ui, KeyCode::Char('?'));
    assert!(ui.help);
    press(&mut ui, KeyCode::Char('q'));
    assert!(!ui.help, "a key closes the help first");
}

#[test]
fn ranges_read_and_print_alike() {
    for range in Range::ALL {
        assert_eq!(range.to_string().parse::<Range>(), Ok(range));
    }
    assert!("2w".parse::<Range>().is_err());
    assert_eq!(Range::default(), Range::Month);
}
