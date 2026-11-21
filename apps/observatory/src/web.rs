//! The observatory's web page and its JSON API, read-only: anyone may look,
//! nobody can make the wallet do anything from here.
//!
//! Pages are written as HTML here, with their style inline: no script, no
//! build step, a page whole in one answer.

// Handlers answer an error page as their `Err`: served at once, its size
// does not matter.
#![allow(clippy::result_large_err)]

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::{
    budget::{self, Budget, Kind},
    format::{self, esc},
    store::{Action, Outcome, Role, Store},
    view::{self, Entry, Health, HubView, Latency, ProviderView, Range, Records, View},
    viz::{self, Part},
};

/// How long a computed view is shown before it is computed again.
const CACHED: Duration = Duration::from_secs(20);

/// What the pages read.
pub struct App {
    pub store: Arc<Store>,
    pub network: String,
    pub tor: bool,
    /// A service probed less recently is shown as not probed lately, in ms.
    pub fresh: i64,
    /// The budget, as the wallet last told it; none without a wallet.
    pub budget: Arc<Mutex<Option<Budget>>>,
    cache: Mutex<HashMap<&'static str, (Instant, Arc<View>)>>,
}

impl App {
    pub fn new(
        store: Arc<Store>,
        network: String,
        tor: bool,
        fresh: Duration,
        budget: Arc<Mutex<Option<Budget>>>,
    ) -> Self {
        Self {
            store,
            network,
            tor,
            fresh: crate::observer::millis(fresh),
            budget,
            cache: Mutex::default(),
        }
    }

    async fn view(self: &Arc<Self>, range: Range) -> Result<Arc<View>, String> {
        let cached = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.get(range.key()).cloned()
        };
        if let Some((at, view)) = cached {
            if at.elapsed() < CACHED {
                return Ok(view);
            }
        }
        let app = self.clone();
        let view = tokio::task::spawn_blocking(move || {
            let now = crate::store::now();
            let records = Records::read(&app.store, now - range.millis())?;
            let budget = app.budget.lock().unwrap_or_else(|e| e.into_inner()).clone();
            anyhow::Ok(view::compute(
                records,
                view::Context {
                    network: app.network.clone(),
                    tor: app.tor,
                    now,
                    range,
                    fresh: app.fresh,
                    budget,
                },
            ))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
        let view = Arc::new(view);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(range.key(), (Instant::now(), view.clone()));
        Ok(view)
    }
}

/// The networks served, each under its own name: `/signet/hubs`. The
/// first is the one `/` leads to.
pub struct Site {
    apps: Vec<Arc<App>>,
    names: Vec<String>,
}

impl Site {
    fn app(&self, network: &str) -> Result<&Arc<App>, Response> {
        let found = self.apps.iter().find(|a| a.network == network);
        found.ok_or_else(|| (StatusCode::NOT_FOUND, "No such network here.").into_response())
    }
}

/// The pages of every network in `apps`, and `/health`.
pub fn router(apps: Vec<Arc<App>>) -> Router {
    let names = apps.iter().map(|a| a.network.clone()).collect();
    let site = Arc::new(Site { apps, names });
    let mut router = Router::new()
        .route("/", get(front))
        .route("/health", get(|| async { "ok" }));
    for (path, handler) in [
        ("", get(overview)),
        ("/hubs", get(hubs)),
        ("/providers", get(providers)),
        ("/service", get(service)),
        ("/moves", get(moves)),
        ("/api/v1/view", get(api)),
    ] {
        router = router.route(&format!("/{{network}}{path}"), handler.clone());
        if path.is_empty() {
            router = router.route("/{network}/", handler);
        }
    }
    router.with_state(site)
}

async fn front(State(site): State<Arc<Site>>) -> Response {
    match site.names.first() {
        Some(first) => Redirect::temporary(&format!("/{first}")).into_response(),
        None => (StatusCode::NOT_FOUND, "No network is watched.").into_response(),
    }
}

#[derive(Deserialize)]
struct Asked {
    range: Option<String>,
    id: Option<String>,
}

impl Asked {
    fn range(&self) -> Range {
        self.range
            .as_deref()
            .and_then(Range::parse)
            .unwrap_or(Range::Week)
    }
}

/// A page, or the page that says why not.
type Page = Result<Response, Response>;

async fn read(site: &Site, network: &str, asked: &Asked) -> Result<Arc<View>, Response> {
    let app = site.app(network)?;
    app.view(asked.range()).await.map_err(|e| {
        tracing::warn!("computing the view: {e}");
        let body = shell(
            &site.names,
            &app.network,
            "Couldn't read the records",
            "",
            asked.range(),
            &failed(&e),
        );
        (StatusCode::INTERNAL_SERVER_ERROR, Html(body)).into_response()
    })
}

async fn api(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = site.app(&network)?.view(asked.range()).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response()
    })?;
    Ok(Json(view.as_ref().clone()).into_response())
}

async fn overview(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = read(&site, &network, &asked).await?;
    Ok(html(&site, &view, "Overview", "/", &overview_body(&view)))
}

async fn hubs(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = read(&site, &network, &asked).await?;
    Ok(html(&site, &view, "Hubs", "/hubs", &hubs_body(&view)))
}

async fn providers(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = read(&site, &network, &asked).await?;
    Ok(html(
        &site,
        &view,
        "Swap providers",
        "/providers",
        &providers_body(&view),
    ))
}

async fn moves(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = read(&site, &network, &asked).await?;
    Ok(html(
        &site,
        &view,
        "Measured moves",
        "/moves",
        &moves_body(&view),
    ))
}

async fn service(
    State(site): State<Arc<Site>>,
    Path(network): Path<String>,
    Query(asked): Query<Asked>,
) -> Page {
    let view = read(&site, &network, &asked).await?;
    let id = asked.id.as_deref().unwrap_or_default();
    let body = if let Some(hub) = view.hubs.iter().find(|h| h.id == id) {
        Some((title(&hub.name, &hub.host), hub_body(&view, hub)))
    } else {
        view.providers
            .iter()
            .find(|p| p.id == id)
            .map(|p| (title(&p.name, &p.host), provider_body(&view, p)))
    };
    match body {
        Some((title, body)) => Ok(html(&site, &view, &title, "", &body)),
        None => {
            let body = shell(
                &site.names,
                &view.network,
                "Not found",
                "",
                view.range,
                &failed("No hub or provider seen lately has this id."),
            );
            Err((StatusCode::NOT_FOUND, Html(body)).into_response())
        }
    }
}

fn html(site: &Site, view: &View, title: &str, active: &str, body: &str) -> Response {
    let page = shell(&site.names, &view.network, title, active, view.range, body);
    ([(header::CACHE_CONTROL, "no-cache")], Html(page)).into_response()
}

fn title(name: &Option<String>, host: &str) -> String {
    name.clone().unwrap_or_else(|| host.to_owned())
}

fn failed(why: &str) -> String {
    format!(
        "<section class=\"calm\"><h2>Couldn't show this</h2><p>{}</p>\
         <p class=\"dim\">Reload the page in a moment.</p></section>",
        esc(why)
    )
}

/// The page around `body`: its header, the networks to switch to,
/// navigation and range. `active` is the page's path within its network.
fn shell(
    networks: &[String],
    network: &str,
    title: &str,
    active: &str,
    range: Range,
    body: &str,
) -> String {
    let base = format!("/{network}");
    let mut nav = String::new();
    for (href, label) in [
        ("/", "Overview"),
        ("/hubs", "Hubs"),
        ("/providers", "Providers"),
        ("/moves", "Moves"),
    ] {
        let current = if href == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(
            nav,
            "<a href=\"{base}{href}?range={}\"{current}>{label}</a>",
            range.key()
        );
    }
    let mut ranges = String::new();
    let here = if active.is_empty() { "/" } else { active };
    for r in Range::ALL {
        let current = if r == range {
            " aria-current=\"true\""
        } else {
            ""
        };
        let _ = write!(
            ranges,
            "<a href=\"{base}{here}?range={}\"{current}>{}</a>",
            r.key(),
            r.label()
        );
    }
    let mut badges = String::new();
    for other in networks.iter().filter(|_| networks.len() > 1) {
        let current = if other == network {
            " aria-current=\"true\""
        } else {
            ""
        };
        let _ = write!(
            badges,
            "<a class=\"badge\" href=\"/{}{}?range={}\"{current}>{}</a>",
            esc(other),
            if active == "/" || active.is_empty() {
                ""
            } else {
                active
            },
            range.key(),
            esc(&network_name(other))
        );
    }
    if networks.len() <= 1 {
        let _ = write!(
            badges,
            "<span class=\"badge\">{}</span>",
            esc(&network_name(network))
        );
    }
    let test = if network == "bitcoin" {
        String::new()
    } else {
        "<span class=\"badge warn\" title=\"Test coins: worth nothing\">Test network</span>"
            .to_owned()
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"refresh\" content=\"120\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <meta name=\"description\" content=\"Uptime, latency, fees and measured moves of Licash hubs and swap providers\">\
         <link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Ccircle cx='16' cy='16' r='13' fill='none' stroke='%231f8a4c' stroke-width='4'/%3E%3Ccircle cx='16' cy='16' r='5' fill='%231f8a4c'/%3E%3C/svg%3E\">\
         <title>{title} · Licash observatory</title><style>{STYLE}</style></head><body>\
         <header><div class=\"bar\"><a class=\"brand\" href=\"/\">licash <span>observatory</span></a>\
         <span class=\"badges\">{badges}</span>{test}</div>\
         <nav>{nav}</nav></header>\
         <main><div class=\"heading\"><h1>{title}</h1><div class=\"ranges\">{ranges}</div></div>\
         {body}</main>\
         <footer>A self-hosted Licash observatory. <a href=\"{base}/api/v1/view?range={key}\">JSON</a></footer>\
         </body></html>",
        title = esc(title),
        key = range.key(),
    )
}

fn network_name(network: &str) -> String {
    match network {
        "bitcoin" => "Bitcoin".into(),
        "signet" => "Signet".into(),
        "mutinynet" => "Mutinynet".into(),
        "regtest" => "Regtest".into(),
        other => other.into(),
    }
}

/// A state in a glyph and a word.
fn state(health: &Health) -> &'static str {
    match health.up {
        Some(true) => "<span class=\"state ok\">✓ Up</span>",
        Some(false) => "<span class=\"state bad\">✗ Down</span>",
        None => "<span class=\"state idle\">– Not probed lately</span>",
    }
}

fn link(view: &View, id: &str, label: &str) -> String {
    format!(
        "<a href=\"/{}/service?id={}&amp;range={}\">{}</a>",
        view.network,
        format::query(id),
        view.range.key(),
        esc(label)
    )
}

fn opt(value: Option<String>) -> String {
    value.unwrap_or_else(|| "<span class=\"dim\">–</span>".into())
}

fn ms(value: Option<u32>) -> String {
    opt(value.map(|v| format::millis(u64::from(v))))
}

fn card(label: &str, value: &str, note: &str) -> String {
    format!(
        "<div class=\"card\"><div class=\"label\">{}</div><div class=\"value\">{value}</div>\
         <div class=\"note\">{note}</div></div>",
        esc(label)
    )
}

fn up_count<'a>(healths: impl Iterator<Item = &'a Health>) -> (usize, usize) {
    let healths: Vec<&Health> = healths.collect();
    let up = healths.iter().filter(|h| h.up == Some(true)).count();
    (up, healths.len())
}

fn overview_body(view: &View) -> String {
    let mut out = String::new();
    let (hubs_up, hubs) = up_count(view.hubs.iter().map(|h| &h.health));
    let (providers_up, providers) = up_count(view.providers.iter().map(|p| &p.health));
    let moved: (u32, u32) = view
        .moves
        .iter()
        .fold((0, 0), |(d, t), k| (d + k.rate.done, t + k.rate.total));
    out.push_str(&banner(view, hubs_up + providers_up, hubs + providers));
    out.push_str("<section class=\"cards\">");
    let states = |healths: Vec<&Health>| healths.iter().map(|h| h.up).collect::<Vec<_>>();
    out.push_str(&card(
        "Hubs up",
        &format!("{hubs_up} of {hubs}"),
        &format!(
            "{} answering now",
            viz::dots(&states(view.hubs.iter().map(|h| &h.health).collect()))
        ),
    ));
    out.push_str(&card(
        "Providers up",
        &format!("{providers_up} of {providers}"),
        &format!(
            "{} answering now",
            viz::dots(&states(view.providers.iter().map(|p| &p.health).collect()))
        ),
    ));
    let opening = if view.tor {
        format!("opening a connection over Tor: {}", ms(view.latency.open))
    } else {
        format!("opening a connection: {}", ms(view.latency.open))
    };
    out.push_str(&card("Round trip", &ms(view.latency.rtt), &opening));
    let rate = (moved.1 > 0).then(|| f64::from(moved.0) / f64::from(moved.1));
    out.push_str(&card(
        "Moves succeeded",
        &opt(rate.map(format::percent)),
        &format!("{} of {}", moved.0, moved.1),
    ));
    if let Some(budget) = &view.budget {
        out.push_str(&card(
            "Spent",
            &format::sat(budget.spent),
            &format!("of {} in 30 days", format::sat(budget.monthly)),
        ));
        out.push_str(&card(
            "Balance",
            &opt(budget.balance.map(format::sat)),
            "the observatory's, in Licash",
        ));
    }
    out.push_str("</section>");

    out.push_str(&time_split(view));

    out.push_str("<section><h2>Leaderboards</h2><div class=\"boards\">");
    out.push_str(&board(
        view,
        "Uptime",
        &view.boards.uptime,
        format::uptime,
        |e| viz::bar(share_class(e.value), e.value, &format::uptime(e.value)),
    ));
    let slowest = view
        .boards
        .latency
        .iter()
        .map(|e| e.value)
        .fold(1.0_f64, f64::max);
    out.push_str(&board(
        view,
        "Round trip",
        &view.boards.latency,
        |v| format::millis(v as u64),
        |e| {
            let setup = health_of(view, &e.id).and_then(|h| h.latency.open);
            let mut parts = vec![Part {
                class: "ok",
                share: e.value / slowest,
                title: format!("Round trip {}", format::millis(e.value as u64)),
            }];
            // Opening the connection, mostly Tor's, in its own thin bar.
            let setup = setup.map(|open| {
                viz::bar(
                    "tor",
                    f64::from(open) / (slowest * 6.0).max(1.0),
                    &format!("Opening the connection {}", format::millis(u64::from(open))),
                )
            });
            parts.retain(|p| p.share > 0.0);
            format!("{}{}", viz::meter(&parts), setup.unwrap_or_default())
        },
    ));
    out.push_str(&board(
        view,
        "Moves succeeded",
        &view.boards.success,
        format::percent,
        |e| viz::bar(share_class(e.value), e.value, &format::percent(e.value)),
    ));
    out.push_str("</div>");
    if !view.boards.latency.is_empty() {
        out.push_str(
            "<ul class=\"legend\"><li><i class=\"key ok\"></i> Round trip on an open connection</li>\
             <li><i class=\"key tor\"></i> Opening the connection (Tor, stream, TLS), at a sixth \
             of the scale</li></ul>",
        );
    }
    out.push_str("</section>");

    let _ = write!(
        out,
        "<section><h2>Fees on {}</h2><table><thead><tr><th>What</th><th>Cheapest</th>\
         <th class=\"num\">Its fee</th><th class=\"num\">Median</th><th class=\"num\">Offered by</th>\
         </tr></thead><tbody>",
        format::sat(view::REFERENCE)
    );
    for row in &view.fees {
        let (cheapest, fee) = match &row.cheapest {
            Some(e) => (
                link(view, &e.id, &e.label),
                format::fee_of(e.value as u64, fee_base(row.what)),
            ),
            None => ("<span class=\"dim\">Nobody</span>".into(), String::new()),
        };
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{cheapest}</td><td class=\"num\">{fee}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            esc(row.what),
            opt(row.median.map(|m| format::fee_of(m, fee_base(row.what)))),
            row.offered
        );
    }
    out.push_str("</tbody></table></section>");

    if !view.moves.is_empty() {
        out.push_str("<section><h2>Moves</h2>");
        out.push_str(&kinds_table(view));
        out.push_str("</section>");
    }
    out.push_str(&how(view));
    out
}

/// What a fee row's share is of: an opening fee is not of the reference.
fn fee_base(what: &str) -> u64 {
    if what.starts_with("Protection") {
        0
    } else {
        view::REFERENCE
    }
}

/// The class of a bar of a share that should be nearly all: green, amber
/// below 99%, red below 90%.
fn share_class(share: f64) -> &'static str {
    if share >= 0.99 {
        "ok"
    } else if share >= 0.9 {
        "some"
    } else {
        "bad"
    }
}

fn health_of<'a>(view: &'a View, id: &str) -> Option<&'a Health> {
    let hub = view.hubs.iter().find(|h| h.id == id).map(|h| &h.health);
    hub.or_else(|| {
        let provider = view.providers.iter().find(|p| p.id == id);
        provider.map(|p| &p.health)
    })
}

/// How every service is, in a line: green if all answer, amber if some,
/// red if none.
fn banner(view: &View, up: usize, all: usize) -> String {
    let (class, said) = match (up, all) {
        (_, 0) => ("idle", "No hub or provider found yet".to_owned()),
        (0, _) => ("bad", "Nothing answers now".to_owned()),
        (up, all) if up == all && all == 1 => ("ok", "The service answers".to_owned()),
        (up, all) if up == all => ("ok", format!("All {all} services answer")),
        (up, all) => ("some", format!("{up} of {all} services answer")),
    };
    let moves = view
        .moves
        .iter()
        .fold((0, 0), |(d, t), k| (d + k.rate.done, t + k.rate.total));
    let moved = match moves {
        (_, 0) => String::new(),
        (done, total) => format!(
            " · {done} of {total} measured moves went through in {}",
            view.range.label()
        ),
    };
    format!(
        "<section class=\"banner {class}\"><span class=\"pulse\"></span><span>{}{moved}</span></section>",
        esc(&said)
    )
}

/// Where a probe's time goes, over every probe: Tor's setup apart from the
/// round trip, and from the services' own work.
fn time_split(view: &View) -> String {
    match latency_bar(view.latency, view.tor) {
        Some(bar) => format!("<section><h2>Where the time goes</h2>{bar}</section>"),
        None => String::new(),
    }
}

/// A bar of `latency` cut in the connection's opening, the round trip and
/// the service's work, with what each took.
fn latency_bar(latency: Latency, tor: bool) -> Option<String> {
    let Latency { open, rtt, work } = latency;
    let (open, rtt) = (open?, rtt?);
    let work = work.unwrap_or(0);
    let total = f64::from(open + rtt + work).max(1.0);
    let part = |class, value: u32, what: &str| Part {
        class,
        share: f64::from(value) / total,
        title: format!("{what} {}", format::millis(u64::from(value))),
    };
    let setup = if tor {
        "Opening the connection (Tor, stream, TLS)"
    } else {
        "Opening the connection"
    };
    Some(format!(
        "<div class=\"split-bar\">{}\
         <ul class=\"legend\"><li><i class=\"key tor\"></i> {setup} <b>{}</b></li>\
         <li><i class=\"key ok\"></i> Round trip <b>{}</b></li>\
         <li><i class=\"key work\"></i> The service's own work <b>{}</b></li></ul></div>",
        viz::meter(&[
            part("tor", open, setup),
            part("ok", rtt, "Round trip"),
            part("work", work, "The service's own work"),
        ]),
        format::millis(u64::from(open)),
        format::millis(u64::from(rtt)),
        format::millis(u64::from(work)),
    ))
}

fn board(
    view: &View,
    title: &str,
    entries: &[Entry],
    show: impl Fn(f64) -> String,
    meter: impl Fn(&Entry) -> String,
) -> String {
    let mut out = format!("<div class=\"board\"><h3>{}</h3>", esc(title));
    if entries.is_empty() {
        out.push_str("<p class=\"dim\">Not enough measured yet.</p></div>");
        return out;
    }
    out.push_str("<ol>");
    for entry in entries.iter().take(10) {
        let role = match entry.role {
            Role::Hub => "hub",
            Role::Provider => "provider",
        };
        let _ = write!(
            out,
            "<li><div class=\"line\"><span class=\"who\">{} <span class=\"tag\">{role}</span></span>\
             <span class=\"num\">{}</span></div>{}</li>",
            link(view, &entry.id, &entry.label),
            show(entry.value),
            meter(entry)
        );
    }
    out.push_str("</ol></div>");
    out
}

fn how(view: &View) -> String {
    let route = if view.tor {
        "Every hub and provider is reached over Tor, each probe on a new circuit, so that \
         none is favored by where the observatory runs. <b>Round trip</b> is the median time \
         of a request on a connection already open: it still crosses Tor, but every service \
         is measured over the same many random circuits. Opening that connection (Tor's \
         circuit, the stream and TLS) is mostly Tor's time, and is shown apart."
    } else {
        "Services are reached directly, not over Tor."
    };
    format!(
        "<section class=\"calm\"><details><summary>How this is measured</summary>\
         <p>{route}</p><p>Each service is probed every few minutes at a random moment: its \
         health, then its terms. <b>Uptime</b> is the share of probes answered. A service \
         is ranked once probed 3 times in the range.</p><p>The observatory also moves a small \
         balance of its own between hubs, at random times, as often as its monthly budget \
         allows: cheap moves often, payouts on Bitcoin seldom. <b>Moves succeeded</b> counts \
         those that went through each hub or provider. A move on Bitcoin is timed by its \
         swap, from asking the provider to its coin being on the network, not by the \
         blocks waited for after it; what it costs is counted in all.</p></details></section>"
    )
}

fn hubs_body(view: &View) -> String {
    if view.hubs.is_empty() {
        return "<section class=\"calm\"><p>No hub found yet. Hubs announce themselves on \
                Nostr every 30 minutes.</p></section>"
            .into();
    }
    let mut out = String::from(
        "<section><table><thead><tr><th>Hub</th><th>State</th><th>Probes</th>\
         <th class=\"num\">Uptime</th><th class=\"num\">Round trip</th><th class=\"num\">Pay in Licash</th>\
         <th class=\"num\">Lightning out</th><th class=\"num\">Lightning in</th>\
         <th class=\"num\">Bitcoin in</th><th class=\"num\">Protection</th>\
         <th class=\"num\">Into it</th></tr></thead><tbody>",
    );
    let deepest = view
        .hubs
        .iter()
        .map(|h| h.inbound)
        .max()
        .unwrap_or(0)
        .max(1);
    for hub in &view.hubs {
        let terms = hub.terms.as_ref();
        let lightning = terms.and_then(|t| t.lightning.as_ref());
        let protection = terms
            .and_then(|t| t.protection.as_ref())
            .map(protection_line);
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            link(view, &hub.id, &title(&hub.name, &hub.host)),
            state(&hub.health),
            viz::strip(&hub.health.chart, &format!("Probes over {}", view.range.label())),
            opt(hub.health.uptime.map(format::uptime)),
            ms(hub.health.latency.rtt),
            opt(terms.and_then(|t| t.fees).map(|f| format::terms(&f.spend))),
            opt(lightning.map(|l| format::terms(&l.pay))),
            opt(lightning.map(|l| format::terms(&l.receive))),
            opt(terms
                .and_then(|t| t.deposits.as_ref())
                .map(|d| format::terms(&d.fee))),
            opt(protection),
            if hub.inbound > 0 {
                format!(
                    "up to {}{}",
                    format::sat(hub.inbound),
                    viz::bar(
                        "ok",
                        hub.inbound as f64 / deepest as f64,
                        "What providers can move into it, against the deepest hub",
                    )
                )
            } else {
                "<span class=\"dim\">–</span>".into()
            },
        );
    }
    out.push_str("</tbody></table></section>");
    out.push_str(
        "<p class=\"dim\">Fees are each hub's terms. <b>Into it</b> is what swap providers \
         can move into the hub now, in all.</p>",
    );
    out
}

fn providers_body(view: &View) -> String {
    if view.providers.is_empty() {
        return "<section class=\"calm\"><p>No swap provider found yet.</p></section>".into();
    }
    let mut out = String::from(
        "<section><table><thead><tr><th>Provider</th><th>State</th><th>Probes</th>\
         <th class=\"num\">Uptime</th><th class=\"num\">Round trip</th><th class=\"num\">Move fee</th>\
         <th class=\"num\">Moves</th><th class=\"num\">Hubs</th>\
         <th class=\"num\">Bitcoin out</th><th class=\"num\">Lightning out</th></tr></thead><tbody>",
    );
    for provider in &view.providers {
        let terms = provider.terms.as_ref();
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            link(view, &provider.id, &title(&provider.name, &provider.host)),
            state(&provider.health),
            viz::strip(&provider.health.chart, &format!("Probes over {}", view.range.label())),
            opt(provider.health.uptime.map(format::uptime)),
            ms(provider.health.latency.rtt),
            opt(terms.and_then(|t| t.fee).map(|f| format::terms(&f))),
            opt(terms.and_then(|t| Some(span(t.min?, t.max?)))),
            provider.hubs.len(),
            opt(terms.and_then(|t| t.chain.as_ref()).map(|c| {
                format!(
                    "{} + {} miners",
                    format::terms(&c.fee),
                    format::sat(c.miner)
                )
            })),
            opt(terms
                .and_then(|t| t.lightning.as_ref())
                .map(|l| format::terms(&l.fee))),
        );
    }
    out.push_str("</tbody></table></section>");
    out
}

fn facts(rows: &[(&str, String)]) -> String {
    let mut out = String::from("<dl class=\"facts\">");
    for (label, value) in rows {
        let _ = write!(out, "<dt>{}</dt><dd>{value}</dd>", esc(label));
    }
    out.push_str("</dl>");
    out
}

fn health_facts(view: &View, health: &Health) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("State", state(health).to_owned()),
        (
            "Uptime",
            opt(health
                .uptime
                .map(|u| format!("{} of {} probes", format::uptime(u), health.probes))),
        ),
        ("Round trip", ms(health.latency.rtt)),
        ("Opening a connection", ms(health.latency.open)),
        ("Answering its terms", ms(health.latency.work)),
        (
            "Moves succeeded",
            opt(health.actions.share().map(|s| {
                format!(
                    "{} ({} of {})",
                    format::percent(s),
                    health.actions.done,
                    health.actions.total
                )
            })),
        ),
        (
            "Last probed",
            opt(health.last_probe.map(|at| format::ago(at, view.at))),
        ),
    ];
    if let Some(error) = &health.last_error {
        rows.push(("Last failure", esc(error)));
    }
    rows
}

/// Where one service's probes spend their time.
fn service_time(view: &View, health: &Health) -> String {
    match latency_bar(health.latency, view.tor) {
        Some(bar) => format!("<section><h2>Where its time goes</h2>{bar}</section>"),
        None => String::new(),
    }
}

fn chart(view: &View, health: &Health) -> String {
    const W: f64 = 720.0;
    const H: f64 = 120.0;
    let n = health.chart.len().max(1) as f64;
    let step = W / n;
    // What the chart spans: the range, or less of it for a service seen
    // more lately.
    let span = match health.chart.first() {
        Some(first) if view.at - first.at < view.range.millis() * 19 / 20 => {
            let seen = u64::try_from(view.at - first.at).unwrap_or(0);
            format!("over the last {}", format::millis(seen))
        }
        _ => format!("over {}", view.range.label()),
    };
    let max = health
        .chart
        .iter()
        .filter_map(|b| b.rtt)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let mut out = format!(
        "<figure class=\"chart\"><svg viewBox=\"0 0 {W} {}\" role=\"img\" \
         aria-label=\"Round trip and uptime {}\" preserveAspectRatio=\"none\">",
        H + 22.0,
        span
    );
    let mut points = String::new();
    let mut area = String::new();
    for quarter in [0.0, 0.5, 1.0] {
        let y = H - 6.0 - quarter * (H - 16.0);
        let _ = write!(
            out,
            "<line class=\"grid\" x1=\"0\" x2=\"{W}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>"
        );
    }
    let mut first_x = None;
    let mut last_x = 0.0;
    for (i, bucket) in health.chart.iter().enumerate() {
        let x = i as f64 * step;
        if let Some(rtt) = bucket.rtt {
            let y = H - 6.0 - (f64::from(rtt) / max) * (H - 16.0);
            let _ = write!(points, "{:.1},{:.1} ", x + step / 2.0, y);
            first_x.get_or_insert(x + step / 2.0);
            last_x = x + step / 2.0;
        }
        let (class, said) = match (bucket.ok, bucket.total) {
            (_, 0) => ("none", "not probed".to_owned()),
            (ok, total) if ok == total => ("ok", format!("{ok} of {total} answered")),
            (0, total) => ("bad", format!("none of {total} answered")),
            (ok, total) => ("some", format!("{ok} of {total} answered")),
        };
        let _ = write!(
            out,
            "<rect class=\"{class}\" x=\"{:.1}\" y=\"{}\" width=\"{:.1}\" height=\"12\">\
             <title>{said}</title></rect>",
            x + 0.5,
            H + 6.0,
            (step - 1.0).max(0.5)
        );
    }
    if let Some(first) = first_x {
        let _ = write!(
            area,
            "{first:.1},{:.1} {points}{last_x:.1},{:.1}",
            H - 6.0,
            H - 6.0
        );
    }
    let _ = write!(
        out,
        "<polygon class=\"area\" points=\"{area}\"/><polyline points=\"{points}\"/></svg>\
         <figcaption>Round trip, highest {}, and below, probes answered, {}</figcaption></figure>",
        format::millis(max as u64),
        span
    );
    out
}

fn urls(health: &Health, view: &View) -> String {
    let mut out = String::from("<ul class=\"urls\">");
    for url in &health.urls {
        let state = if url.ok {
            "<span class=\"state ok\">✓</span>"
        } else {
            "<span class=\"state bad\">✗</span>"
        };
        let _ = write!(
            out,
            "<li>{state} <code>{}</code> <span class=\"dim\">{}</span></li>",
            esc(&url.url),
            format::ago(url.at, view.at)
        );
    }
    out.push_str("</ul>");
    out
}

fn hub_body(view: &View, hub: &HubView) -> String {
    let mut out = String::from("<div class=\"split\"><section>");
    out.push_str(&facts(&health_facts(view, &hub.health)));
    out.push_str("</section><section>");
    out.push_str(&chart(view, &hub.health));
    out.push_str("</section></div>");
    out.push_str(&service_time(view, &hub.health));
    let Some(t) = &hub.terms else {
        out.push_str("<section class=\"calm\"><p>Its terms are not known yet.</p></section>");
        return out;
    };
    let mut terms = Vec::new();
    match &t.fees {
        Some(fees) => {
            terms.push(("Pay in Licash", format::terms(&fees.spend)));
            terms.push((
                "On each note made",
                if fees.output_msat == 0 {
                    "None".into()
                } else {
                    format!("{} sat", fees.output_msat as f64 / 1000.0)
                },
            ));
            terms.push(("Locking", format::terms(&fees.lock)));
            terms.push(("Refunding a lock", format::terms(&fees.refund)));
        }
        None => terms.push(("Fees", unknown())),
    }
    match &t.lightning {
        Some(l) => {
            terms.push(("Pay over Lightning", format::terms(&l.pay)));
            terms.push(("Largest Lightning payment", format::sat(l.max_pay)));
            terms.push(("Receive over Lightning", format::terms(&l.receive)));
            terms.push(("Lightning received", span(l.min_receive, l.max_receive)));
        }
        None => terms.push(("Lightning", "Not offered".into())),
    }
    match &t.deposits {
        Some(d) => {
            terms.push(("Receive on Bitcoin", format::terms(&d.fee)));
            terms.push(("Received on Bitcoin", span(d.min, d.max)));
        }
        None => terms.push(("Receive on Bitcoin", "Not offered".into())),
    }
    match &t.protection {
        Some(p) if p.mock => terms.push(("Protection", "Test only: protects nothing".into())),
        Some(p) => {
            if let Some(opening) = p.opening {
                terms.push(("Protection, opening", format::sat(opening)));
            }
            if let Some(ppm) = p.ppm_year {
                terms.push(("Protection, interest", yearly(ppm)));
            }
            if let Some(ppm) = p.ppm_year_funded {
                terms.push(("Protection, interest on her part", yearly(ppm)));
            }
            if let Some(reserve) = p.reserve {
                terms.push(("Protection, reserve", format::sat(reserve)));
            }
            if let (Some(min), Some(max)) = (p.min_cap, p.max_cap) {
                terms.push(("Protection, sizes", span(min, max)));
            } else if let (Some(cap), Some(price)) = (p.cap, p.price) {
                terms.push((
                    "Protection",
                    format!("{} for {}", format::sat(price), format::sat(cap)),
                ));
            }
        }
        None => terms.push(("Protection", "Not offered".into())),
    }
    if let Some(behind) = hub.behind {
        terms.push(("Chain", behind_line(&view.network, behind)));
    }
    if hub.inbound > 0 {
        terms.push((
            "Into it, through providers",
            format!("up to {}", format::sat(hub.inbound)),
        ));
    }
    out.push_str("<div class=\"split\"><section><h2>Terms</h2>");
    out.push_str(&facts(&terms));
    out.push_str("</section><section><h2>Where it answers</h2>");
    out.push_str(&urls(&hub.health, view));
    if hub.announced {
        let _ = write!(
            out,
            "<p class=\"dim\">Announced on Nostr by <code>{}</code>.</p>",
            esc(&format::short(&hub.id))
        );
    }
    out.push_str("</section></div>");
    out.push_str(&recent(view, Some(&hub.id)));
    out
}

fn provider_body(view: &View, provider: &ProviderView) -> String {
    let mut out = String::from("<div class=\"split\"><section>");
    out.push_str(&facts(&health_facts(view, &provider.health)));
    out.push_str("</section><section>");
    out.push_str(&chart(view, &provider.health));
    out.push_str("</section></div>");
    out.push_str(&service_time(view, &provider.health));
    let Some(t) = &provider.terms else {
        out.push_str("<section class=\"calm\"><p>Its terms are not known yet.</p></section>");
        return out;
    };
    let mut terms = vec![
        (
            "Move to another hub",
            t.fee.map_or_else(unknown, |f| format::terms(&f)),
        ),
        (
            "Moves",
            t.min
                .zip(t.max)
                .map_or_else(unknown, |(min, max)| span(min, max)),
        ),
    ];
    match &t.chain {
        Some(c) => {
            terms.push((
                "Pay on Bitcoin",
                format!(
                    "{} + {} for miners",
                    format::terms(&c.fee),
                    format::sat(c.miner)
                ),
            ));
            terms.push(("Paid on Bitcoin", span(c.min, c.max)));
        }
        None => terms.push(("Pay on Bitcoin", "Not offered".into())),
    }
    match &t.lightning {
        Some(l) => {
            terms.push(("Pay over Lightning", format::terms(&l.fee)));
            terms.push(("Largest Lightning payment", format::sat(l.max)));
        }
        None => terms.push(("Pay over Lightning", "Not offered".into())),
    }
    out.push_str("<div class=\"split\"><section><h2>Terms</h2>");
    out.push_str(&facts(&terms));
    out.push_str("<h2>Hubs it moves into</h2><table><tbody>");
    for (id, max) in &provider.hubs {
        let name = view.hubs.iter().find(|h| &h.id == id).map_or_else(
            || "<span class=\"dim\">A hub not watched</span>".to_owned(),
            |h| link(view, &h.id, &title(&h.name, &h.host)),
        );
        let _ = write!(
            out,
            "<tr><td>{name}</td><td class=\"num\">up to {}</td></tr>",
            format::sat(*max)
        );
    }
    out.push_str("</tbody></table></section><section><h2>Where it answers</h2>");
    out.push_str(&urls(&provider.health, view));
    out.push_str("</section></div>");
    out.push_str(&recent(view, Some(&provider.id)));
    out
}

fn unknown() -> String {
    "<span class=\"dim\">Not known</span>".into()
}

/// `1,000 to 50,000 sat`.
fn span(min: u64, max: u64) -> String {
    format!("{} to {}", format::grouped(min), format::sat(max))
}

fn yearly(ppm: u64) -> String {
    format::yearly(u32::try_from(ppm).unwrap_or(u32::MAX))
}

/// A hub's protection in a table's cell: its opening fee and interest.
fn protection_line(p: &crate::terms::Protection) -> String {
    if p.mock {
        return "Test only".into();
    }
    match (p.opening.or(p.price), p.ppm_year) {
        (Some(fee), Some(ppm)) => format!("{} + {}", format::sat(fee), yearly(ppm)),
        (Some(fee), None) => format::sat(fee),
        _ => "Offered".into(),
    }
}

/// How far a hub's chain is behind the others', in time.
fn behind_line(network: &str, blocks: u32) -> String {
    if blocks <= 1 {
        return "In step with the other hubs".into();
    }
    let seconds = if network == "mutinynet" { 30 } else { 600 };
    format!(
        "<span class=\"state bad\">! About {} behind the other hubs</span>",
        format::millis(u64::from(blocks) * seconds * 1000)
    )
}

fn kind_label(kind: &str) -> &str {
    match Kind::parse(kind) {
        Some(kind) => kind.label(),
        None => kind,
    }
}

fn kinds_table(view: &View) -> String {
    let mut out = String::from(
        "<table><thead><tr><th>What</th><th class=\"num\">Succeeded</th>\
         <th class=\"num\">Median time</th><th class=\"num\">Median cost</th>\
         <th class=\"num\">Last</th></tr></thead><tbody>",
    );
    for kind in &view.moves {
        let label = kind_label(&kind.kind);
        let _ = write!(
            out,
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            esc(label),
            opt(kind.rate.share().map(|s| format!(
                "{} ({} of {}){}",
                format::percent(s),
                kind.rate.done,
                kind.rate.total,
                viz::bar(share_class(s), s, &format::percent(s)),
            ))),
            opt(kind.duration.map(|d| {
                let swap = Kind::parse(&kind.kind) == Some(Kind::Chain);
                match swap {
                    true => format!("{} <span class=\"dim\">swap</span>", format::millis(d)),
                    false => format::millis(d),
                }
            })),
            opt(kind.cost.map(format::sat)),
            opt(kind.last.map(|at| format::ago(at, view.at))),
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn name_of(view: &View, id: &Option<String>) -> String {
    let Some(id) = id else {
        return String::new();
    };
    if let Some(h) = view.hubs.iter().find(|h| &h.id == id) {
        return link(view, id, &title(&h.name, &h.host));
    }
    if let Some(p) = view.providers.iter().find(|p| &p.id == id) {
        return link(view, id, &title(&p.name, &p.host));
    }
    esc(&format::short(id))
}

fn recent(view: &View, of: Option<&str>) -> String {
    let actions: Vec<&Action> = view
        .recent
        .iter()
        .filter(|a| {
            of.is_none_or(|id| {
                [&a.source, &a.target, &a.via]
                    .into_iter()
                    .any(|s| s.as_deref() == Some(id))
            })
        })
        .collect();
    if actions.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<section><h2>Latest moves</h2><table><thead><tr><th>When</th><th>What</th>\
         <th>From</th><th>To</th><th>Through</th><th class=\"num\">Amount</th>\
         <th class=\"num\">Took</th><th class=\"num\">Cost</th><th>Outcome</th></tr></thead><tbody>",
    );
    for a in actions {
        let label = kind_label(&a.kind);
        let outcome = match a.outcome {
            Outcome::Done => "<span class=\"state ok\">✓ Done</span>".to_owned(),
            Outcome::Running => "<span class=\"state idle\">… Under way</span>".to_owned(),
            Outcome::Failed => format!(
                "<span class=\"state bad\">✗ Failed</span> <span class=\"dim\">{}</span>",
                esc(a.detail.as_deref().unwrap_or_default())
            ),
        };
        // A move on Bitcoin says its swap's time, and the whole apart.
        let took = budget::took(a).map(|ms| match (a.swapped, a.ended) {
            (Some(_), Some(ended)) => format!(
                "{}<br><span class=\"dim\">{} in all</span>",
                format::millis(ms),
                format::millis(u64::try_from(ended - a.started).unwrap_or(0))
            ),
            _ => format::millis(ms),
        });
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td>{outcome}</td></tr>",
            format::ago(a.started, view.at),
            esc(label),
            name_of(view, &a.source),
            name_of(view, &a.target),
            name_of(view, &a.via),
            format::sat(a.amount),
            opt(took),
            opt(a.cost.map(|c| format::fee_of(c, a.amount))),
        );
    }
    out.push_str("</tbody></table></section>");
    out
}

fn moves_body(view: &View) -> String {
    let mut out = String::new();
    match &view.budget {
        None => out.push_str(
            "<section class=\"calm\"><p>This observatory has no wallet: it watches, and \
             moves nothing.</p></section>",
        ),
        Some(budget) => {
            for note in &budget.notes {
                let _ = write!(
                    out,
                    "<section class=\"calm\"><p><span class=\"state warn\">! {}</span></p></section>",
                    esc(note)
                );
            }
            out.push_str("<section class=\"cards\">");
            out.push_str(&card(
                "Spent",
                &format::sat(budget.spent),
                &format!(
                    "of {} in 30 days{}",
                    format::sat(budget.monthly),
                    viz::bar(
                        "ok",
                        budget.spent as f64 / budget.monthly.max(1) as f64,
                        "Spent of the monthly budget",
                    )
                ),
            ));
            out.push_str(&card(
                "Balance",
                &opt(budget.balance.map(format::sat)),
                "the observatory's, in Licash",
            ));
            out.push_str(
                "</section><section><h2>Budget</h2><table><thead><tr><th>What</th>\
                          <th class=\"num\">Share</th><th class=\"num\">Spent</th>\
                          <th class=\"num\">Next</th></tr></thead><tbody>",
            );
            for k in &budget.kinds {
                let next = if !k.enabled {
                    "<span class=\"dim\">Off</span>".to_owned()
                } else {
                    opt(k.next.map(|at| format::until(at, view.at)))
                };
                let used = if k.allowance > 0 {
                    k.spent as f64 / k.allowance as f64
                } else {
                    0.0
                };
                let class = if used >= 0.9 {
                    "bad"
                } else if used >= 0.6 {
                    "some"
                } else {
                    "ok"
                };
                let _ = write!(
                    out,
                    "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}{}</td>\
                     <td class=\"num\">{next}</td></tr>",
                    esc(k.kind.label()),
                    format::sat(k.allowance),
                    format::sat(k.spent),
                    viz::bar(
                        class,
                        used,
                        &format!("{} of its share spent", format::percent(used.min(1.0))),
                    ),
                );
            }
            out.push_str("</tbody></table></section>");
        }
    }
    if !view.moves.is_empty() {
        out.push_str("<section><h2>By kind</h2>");
        out.push_str(&kinds_table(view));
        out.push_str("</section>");
    }
    out.push_str(&recent(view, None));
    out
}

const STYLE: &str = r#"
:root{--bg:#f7f7f5;--fg:#1c1d1f;--dim:#6b6f76;--line:#e3e3df;--card:#fff;--accent:#1f8a4c;--bad:#c0392b;--warn:#b7791f;--idle:#8a8f98;--tint:#e8f4ec;--tor:#7b6fd6;--work:#5b9fd0}
@media (prefers-color-scheme:dark){:root{--bg:#111214;--fg:#e8e8e6;--dim:#9a9ea6;--line:#26282c;--card:#18191c;--accent:#3ccf7a;--bad:#ff6b5e;--warn:#f0b450;--idle:#7d828a;--tint:#16271d;--tor:#9d93f0;--work:#6bb2e8}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
a{color:inherit;text-decoration-color:var(--line);text-underline-offset:3px}
a:hover{text-decoration-color:var(--accent)}
header{border-bottom:1px solid var(--line);background:var(--card)}
header .bar,header nav,main,footer{max-width:1120px;margin:0 auto;padding:0 20px}
header .bar{display:flex;align-items:center;gap:10px;padding-top:16px}
.brand{font-weight:700;font-size:18px;text-decoration:none;letter-spacing:-.01em}
.brand span{font-weight:400;color:var(--dim)}
.badge{font-size:12px;padding:2px 8px;border-radius:999px;color:var(--dim);font-weight:600;text-decoration:none}
a.badge:hover{color:var(--fg)}
.badge[aria-current],span.badge{background:var(--tint);color:var(--accent)}
.badges{display:inline-flex;gap:4px;flex-wrap:wrap}
.badge.warn{background:transparent;border:1px solid var(--warn);color:var(--warn)}
nav{display:flex;gap:22px;padding-top:10px}
nav a{text-decoration:none;color:var(--dim);padding:8px 0 10px;border-bottom:2px solid transparent}
nav a[aria-current]{color:var(--fg);border-color:var(--accent)}
.heading{display:flex;justify-content:space-between;align-items:baseline;flex-wrap:wrap;gap:12px;margin:28px 0 12px}
h1{font-size:26px;margin:0;letter-spacing:-.02em;overflow-wrap:anywhere}
h2{font-size:16px;margin:28px 0 10px}
h3{font-size:14px;margin:0 0 8px;color:var(--dim);font-weight:600}
.ranges{display:flex;gap:4px;background:var(--card);border:1px solid var(--line);border-radius:999px;padding:3px}
.ranges a{text-decoration:none;font-size:13px;padding:3px 12px;border-radius:999px;color:var(--dim)}
.ranges a[aria-current]{background:var(--tint);color:var(--accent);font-weight:600}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}
.card,.board,table,.chart,.calm{background:var(--card);border:1px solid var(--line);border-radius:16px}
.card{padding:14px 16px}
.card .label{font-size:13px;color:var(--dim)}
.card .value{font-size:24px;font-weight:650;letter-spacing:-.02em;font-variant-numeric:tabular-nums}
.card .note{font-size:12px;color:var(--dim)}
.boards{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:12px}
.board{padding:14px 16px}
.board ol{margin:0;padding-left:22px}
.board li{padding:4px 0;border-bottom:1px solid var(--line)}
.board li:last-child{border:0}
.board li .line{display:flex;justify-content:space-between;align-items:baseline;gap:12px}
.board li span.who{overflow-wrap:anywhere}
.board li .meter{margin-top:6px}
.board li .meter+.meter{margin-top:3px;height:3px}
.legend{display:flex;flex-wrap:wrap;gap:4px 18px;list-style:none;padding:0;margin:12px 0 0;font-size:13px;color:var(--dim)}
.legend b{color:var(--fg);font-variant-numeric:tabular-nums;font-weight:600}
.key{display:inline-block;width:10px;height:10px;border-radius:3px;margin-right:4px;vertical-align:-1px}
.key.ok{background:var(--accent)}.key.tor{background:var(--tor)}.key.work{background:var(--work)}
.meter{display:flex;gap:2px;height:6px;border-radius:999px;overflow:hidden;background:var(--line);width:100%}
.meter i{display:block;height:100%;min-width:2px}
.meter i.ok{background:var(--accent)}.meter i.some{background:var(--warn)}.meter i.bad{background:var(--bad)}
.meter i.tor{background:var(--tor)}.meter i.work{background:var(--work)}
td .meter,.card .meter{height:4px;margin-top:5px;min-width:64px}
.split-bar{background:var(--card);border:1px solid var(--line);border-radius:16px;padding:16px 18px}
.split-bar .meter{height:16px;border-radius:8px}
.strip{display:block;width:96px;height:14px}
td:first-child{min-width:150px}
.strip rect.ok{fill:var(--accent)}.strip rect.some{fill:var(--warn)}.strip rect.bad{fill:var(--bad)}.strip rect.none{fill:var(--line)}
.dots{display:inline-flex;gap:3px;margin-right:6px;vertical-align:middle}
.dots i{width:8px;height:8px;border-radius:50%;background:var(--idle)}
.dots i.ok{background:var(--accent)}.dots i.bad{background:var(--bad)}
.banner{display:flex;align-items:center;gap:12px;margin:0 0 14px;padding:14px 18px;border-radius:16px;border:1px solid var(--line);background:var(--card);font-weight:600}
.banner .pulse{width:12px;height:12px;border-radius:50%;background:var(--idle);flex:none}
.banner.ok{background:var(--tint);border-color:transparent}.banner.ok .pulse{background:var(--accent)}
.banner.some .pulse{background:var(--warn)}.banner.bad .pulse{background:var(--bad)}
@media (prefers-reduced-motion:no-preference){.banner.ok .pulse{animation:pulse 2.6s ease-out infinite}}
@keyframes pulse{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--accent) 45%,transparent)}70%,100%{box-shadow:0 0 0 9px transparent}}
tbody tr:hover td{background:color-mix(in srgb,var(--tint) 55%,transparent)}
.card{transition:border-color .15s}.card:hover{border-color:var(--accent)}
.tag{font-size:11px;color:var(--dim);border:1px solid var(--line);border-radius:6px;padding:0 5px}
table{width:100%;border-collapse:separate;border-spacing:0;overflow:hidden}
th,td{padding:9px 12px;text-align:left;border-bottom:1px solid var(--line);vertical-align:top}
th{font-size:12px;color:var(--dim);font-weight:600}
tr:last-child td{border-bottom:0}
.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
section{margin-bottom:8px;overflow-x:auto}
.calm{padding:14px 18px}
.dim{color:var(--dim)}
.state{font-weight:600;white-space:nowrap}
.state.ok{color:var(--accent)}.state.bad{color:var(--bad)}.state.idle{color:var(--idle)}.state.warn{color:var(--warn)}
.split{display:grid;grid-template-columns:repeat(auto-fit,minmax(340px,1fr));gap:16px}
.facts{display:grid;grid-template-columns:max-content 1fr;gap:6px 18px;margin:0;background:var(--card);border:1px solid var(--line);border-radius:16px;padding:14px 18px}
.facts dt{color:var(--dim)}.facts dd{margin:0;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}
.chart{margin:0;padding:12px}
.chart svg{width:100%;height:150px;display:block}
.chart polyline{fill:none;stroke:var(--accent);stroke-width:2;vector-effect:non-scaling-stroke}
.chart polygon.area{fill:var(--accent);opacity:.13}
.chart line.grid{stroke:var(--line);stroke-width:1;vector-effect:non-scaling-stroke;stroke-dasharray:3 4}
.chart rect.ok{fill:var(--accent)}.chart rect.some{fill:var(--warn)}.chart rect.bad{fill:var(--bad)}.chart rect.none{fill:var(--line)}
.chart figcaption{font-size:12px;color:var(--dim)}
.urls{list-style:none;padding:0;margin:0}.urls li{padding:4px 0;overflow-wrap:anywhere}
code{font:12.5px ui-monospace,SFMono-Regular,Menlo,monospace}
details summary{cursor:pointer;font-weight:600}
footer{color:var(--dim);font-size:12px;padding:28px 20px 40px}
"#;
