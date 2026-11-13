//! The service over its router, in process: each endpoint's body, status
//! and problem code.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{Output, Proposal},
    unlink::{Nonce, Session},
};
use licash_api::{Code, Info, Offer, Offered, Problem, Release, Released, Unlink, Unlinked};
use licash_hub::{Fee, Hub, Policy};
use licash_hub_service::notify::Notifier;
use licash_notify::Timing;
use serde::de::DeserializeOwned;
use tower::ServiceExt;

const MAX_POW: u32 = 6;

struct Service {
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
    app: Router,
    keys: IssuerKeys,
    keyset: Keyset,
}

fn service(fee: u64) -> Service {
    let dir = tempfile::tempdir().unwrap();
    let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
    Hub::init(&seed, &db, MAX_POW).unwrap();
    let policy = Policy {
        spend: Fee { base: fee, ppm: 0 },
    };
    let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
    let mut secret = [0; 32];
    hex::decode_to_slice(std::fs::read_to_string(&seed).unwrap().trim(), &mut secret).unwrap();
    let keys = IssuerKeys::from_seed(&secret, 0, MAX_POW).unwrap();
    let keyset = keys.keyset().unwrap();
    Service {
        dir,
        app: licash_hub_service::router(hub.clone()),
        hub,
        keys,
        keyset,
    }
}

impl Service {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec())
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> T {
        let (status, body) = self
            .call(
                Method::POST,
                path,
                Some(serde_json::to_string(body).unwrap()),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    /// The problem a request is answered with, checked against its status.
    async fn problem(&self, path: &str, body: String) -> Problem {
        let (status, body) = self.call(Method::POST, path, Some(body)).await;
        let problem: Problem = serde_json::from_slice(&body).unwrap();
        assert_eq!(status.as_u16(), problem.code.status(), "{problem:?}");
        problem
    }

    async fn info(&self) -> Info {
        let (status, body) = self.call(Method::GET, licash_api::INFO, None).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    fn mint(&self, to: &OwnerKeys, denom: u64) -> Coin {
        mint_under(&self.keys, &self.keyset, to, denom)
    }
}

fn mint_under(keys: &IssuerKeys, keyset: &Keyset, to: &OwnerKeys, denom: u64) -> Coin {
    let d = Ephemeral::random()
        .derive(&to.address(), denom, Branch::Pay, 0)
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
    Coin {
        note,
        derivation: d,
        owner: to.clone(),
    }
}

struct Coin {
    note: Note,
    derivation: Derivation,
    owner: OwnerKeys,
}

/// A spend of `coin` to `outputs`, with the derivations of its outputs.
fn spend(keyset: &Keyset, coin: &Coin, outputs: &[(Address, u64)]) -> (Proposal, Vec<Derivation>) {
    let e = Ephemeral::random();
    let outputs: Vec<_> = outputs
        .iter()
        .enumerate()
        .map(|(i, (to, denom))| e.derive(to, *denom, Branch::Pay, i).unwrap())
        .collect();
    let proposal = Proposal::new(
        keyset,
        std::slice::from_ref(&coin.note),
        e.public(),
        outputs
            .iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect(),
        None,
    )
    .unwrap();
    (proposal, outputs)
}

fn sign(
    keyset: &Keyset,
    coin: &Coin,
    proposal: &Proposal,
    authorization: Authorization,
) -> SignedProposal {
    let secret = coin
        .derivation
        .ownership()
        .owner_secret(coin.owner.spend_secret())
        .unwrap();
    SignedProposal::sign(
        keyset,
        proposal.clone(),
        std::slice::from_ref(&coin.note),
        authorization,
        &[secret],
    )
    .unwrap()
}

fn owner(seed: u8) -> OwnerKeys {
    OwnerKeys::from_seed(&[seed; 32]).unwrap()
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

#[tokio::test]
async fn a_wallet_spends_and_unlinks_through_the_api() {
    let service = service(2);
    let (status, body) = service
        .call(Method::GET, licash_hub_service::HEALTH, None)
        .await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"ok"[..]));
    let info = service.info().await;
    assert_eq!(info.keysets().unwrap(), vec![service.keyset.clone()]);
    assert_eq!(info.fees.spend, licash_api::Fee { base: 2, ppm: 0 });
    let keyset = &service.keyset;
    let (alice, bob) = (owner(1), owner(2));
    let coin = service.mint(&alice, 8);
    let (proposal, outputs) = spend(
        keyset,
        &coin,
        &[(bob.address(), 4), (alice.address(), 2), (info.address, 2)],
    );
    let notes = vec![coin.note.clone()];

    let offered: Offered = service
        .post(licash_api::OFFER, &Offer::new(&proposal, &notes))
        .await;
    let grant = offered.decode().unwrap();
    grant.verify(keyset, &proposal).unwrap();
    let signed = sign(
        keyset,
        &coin,
        &proposal,
        Authorization::Granted(Box::new(grant)),
    );
    let released: Released = service
        .post(licash_api::RELEASE, &Release::new(&signed, &notes))
        .await;
    let unlock = released.decode().unwrap();
    let paid = signed
        .open_granted_output(keyset, &notes, 0, &outputs[0], &unlock)
        .unwrap();

    let session = Session::start(keyset, &paid, &outputs[0]).unwrap();
    let unlinked: Unlinked = service
        .post(licash_api::UNLINK, &Unlink::new(session.request()))
        .await;
    let unlinked = session.finish(keyset, &unlinked.decode().unwrap()).unwrap();
    assert!(matches!(unlinked.evidence(), Evidence::Unlinked(_)));
}

#[tokio::test]
async fn refusals_are_problems_with_their_codes() {
    let service = service(1);
    let keyset = &service.keyset;
    let address = service.info().await.address;
    let (alice, bob) = (owner(1), owner(2));
    let problem = |path, body| service.problem(path, body);

    let coin = service.mint(&alice, 8);
    let notes = vec![coin.note.clone()];
    let (unpaid, _) = spend(keyset, &coin, &[(bob.address(), 8)]);
    let offer = json(&Offer::new(&unpaid, &notes));
    assert_eq!(problem(licash_api::OFFER, offer).await.code, Code::Fee);

    let (paid, _) = spend(
        keyset,
        &coin,
        &[(bob.address(), 4), (bob.address(), 2), (address, 2)],
    );
    // Signed for a grant the hub never offered.
    let foreign = licash::authorization::PreparedGrant::new(&service.keys, &paid)
        .unwrap()
        .offer();
    let signed = sign(
        keyset,
        &coin,
        &paid,
        Authorization::Granted(Box::new(foreign)),
    );
    let release = json(&Release::new(&signed, &notes));
    assert_eq!(
        problem(licash_api::RELEASE, release).await.code,
        Code::UnknownOffer
    );

    // Two spends of one coin: the second is refused as spent.
    let (rival, _) = spend(
        keyset,
        &coin,
        &[(alice.address(), 4), (alice.address(), 2), (address, 2)],
    );
    let mut signed = Vec::new();
    for proposal in [&paid, &rival] {
        let offered: Offered = service
            .post(licash_api::OFFER, &Offer::new(proposal, &notes))
            .await;
        let grant = offered.decode().unwrap();
        signed.push(sign(
            keyset,
            &coin,
            proposal,
            Authorization::Granted(Box::new(grant)),
        ));
    }
    let _: Released = service
        .post(licash_api::RELEASE, &Release::new(&signed[0], &notes))
        .await;
    let release = json(&Release::new(&signed[1], &notes));
    let refused = problem(licash_api::RELEASE, release).await;
    assert_eq!(
        refused,
        Problem::new(Code::Spent, "refused: an input is already spent")
    );

    let other = IssuerKeys::random(MAX_POW).unwrap();
    let other_keyset = other.keyset().unwrap();
    let elsewhere = mint_under(&other, &other_keyset, &alice, 8);
    let (proposal, _) = spend(&other_keyset, &elsewhere, &[(bob.address(), 8)]);
    let offer = json(&Offer::new(
        &proposal,
        std::slice::from_ref(&elsewhere.note),
    ));
    assert_eq!(
        problem(licash_api::OFFER, offer).await.code,
        Code::UnknownKeyset
    );

    let never = service.mint(&bob, 4);
    let session = Session::start(keyset, &never.note, &never.derivation).unwrap();
    let unlink = json(&Unlink::new(session.request()));
    assert_eq!(
        problem(licash_api::UNLINK, unlink).await.code,
        Code::NotIssued
    );
}

#[tokio::test]
async fn malformed_bodies_are_invalid() {
    let service = service(0);
    for body in [
        "not json".to_owned(),
        r#"{"request":"0g"}"#.to_owned(),
        r#"{"request":"00"}"#.to_owned(),
        format!(
            r#"{{"request":"{}"}}"#,
            "00".repeat(licash_hub_service::BODY_LIMIT)
        ),
    ] {
        let problem = service.problem(licash_api::UNLINK, body).await;
        assert_eq!(problem.code, Code::Invalid, "{problem:?}");
    }
    let (status, body) = service.call(Method::POST, licash_api::OFFER, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let problem: Problem = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem.code, Code::Invalid);
}

#[tokio::test]
async fn a_halted_hub_is_unhealthy_and_serves_nothing() {
    let service = service(0);
    let alice = owner(1);
    let coin = service.mint(&alice, 8);
    let (proposal, _) = spend(&service.keyset, &coin, &[(alice.address(), 8)]);
    let offer = json(&Offer::new(&proposal, std::slice::from_ref(&coin.note)));
    // The database changed under the hub, as a restored backup would.
    rusqlite::Connection::open(service.dir.path().join("hub.sqlite"))
        .unwrap()
        .execute("UPDATE meta SET seq = seq + 7", [])
        .unwrap();
    let problem = service.problem(licash_api::OFFER, offer).await;
    assert_eq!(
        problem,
        Problem::new(Code::Unavailable, "the hub cannot serve now")
    );
    let (status, body) = service
        .call(Method::GET, licash_hub_service::HEALTH, None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let problem: Problem = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem.code, Code::Unavailable);
}

/// A JSON webhook, and the titles it took.
async fn hook() -> (licash_notify::Target, Arc<Mutex<Vec<String>>>) {
    let titles = Arc::new(Mutex::new(Vec::new()));
    let took = titles.clone();
    let app = Router::new().fallback(move |body: axum::body::Bytes| {
        let took = took.clone();
        async move {
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            took.lock()
                .unwrap()
                .push(body["title"].as_str().unwrap().to_owned());
            StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("json://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (target.parse().unwrap(), titles)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn the_operator_hears_of_spends_a_halt_and_the_stop() {
    let service = service(0);
    let (target, titles) = hook().await;
    let dir = service.dir.path();
    let fast = Timing {
        poll: Duration::from_millis(10),
        retry: Duration::from_millis(10),
        max_retry: Duration::from_millis(40),
    };
    let notifier = Notifier::new(
        &dir.join("hub.sqlite"),
        "north",
        vec![target],
        &dir.join("notify.json"),
        vec![],
    )
    .unwrap()
    .timing(fast);
    let running = tokio::spawn(notifier.clone().run(service.hub.clone()));

    let alice = owner(1);
    let coin = service.mint(&alice, 8);
    let (proposal, _) = spend(&service.keyset, &coin, &[(owner(2).address(), 8)]);
    let notes = vec![coin.note.clone()];
    let offered: Offered = service
        .post(licash_api::OFFER, &Offer::new(&proposal, &notes))
        .await;
    let grant = Authorization::Granted(Box::new(offered.decode().unwrap()));
    let signed = sign(&service.keyset, &coin, &proposal, grant);
    let _: Released = service
        .post(licash_api::RELEASE, &Release::new(&signed, &notes))
        .await;
    until("released", || titles.lock().unwrap().len() == 1).await;
    assert_eq!(titles.lock().unwrap()[0], "north: Spend released");

    // The database changed under the hub.
    rusqlite::Connection::open(dir.join("hub.sqlite"))
        .unwrap()
        .execute("UPDATE meta SET seq = seq + 7", [])
        .unwrap();
    let (proposal, _) = spend(&service.keyset, &coin, &[(alice.address(), 8)]);
    service
        .problem(licash_api::OFFER, json(&Offer::new(&proposal, &notes)))
        .await;
    until("halted", || titles.lock().unwrap().len() == 2).await;
    assert_eq!(titles.lock().unwrap()[1], "north: Hub halted");

    running.abort();
    notifier.stopped().await;
    assert_eq!(titles.lock().unwrap()[2..], ["north: Hub stopped"]);
}
