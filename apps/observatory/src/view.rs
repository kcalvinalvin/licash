//! What the observatory shows, computed from its records over a range of
//! time: each service's health and terms, the leaderboards, the fees of
//! the network at a glance, and the wallet's measured moves. The web page
//! and its JSON API both show this one model.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::{
    budget::{self, Budget},
    store::{Action, Outcome, Probe, Role, Service, Store},
    terms::{self, HubTerms, ProviderTerms},
};

/// The amount fees are compared on, in sat.
pub const REFERENCE: u64 = 10_000;

/// Points in a service's chart.
const BUCKETS: i64 = 48;

/// What a provider's hub is called when no hub watched has its address.
pub const UNKNOWN_HUB: &str = "";

/// The fewest probes a service needs in a range to be ranked.
const RANKED_AFTER: u32 = 3;

/// The span of time shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Range {
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Day, Range::Week, Range::Month];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.key() == s)
    }

    pub fn key(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::Day => "24 hours",
            Range::Week => "7 days",
            Range::Month => "30 days",
        }
    }

    pub fn millis(self) -> i64 {
        let day = 24 * 3600 * 1000;
        match self {
            Range::Day => day,
            Range::Week => 7 * day,
            Range::Month => 30 * day,
        }
    }
}

/// Everything shown, at one moment, over one range.
#[derive(Clone, Debug, Serialize)]
pub struct View {
    pub network: String,
    /// Whether services are reached over Tor.
    pub tor: bool,
    pub at: i64,
    pub range: Range,
    pub hubs: Vec<HubView>,
    pub providers: Vec<ProviderView>,
    /// Over every successful probe of the range.
    pub latency: Latency,
    pub boards: Boards,
    pub fees: Vec<FeeRow>,
    pub moves: Vec<KindStats>,
    /// The latest actions, newest first.
    pub recent: Vec<Action>,
    pub budget: Option<Budget>,
}

/// Medians of a set of probes, in milliseconds.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Latency {
    /// Opening a connection: Tor's circuit and stream, and TLS.
    pub open: Option<u32>,
    /// A round trip on an open connection.
    pub rtt: Option<u32>,
    /// The service's work on its info, beyond the round trip.
    pub work: Option<u32>,
}

/// How a service has been.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Health {
    /// Whether it answered its last probe, at any of its URLs; none if not
    /// probed lately.
    pub up: Option<bool>,
    pub last_probe: Option<i64>,
    /// Of its probes in the range, the share answered, 0 to 1.
    pub uptime: Option<f64>,
    pub probes: u32,
    pub latency: Latency,
    pub last_error: Option<String>,
    /// The wallet's actions through it in the range.
    pub actions: Rate,
    pub chart: Vec<Bucket>,
    /// Each URL's last probe in the range.
    pub urls: Vec<UrlState>,
}

/// A URL's last probe.
#[derive(Clone, Debug, Serialize)]
pub struct UrlState {
    pub url: String,
    pub ok: bool,
    pub at: i64,
    pub error: Option<String>,
}

/// Of some attempts, those that succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Rate {
    pub done: u32,
    pub total: u32,
}

impl Rate {
    pub fn share(self) -> Option<f64> {
        (self.total > 0).then(|| f64::from(self.done) / f64::from(self.total))
    }
}

/// A stretch of a service's chart.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Bucket {
    pub at: i64,
    pub ok: u32,
    pub total: u32,
    pub rtt: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HubView {
    pub id: String,
    pub name: Option<String>,
    pub host: String,
    pub urls: Vec<String>,
    pub announced: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    pub health: Health,
    pub terms: Option<HubTerms>,
    /// What providers can swap into it now, in all.
    pub inbound: u64,
    /// Blocks its chain is behind the furthest of the hubs', if known.
    pub behind: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderView {
    pub id: String,
    pub name: Option<String>,
    pub host: String,
    pub urls: Vec<String>,
    pub announced: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    pub health: Health,
    pub terms: Option<ProviderTerms>,
    /// The hubs it swaps between, by id where known, and what it can
    /// lock to a client at each now.
    pub hubs: Vec<(String, u64)>,
}

/// A leaderboard's line.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub id: String,
    pub role: Role,
    pub label: String,
    pub value: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Boards {
    /// Share of probes answered, highest first.
    pub uptime: Vec<Entry>,
    /// Median round trip in ms, lowest first.
    pub latency: Vec<Entry>,
    /// Share of the wallet's actions through it that succeeded.
    pub success: Vec<Entry>,
}

/// One kind of fee across the network, on [`REFERENCE`].
#[derive(Clone, Debug, Serialize)]
pub struct FeeRow {
    pub what: &'static str,
    /// The services offering it.
    pub offered: usize,
    pub cheapest: Option<Entry>,
    pub median: Option<u64>,
}

/// The wallet's actions of one kind, in the range.
#[derive(Clone, Debug, Serialize)]
pub struct KindStats {
    pub kind: String,
    pub rate: Rate,
    /// Median of those that succeeded, in ms: of a move on Bitcoin, its
    /// swap, not the waits for blocks.
    pub duration: Option<u64>,
    /// Median cost of those that succeeded, in sat.
    pub cost: Option<u64>,
    pub last: Option<i64>,
}

/// What the view is computed from: read from the store, then computed
/// without it.
pub struct Records {
    pub services: Vec<Service>,
    pub probes: Vec<Probe>,
    pub terms: Vec<(String, i64, String)>,
    pub actions: Vec<Action>,
}

impl Records {
    pub fn read(store: &Store, since: i64) -> anyhow::Result<Self> {
        Ok(Self {
            services: store.services()?,
            probes: store.probes_since(since)?,
            terms: store.latest_terms()?,
            actions: store.actions_since(since)?,
        })
    }
}

/// What the page shows, besides the records.
pub struct Context {
    pub network: String,
    pub tor: bool,
    pub now: i64,
    pub range: Range,
    /// A service is taken for up or down only if probed this recently.
    pub fresh: i64,
    pub budget: Option<Budget>,
}

pub fn compute(records: Records, cx: Context) -> View {
    let since = cx.now - cx.range.millis();
    let probes: Vec<&Probe> = records.probes.iter().filter(|p| p.at >= since).collect();
    let actions: Vec<&Action> = records
        .actions
        .iter()
        .filter(|a| a.started >= since)
        .collect();
    let mut by_service: HashMap<&str, Vec<&Probe>> = HashMap::new();
    for probe in &probes {
        by_service.entry(&probe.service).or_default().push(probe);
    }
    let terms: HashMap<&str, &str> = records
        .terms
        .iter()
        .map(|(id, _, info)| (id.as_str(), info.as_str()))
        .collect();
    let shown_since =
        cx.now - crate::observer::millis(crate::observer::KEPT).max(cx.range.millis());
    let services: Vec<&Service> = records
        .services
        .iter()
        .filter(|s| s.last_seen >= shown_since)
        .collect();

    let health = |service: &Service| {
        let probes = by_service.get(service.id.as_str()).map_or(&[][..], |p| p);
        health(service, probes, &actions, &cx)
    };
    let mut hubs: Vec<HubView> = services
        .iter()
        .filter(|s| s.role == Role::Hub)
        .map(|s| HubView {
            id: s.id.clone(),
            name: s.name.clone(),
            host: host_of(s),
            urls: s.urls.clone(),
            announced: s.key.is_some(),
            first_seen: s.first_seen,
            last_seen: s.last_seen,
            health: health(s),
            terms: terms.get(s.id.as_str()).and_then(|t| terms::hub(t)),
            inbound: 0,
            behind: None,
        })
        .collect();
    let mut providers: Vec<ProviderView> = services
        .iter()
        .filter(|s| s.role == Role::Provider)
        .map(|s| {
            let terms = terms.get(s.id.as_str()).and_then(|t| terms::provider(t));
            let at = terms.iter().flat_map(|t| &t.hubs).map(|(address, max)| {
                let hub = hubs.iter().find(|v| {
                    let known = v.terms.as_ref().and_then(|t| t.address.as_ref());
                    known == Some(address)
                });
                let id = hub.map_or_else(|| UNKNOWN_HUB.to_owned(), |v| v.id.clone());
                (id, *max)
            });
            ProviderView {
                id: s.id.clone(),
                name: s.name.clone(),
                host: host_of(s),
                urls: s.urls.clone(),
                announced: s.key.is_some(),
                first_seen: s.first_seen,
                last_seen: s.last_seen,
                health: health(s),
                hubs: at.collect(),
                terms,
            }
        })
        .collect();
    let tip = hubs.iter().filter_map(|h| h.terms.as_ref()?.height).max();
    for hub in &mut hubs {
        let height = hub.terms.as_ref().and_then(|t| t.height);
        hub.behind = tip.zip(height).map(|(tip, h)| tip.saturating_sub(h));
        hub.inbound = providers
            .iter()
            .filter(|p| p.health.up == Some(true))
            .flat_map(|p| &p.hubs)
            .filter(|(id, _)| *id == hub.id)
            .map(|(_, max)| max)
            .sum();
    }
    // Up first, then by name.
    hubs.sort_by(|a, b| order(&a.health, &a.host, &b.health, &b.host));
    providers.sort_by(|a, b| order(&a.health, &a.host, &b.health, &b.host));

    let ok: Vec<&Probe> = probes.iter().copied().filter(|p| p.ok).collect();
    let boards = boards(&hubs, &providers);
    let fees = fees(&hubs, &providers);
    let moves = moves(&actions);
    let mut recent: Vec<Action> = records.actions.iter().rev().take(20).cloned().collect();
    recent.sort_by_key(|a| std::cmp::Reverse(a.started));
    View {
        network: cx.network,
        tor: cx.tor,
        at: cx.now,
        range: cx.range,
        latency: latency(&ok),
        hubs,
        providers,
        boards,
        fees,
        moves,
        recent,
        budget: cx.budget,
    }
}

fn order(a: &Health, a_host: &str, b: &Health, b_host: &str) -> std::cmp::Ordering {
    let rank = |h: &Health| match h.up {
        Some(true) => 0,
        Some(false) => 1,
        None => 2,
    };
    rank(a).cmp(&rank(b)).then_with(|| a_host.cmp(b_host))
}

fn health(service: &Service, probes: &[&Probe], actions: &[&Action], cx: &Context) -> Health {
    let mut last: BTreeMap<&str, &Probe> = BTreeMap::new();
    for probe in probes {
        last.insert(&probe.url, probe);
    }
    let last_probe = probes.iter().map(|p| p.at).max();
    let fresh = last_probe.is_some_and(|at| at >= cx.now - cx.fresh);
    let up = fresh.then(|| last.values().any(|p| p.ok));
    let ok: Vec<&Probe> = probes.iter().copied().filter(|p| p.ok).collect();
    let total = u32::try_from(probes.len()).unwrap_or(u32::MAX);
    let answered = u32::try_from(ok.len()).unwrap_or(u32::MAX);
    let last_error = probes
        .iter()
        .rev()
        .find(|p| !p.ok)
        .and_then(|p| p.error.clone());
    let mut rate = Rate::default();
    for action in actions {
        let through = [&action.source, &action.target, &action.via]
            .into_iter()
            .any(|s| s.as_deref() == Some(service.id.as_str()));
        if through && action.outcome != Outcome::Running {
            rate.total += 1;
            rate.done += u32::from(action.outcome == Outcome::Done);
        }
    }
    Health {
        up,
        last_probe,
        uptime: (total > 0).then(|| f64::from(answered) / f64::from(total)),
        probes: total,
        latency: latency(&ok),
        last_error,
        actions: rate,
        chart: chart(probes, cx.now, cx.range),
        urls: last
            .values()
            .map(|p| UrlState {
                url: p.url.clone(),
                ok: p.ok,
                at: p.at,
                error: p.error.clone(),
            })
            .collect(),
    }
}

/// The probes of a service as buckets over the range, or over what there
/// is of it: from its first probe, if later, but at least a forty-eighth of the
/// range, so that a new service's chart is not all empty cells.
fn chart(probes: &[&Probe], now: i64, range: Range) -> Vec<Bucket> {
    let first = probes.iter().map(|p| p.at).min().unwrap_or(now);
    let span = (now - first).clamp(range.millis() / 48, range.millis());
    let width = (span / BUCKETS).max(1);
    let start = now - width * BUCKETS;
    let mut buckets: Vec<(u32, u32, Vec<u32>)> = vec![(0, 0, Vec::new()); BUCKETS as usize];
    for probe in probes {
        let i = ((probe.at - start) / width).clamp(0, BUCKETS - 1) as usize;
        buckets[i].1 += 1;
        if probe.ok {
            buckets[i].0 += 1;
            buckets[i].2.extend(probe.rtt_ms);
        }
    }
    buckets
        .into_iter()
        .enumerate()
        .map(|(i, (ok, total, mut rtt))| Bucket {
            at: start + i as i64 * width,
            ok,
            total,
            rtt: median(&mut rtt),
        })
        .collect()
}

fn latency(ok: &[&Probe]) -> Latency {
    let mut open: Vec<u32> = ok.iter().filter_map(|p| p.open_ms).collect();
    let mut rtt: Vec<u32> = ok.iter().filter_map(|p| p.rtt_ms).collect();
    let mut work: Vec<u32> = ok
        .iter()
        .filter_map(|p| Some(p.info_ms?.saturating_sub(p.rtt_ms?)))
        .collect();
    Latency {
        open: median(&mut open),
        rtt: median(&mut rtt),
        work: median(&mut work),
    }
}

pub fn median<T: Ord + Copy>(values: &mut [T]) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// A service's name to show: its announced name, else its host.
pub fn host_of(service: &Service) -> String {
    service
        .urls
        .first()
        .map_or_else(|| service.id.clone(), |u| host(u))
}

/// The host of `url`.
pub fn host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest).to_owned()
}

fn label(name: &Option<String>, host: &str) -> String {
    name.clone().unwrap_or_else(|| host.to_owned())
}

fn boards(hubs: &[HubView], providers: &[ProviderView]) -> Boards {
    let all = hubs
        .iter()
        .map(|h| (&h.id, Role::Hub, label(&h.name, &h.host), &h.health))
        .chain(
            providers
                .iter()
                .map(|p| (&p.id, Role::Provider, label(&p.name, &p.host), &p.health)),
        );
    let mut boards = Boards::default();
    for (id, role, label, health) in all {
        let entry = |value| Entry {
            id: id.clone(),
            role,
            label: label.clone(),
            value,
        };
        if health.probes >= RANKED_AFTER {
            if let Some(uptime) = health.uptime {
                boards.uptime.push(entry(uptime));
            }
            if let Some(rtt) = health.latency.rtt {
                boards.latency.push(entry(f64::from(rtt)));
            }
        }
        if let Some(share) = health.actions.share() {
            boards.success.push(entry(share));
        }
    }
    let desc = |a: &Entry, b: &Entry| b.value.total_cmp(&a.value).then(a.label.cmp(&b.label));
    boards.uptime.sort_by(desc);
    boards.success.sort_by(desc);
    boards
        .latency
        .sort_by(|a, b| a.value.total_cmp(&b.value).then(a.label.cmp(&b.label)));
    boards
}

fn fees(hubs: &[HubView], providers: &[ProviderView]) -> Vec<FeeRow> {
    let hub_fee = |what, fee: &dyn Fn(&HubTerms) -> Option<u64>| {
        row(
            what,
            hubs.iter().filter_map(|h| {
                let fee = fee(h.terms.as_ref()?)?;
                Some(Entry {
                    id: h.id.clone(),
                    role: Role::Hub,
                    label: label(&h.name, &h.host),
                    value: fee as f64,
                })
            }),
        )
    };
    let provider_fee = |what, fee: &dyn Fn(&ProviderTerms) -> Option<u64>| {
        row(
            what,
            providers.iter().filter_map(|p| {
                let fee = fee(p.terms.as_ref()?)?;
                Some(Entry {
                    id: p.id.clone(),
                    role: Role::Provider,
                    label: label(&p.name, &p.host),
                    value: fee as f64,
                })
            }),
        )
    };
    vec![
        hub_fee("Pay in Licash", &|i| Some(i.fees?.spend.due(REFERENCE))),
        provider_fee("Move to another hub", &|i| Some(i.fee?.due(REFERENCE))),
        hub_fee("Pay over Lightning", &|i| {
            Some(i.lightning.as_ref()?.pay.due(REFERENCE))
        }),
        hub_fee("Receive over Lightning", &|i| {
            Some(i.lightning.as_ref()?.receive.due(REFERENCE))
        }),
        provider_fee("Pay over Lightning, through a provider", &|i| {
            Some(i.lightning.as_ref()?.fee.due(REFERENCE))
        }),
        hub_fee("Receive on Bitcoin", &|i| {
            Some(i.deposits.as_ref()?.fee.due(REFERENCE))
        }),
        provider_fee("Pay on Bitcoin", &|i| {
            let chain = i.chain.as_ref()?;
            Some(chain.fee.due(REFERENCE).saturating_add(chain.miner))
        }),
        hub_fee("Protection, opening", &|i| {
            let protection = i.protection.as_ref().filter(|p| !p.mock)?;
            protection.opening.or(protection.price)
        }),
    ]
}

fn row(what: &'static str, entries: impl Iterator<Item = Entry>) -> FeeRow {
    let mut entries: Vec<Entry> = entries.collect();
    entries.sort_by(|a, b| a.value.total_cmp(&b.value).then(a.label.cmp(&b.label)));
    let mut values: Vec<u64> = entries.iter().map(|e| e.value as u64).collect();
    FeeRow {
        what,
        offered: entries.len(),
        median: median(&mut values),
        cheapest: entries.into_iter().next(),
    }
}

fn moves(actions: &[&Action]) -> Vec<KindStats> {
    let mut kinds: BTreeMap<&str, Vec<&Action>> = BTreeMap::new();
    for action in actions {
        kinds.entry(&action.kind).or_default().push(action);
    }
    kinds
        .into_iter()
        .map(|(kind, actions)| {
            let ended: Vec<&&Action> = actions
                .iter()
                .filter(|a| a.outcome != Outcome::Running)
                .collect();
            let done: Vec<&&Action> = ended
                .iter()
                .copied()
                .filter(|a| a.outcome == Outcome::Done)
                .collect();
            // The swap's time, not the waits for blocks of a move on Bitcoin.
            let mut durations: Vec<u64> = done.iter().filter_map(|a| budget::took(a)).collect();
            let mut costs: Vec<u64> = done.iter().filter_map(|a| a.cost).collect();
            KindStats {
                kind: kind.to_owned(),
                rate: Rate {
                    done: u32::try_from(done.len()).unwrap_or(u32::MAX),
                    total: u32::try_from(ended.len()).unwrap_or(u32::MAX),
                },
                duration: median(&mut durations),
                cost: median(&mut costs),
                last: actions.iter().map(|a| a.started).max(),
            }
        })
        .collect()
}
