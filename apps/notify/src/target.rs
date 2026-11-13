//! Notification targets, named by Apprise-style URLs:
//!
//! | URL | Service |
//! | --- | --- |
//! | `gotify[s]://host[:port]/[path/]TOKEN[?priority=low\|moderate\|normal\|high\|emergency]` | Gotify |
//! | `tgram://BOT_TOKEN/CHAT[:THREAD][/CHAT...][?silent=yes&topic=THREAD]` | Telegram |
//! | `ntfy[s]://[user:pass@]host[:port]/[path/]TOPIC`, `ntfy://TOPIC` (ntfy.sh) | ntfy |
//! | `json[s]://[user:pass@]host[:port]/path[?method=PUT&+Header=value&-param=value]` | a JSON webhook |
//! | `apprise[s]://[user:pass@]host[:port]/[path/]KEY[?tags=TAG&version=2]` | an Apprise API server, and every service it reaches |
//!
//! A target shows only its scheme and host: tokens never reach the logs.

use anyhow::{anyhow, bail, ensure, Context};
use reqwest::{Client, Method, RequestBuilder, Url};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{Message, Priority};

#[derive(Clone)]
pub struct Target {
    id: String,
    shown: String,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    Gotify {
        url: Url,
        token: String,
        priority: Option<u8>,
    },
    Telegram {
        api: String,
        token: String,
        chats: Vec<(String, Option<i64>)>,
        silent: bool,
    },
    Ntfy {
        url: Url,
        topic: String,
        auth: Option<(String, String)>,
    },
    Json {
        url: Url,
        method: Method,
        headers: Vec<(String, String)>,
        auth: Option<(String, String)>,
    },
    Apprise {
        url: Url,
        key: String,
        header: bool,
        tag: Option<String>,
        auth: Option<(String, String)>,
    },
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown)
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Target({})", self.shown)
    }
}

impl std::str::FromStr for Target {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let (scheme, rest) = text
            .split_once("://")
            .ok_or_else(|| anyhow!("a notification URL needs a scheme, such as gotify://"))?;
        let scheme = scheme.to_ascii_lowercase();
        let secure = scheme.ends_with('s') && scheme != "tgram";
        let (kind, host) = match scheme.as_str() {
            "tgram" => (telegram(rest)?, String::new()),
            "gotify" | "gotifys" => {
                let url = http(secure, rest)?;
                let (base, mut segments) = split(&url);
                let token = segments.pop().context("gotify:// needs a token")?;
                let priority = query(&url, "priority").map(|p| gotify_priority(&p));
                let url = join(&base, &segments, "message");
                (
                    Kind::Gotify {
                        url,
                        token,
                        priority,
                    },
                    host(&base),
                )
            }
            "ntfy" | "ntfys" => {
                let url = http(secure, rest)?;
                let (base, mut segments) = split(&url);
                let auth = auth(&url);
                let (url, topic) = match segments.pop() {
                    Some(topic) => (join(&base, &segments, ""), topic),
                    // ntfy://TOPIC is a topic on ntfy.sh, its case kept (a
                    // host would be lowercased).
                    None => {
                        let given = rest.split('?').next().unwrap_or_default();
                        let given = given.rsplit_once('@').map_or(given, |(_, t)| t);
                        let topic = decode(given.trim_end_matches('/'));
                        ensure!(!topic.is_empty(), "ntfy:// needs a topic");
                        (Url::parse("https://ntfy.sh/")?, topic)
                    }
                };
                let shown = host(&url);
                (Kind::Ntfy { url, topic, auth }, shown)
            }
            "json" | "jsons" => {
                let mut url = http(secure, rest)?;
                let method = match query(&url, "method") {
                    Some(m) => Method::from_bytes(m.to_ascii_uppercase().as_bytes())
                        .context("json:// method")?,
                    None => Method::POST,
                };
                // Raw: form decoding reads the `+` of `+Header` as a space.
                let pairs: Vec<(String, String)> = url
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let (k, v) = p.split_once('=').unwrap_or((p, ""));
                        (decode(k), decode(v))
                    })
                    .collect();
                let headers: Vec<_> = pairs
                    .iter()
                    .filter_map(|(k, v)| Some((k.strip_prefix('+')?.to_owned(), v.clone())))
                    .collect();
                for (name, value) in &headers {
                    reqwest::header::HeaderName::from_bytes(name.as_bytes())
                        .context("json:// header name")?;
                    reqwest::header::HeaderValue::from_str(value)
                        .context("json:// header value")?;
                }
                let params: Vec<_> = pairs
                    .iter()
                    .filter_map(|(k, v)| Some((k.strip_prefix('-')?.to_owned(), v.clone())))
                    .collect();
                let auth = auth(&url);
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                if !params.is_empty() {
                    url.query_pairs_mut().extend_pairs(params);
                }
                let shown = host(&url);
                (
                    Kind::Json {
                        url,
                        method,
                        headers,
                        auth,
                    },
                    shown,
                )
            }
            "apprise" | "apprises" => {
                let given = http(secure, rest)?;
                let (base, mut segments) = split(&given);
                let key = segments
                    .pop()
                    .context("apprise:// needs a configuration key")?;
                let kind = Kind::Apprise {
                    url: join(&base, &segments, "notify"),
                    key,
                    header: query(&given, "version").is_some_and(|v| v == "2"),
                    tag: query(&given, "tags"),
                    auth: auth(&given),
                };
                (kind, host(&base))
            }
            _ => bail!("unknown notification scheme {scheme}://"),
        };
        let shown = match &kind {
            Kind::Telegram { .. } => "tgram://***".to_owned(),
            _ => format!("{scheme}://{host}/***"),
        };
        Ok(Self {
            id: hex(&Sha256::digest(text.as_bytes())[..16]),
            shown,
            kind,
        })
    }
}

impl Target {
    /// Stable for the URL as given: where a relay keeps this target's place.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send `message` once.
    pub async fn send(&self, client: &Client, message: &Message) -> anyhow::Result<()> {
        let (title, body) = (&message.title, &message.body);
        match &self.kind {
            Kind::Gotify {
                url,
                token,
                priority,
            } => {
                let priority = priority.unwrap_or(match message.priority {
                    Priority::Low => 3,
                    Priority::Normal => 5,
                    Priority::High => 8,
                });
                let request = client.post(url.clone()).header("X-Gotify-Key", token);
                check(request.json(&json!({"title": title, "message": body, "priority": priority})))
                    .await
            }
            Kind::Telegram {
                api,
                token,
                chats,
                silent,
            } => {
                let text: String = format!("{title}\n{body}").chars().take(4096).collect();
                for (chat, thread) in chats {
                    let mut json = json!({
                        "chat_id": chat,
                        "text": text,
                        "disable_notification": silent,
                        "link_preview_options": {"is_disabled": true},
                    });
                    if let Some(thread) = thread {
                        json["message_thread_id"] = json!(thread);
                    }
                    check(
                        client
                            .post(format!("{api}/bot{token}/sendMessage"))
                            .json(&json),
                    )
                    .await?;
                }
                Ok(())
            }
            Kind::Ntfy { url, topic, auth } => {
                let priority = match message.priority {
                    Priority::Low => 2,
                    Priority::Normal => 3,
                    Priority::High => 4,
                };
                let request = authorized(client.post(url.clone()), auth);
                check(request.json(
                    &json!({"topic": topic, "title": title, "message": body, "priority": priority}),
                ))
                .await
            }
            Kind::Json {
                url,
                method,
                headers,
                auth,
            } => {
                let mut request = client.request(method.clone(), url.clone());
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let json = json!({
                    "version": "1.0",
                    "title": title,
                    "message": body,
                    "attachments": [],
                    "type": apprise_type(message.priority),
                });
                check(authorized(request, auth).json(&json)).await
            }
            Kind::Apprise {
                url,
                key,
                header,
                tag,
                auth,
            } => {
                let request = if *header {
                    client.post(url.clone()).header("X-Apprise-Config-ID", key)
                } else {
                    let mut url = url.clone();
                    url.path_segments_mut().expect("http URL").push(key);
                    client.post(url)
                };
                let mut json =
                    json!({"title": title, "body": body, "type": apprise_type(message.priority)});
                if let Some(tag) = tag {
                    json["tag"] = json!(tag);
                }
                check(authorized(request, auth).json(&json)).await
            }
        }
    }
}

async fn check(request: RequestBuilder) -> anyhow::Result<()> {
    // Errors name the URL, which holds the token.
    let response = request.send().await.map_err(|e| {
        let e = e.without_url();
        let mut text = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            text = format!("{text}: {cause}");
            source = cause.source();
        }
        anyhow!(text)
    })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "answered {status}: {}",
            body.chars().take(200).collect::<String>()
        );
    }
    Ok(())
}

fn authorized(request: RequestBuilder, auth: &Option<(String, String)>) -> RequestBuilder {
    match auth {
        Some((user, password)) => request.basic_auth(user, Some(password)),
        None => request,
    }
}

fn apprise_type(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "info",
        Priority::Normal => "warning",
        Priority::High => "failure",
    }
}

/// Apprise's Gotify priorities, by name or number.
fn gotify_priority(value: &str) -> u8 {
    if let Ok(n) = value.parse::<u8>() {
        return match n {
            0..=2 => 0,
            3..=4 => 3,
            5..=7 => 5,
            8..=9 => 8,
            _ => 10,
        };
    }
    match value.to_ascii_lowercase().chars().next() {
        Some('l') => 0,
        Some('m') => 3,
        Some('h') => 8,
        Some('e') => 10,
        _ => 5,
    }
}

fn telegram(rest: &str) -> anyhow::Result<Kind> {
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    // The token's colon would read as a port: parse by hand.
    let path = path.replace("%3A", ":").replace("%3a", ":");
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let token = parts
        .next()
        .filter(|t| {
            t.split_once(':').is_some_and(|(id, secret)| {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !secret.is_empty()
            })
        })
        .context("tgram:// needs a bot token, as 123456789:ABC...")?;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    let thread = get("topic")
        .or_else(|| get("thread"))
        .map(|t| t.parse::<i64>())
        .transpose()
        .context("tgram:// topic must be a number")?;
    let chats: Vec<_> = parts
        .map(|chat| {
            let threaded = chat
                .rsplit_once(':')
                .and_then(|(chat, t)| Some((chat.to_owned(), Some(t.parse::<i64>().ok()?))));
            threaded.unwrap_or_else(|| (chat.to_owned(), thread))
        })
        .collect();
    ensure!(!chats.is_empty(), "tgram:// needs a chat id");
    Ok(Kind::Telegram {
        api: "https://api.telegram.org".into(),
        token: token.to_owned(),
        chats,
        silent: get("silent").is_some_and(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "yes" | "y" | "true" | "1" | "on"
            )
        }),
    })
}

/// `rest` of a URL, under http or https.
fn http(secure: bool, rest: &str) -> anyhow::Result<Url> {
    let scheme = if secure { "https" } else { "http" };
    // The parser's errors would quote the URL.
    let url = Url::parse(&format!("{scheme}://{rest}"))
        .map_err(|e| anyhow!("notification URL does not parse: {e}"))?;
    ensure!(
        url.host_str().is_some_and(|h| !h.is_empty()),
        "notification URL needs a host"
    );
    Ok(url)
}

/// The URL's origin, and its path segments.
fn split(url: &Url) -> (Url, Vec<String>) {
    let mut base = url.clone();
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    let _ = base.set_username("");
    let _ = base.set_password(None);
    let segments = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).map(decode).collect())
        .unwrap_or_default();
    (base, segments)
}

/// `base` under the path `segments`, then `last` if not empty, else a slash.
fn join(base: &Url, segments: &[String], last: &str) -> Url {
    let mut url = base.clone();
    {
        let mut path = url.path_segments_mut().expect("http URL");
        path.clear().extend(segments);
        if !last.is_empty() {
            path.push(last);
        } else if !segments.is_empty() {
            path.push("");
        }
    }
    url
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.into_owned())
}

fn auth(url: &Url) -> Option<(String, String)> {
    (!url.username().is_empty()).then(|| {
        (
            decode(url.username()),
            url.password().map(decode).unwrap_or_default(),
        )
    })
}

fn host(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use axum::{
        body::Bytes,
        http::{HeaderMap, Method, StatusCode, Uri},
    };
    use serde_json::Value;

    use super::*;

    fn target(url: &str) -> Target {
        url.parse().unwrap()
    }

    fn refused(url: &str) -> String {
        format!("{:#}", url.parse::<Target>().unwrap_err())
    }

    #[test]
    fn gotify_urls() {
        let t = target("gotifys://push.example.com:8443/sub/path/AppToken?priority=high");
        let Kind::Gotify {
            url,
            token,
            priority,
        } = &t.kind
        else {
            panic!()
        };
        assert_eq!(
            url.as_str(),
            "https://push.example.com:8443/sub/path/message"
        );
        assert_eq!((token.as_str(), *priority), ("AppToken", Some(8)));
        assert_eq!(t.to_string(), "gotifys://push.example.com:8443/***");
        assert!(!format!("{t:?}").contains("AppToken"));
        let t = target("gotify://push.example.com/AppToken");
        let Kind::Gotify { url, priority, .. } = &t.kind else {
            panic!()
        };
        assert_eq!(url.as_str(), "http://push.example.com/message");
        assert_eq!(*priority, None);
        for (given, priority) in [
            ("low", 0),
            ("Moderate", 3),
            ("e", 10),
            ("4", 3),
            ("9", 8),
            ("x", 5),
        ] {
            assert_eq!(gotify_priority(given), priority, "{given}");
        }
        assert!(refused("gotify://push.example.com").contains("token"));
    }

    #[test]
    fn telegram_urls() {
        for url in [
            "tgram://123456789:AbC-dEf_1/-100123/@channel:7?silent=yes&topic=5",
            "TGRAM://123456789%3AAbC-dEf_1/-100123/@channel:7?silent=yes&thread=5",
        ] {
            let t = target(url);
            let Kind::Telegram {
                token,
                chats,
                silent,
                ..
            } = &t.kind
            else {
                panic!()
            };
            assert_eq!(token, "123456789:AbC-dEf_1");
            let expected = [
                ("-100123".to_owned(), Some(5)),
                ("@channel".to_owned(), Some(7)),
            ];
            assert_eq!(chats.as_slice(), expected.as_slice());
            assert!(*silent);
            assert_eq!(t.to_string(), "tgram://***");
            assert!(!format!("{t:?}").contains("AbC"));
        }
        assert!(refused("tgram://AbC/-100123").contains("bot token"));
        assert!(refused("tgram://123:AbC").contains("chat"));
        assert!(refused("tgram://123:AbC/1?topic=x").contains("topic"));
    }

    #[test]
    fn ntfy_urls() {
        let t = target("ntfy://MyTopic_1");
        let Kind::Ntfy { url, topic, auth } = &t.kind else {
            panic!()
        };
        assert_eq!(
            (url.as_str(), topic.as_str()),
            ("https://ntfy.sh/", "MyTopic_1")
        );
        assert!(auth.is_none());
        assert_eq!(t.to_string(), "ntfy://ntfy.sh/***");
        let t = target("ntfys://user:p%40ss@ntfy.example.com/prefix/alerts");
        let Kind::Ntfy { url, topic, auth } = &t.kind else {
            panic!()
        };
        assert_eq!(url.as_str(), "https://ntfy.example.com/prefix/");
        assert_eq!(topic, "alerts");
        assert_eq!(auth, &Some(("user".to_owned(), "p@ss".to_owned())));
        assert_eq!(t.to_string(), "ntfys://ntfy.example.com/***");
    }

    #[test]
    fn json_urls() {
        let t =
            target("json://u:p@hook.example.com:8080/path/x?method=put&+X-Token=abc&-key=v%2B1");
        let Kind::Json {
            url,
            method,
            headers,
            auth,
        } = &t.kind
        else {
            panic!()
        };
        assert_eq!(
            url.as_str(),
            "http://hook.example.com:8080/path/x?key=v%2B1"
        );
        assert_eq!(method, Method::PUT);
        assert_eq!(headers, &[("X-Token".to_owned(), "abc".to_owned())]);
        assert_eq!(auth, &Some(("u".to_owned(), "p".to_owned())));
        assert_eq!(t.to_string(), "json://hook.example.com:8080/***");
        let t = target("jsons://hook.example.com/");
        let Kind::Json { url, method, .. } = &t.kind else {
            panic!()
        };
        assert_eq!(
            (url.as_str(), method),
            ("https://hook.example.com/", &Method::POST)
        );
        assert!(refused("json://hook.example.com/?+Bad%20Name=1").contains("header name"));
        assert!(refused("json://hook.example.com/?+X=a%0Ab").contains("header value"));
    }

    #[test]
    fn apprise_urls() {
        let t = target("apprises://user:pw@apprise.example.com/prefix/KEY?tags=ops&version=2");
        let Kind::Apprise {
            url,
            key,
            header,
            tag,
            auth,
        } = &t.kind
        else {
            panic!()
        };
        assert_eq!(url.as_str(), "https://apprise.example.com/prefix/notify");
        assert_eq!((key.as_str(), *header), ("KEY", true));
        assert_eq!(tag.as_deref(), Some("ops"));
        assert_eq!(auth, &Some(("user".to_owned(), "pw".to_owned())));
        assert_eq!(t.to_string(), "apprises://apprise.example.com/***");
        let t = target("apprise://apprise.example.com/KEY");
        let Kind::Apprise { header, tag, .. } = &t.kind else {
            panic!()
        };
        assert!(!header && tag.is_none());
        assert!(refused("apprise://apprise.example.com").contains("key"));
    }

    #[test]
    fn other_urls_are_refused_without_their_secrets() {
        assert!(
            refused("discord://Secret1/Secret2").contains("unknown notification scheme discord://")
        );
        assert!(refused("push.example.com/Secret").contains("scheme"));
        let refusal = refused("gotify:///Secret");
        assert!(!refusal.contains("Secret"), "{refusal}");
        let refusal = refused("gotify://[push.example.com/Secret");
        assert!(!refusal.contains("Secret"), "{refusal}");
    }

    #[test]
    fn ids_follow_the_url() {
        let a = target("gotify://push.example.com/A");
        assert_eq!(a.id(), target(" gotify://push.example.com/A ").id());
        assert_ne!(a.id(), target("gotify://push.example.com/B").id());
        assert_eq!(a.id().len(), 32);
        assert!(a.id().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// What a mock service saw.
    #[derive(Debug)]
    struct Seen {
        method: Method,
        uri: String,
        headers: HeaderMap,
        body: Value,
    }

    struct Mock {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
        /// Requests to answer 500 before taking any.
        fail: Arc<Mutex<usize>>,
    }

    impl Mock {
        async fn start() -> Self {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let fail = Arc::new(Mutex::new(0));
            let state = (seen.clone(), fail.clone());
            let app = axum::Router::new().fallback(
                move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                    let (seen, fail): (Arc<Mutex<Vec<Seen>>>, Arc<Mutex<usize>>) = state.clone();
                    async move {
                        {
                            let mut fail = fail.lock().unwrap();
                            if *fail > 0 {
                                *fail -= 1;
                                return (StatusCode::INTERNAL_SERVER_ERROR, "down");
                            }
                        }
                        seen.lock().unwrap().push(Seen {
                            method,
                            uri: uri.to_string(),
                            headers,
                            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                        });
                        (StatusCode::OK, "{\"ok\":true}")
                    }
                },
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { addr, seen, fail }
        }

        fn take(&self) -> Vec<Seen> {
            std::mem::take(&mut self.seen.lock().unwrap())
        }
    }

    fn message(priority: Priority) -> Message {
        Message::new(priority, "Hub halted", "Restart it.")
    }

    #[tokio::test]
    async fn gotify_sends() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        let t = target(&format!("gotify://{}/sub/AppToken", mock.addr));
        t.send(&client, &message(Priority::High)).await.unwrap();
        let low = target(&format!("gotify://{}/AppToken?priority=low", mock.addr));
        low.send(&client, &message(Priority::High)).await.unwrap();
        let seen = mock.take();
        assert_eq!(
            (&seen[0].method, seen[0].uri.as_str()),
            (&Method::POST, "/sub/message")
        );
        assert_eq!(seen[0].headers["x-gotify-key"], "AppToken");
        assert_eq!(
            seen[0].body,
            json!({"title": "Hub halted", "message": "Restart it.", "priority": 8})
        );
        assert_eq!(seen[1].uri, "/message");
        assert_eq!(seen[1].body["priority"], 0);
    }

    #[tokio::test]
    async fn telegram_sends_to_each_chat() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        let mut t = target("tgram://123456789:AbC/-100123/@channel:7?silent=yes");
        let Kind::Telegram { api, .. } = &mut t.kind else {
            panic!()
        };
        *api = format!("http://{}", mock.addr);
        let long = Message::new(Priority::Normal, "Title", "x".repeat(5000));
        t.send(&client, &long).await.unwrap();
        let seen = mock.take();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].uri, "/bot123456789:AbC/sendMessage");
        let body = &seen[0].body;
        assert_eq!(body["chat_id"], "-100123");
        assert_eq!(body["disable_notification"], true);
        assert_eq!(body["link_preview_options"]["is_disabled"], true);
        assert!(body.get("message_thread_id").is_none());
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with("Title\nxxx"));
        assert_eq!(text.chars().count(), 4096);
        assert_eq!(seen[1].body["chat_id"], "@channel");
        assert_eq!(seen[1].body["message_thread_id"], 7);
    }

    #[tokio::test]
    async fn ntfy_sends_with_credentials() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        let t = target(&format!("ntfy://user:pw@{}/prefix/alerts", mock.addr));
        t.send(&client, &message(Priority::Low)).await.unwrap();
        let seen = mock.take();
        assert_eq!(seen[0].uri, "/prefix/");
        // base64("user:pw")
        assert_eq!(seen[0].headers["authorization"], "Basic dXNlcjpwdw==");
        assert_eq!(
            seen[0].body,
            json!({"topic": "alerts", "title": "Hub halted", "message": "Restart it.", "priority": 2})
        );
    }

    #[tokio::test]
    async fn json_sends_as_apprise_does() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        let t = target(&format!(
            "json://{}/hook?method=put&+X-Token=abc&-key=v",
            mock.addr
        ));
        t.send(&client, &message(Priority::High)).await.unwrap();
        let seen = mock.take();
        assert_eq!(
            (&seen[0].method, seen[0].uri.as_str()),
            (&Method::PUT, "/hook?key=v")
        );
        assert_eq!(seen[0].headers["x-token"], "abc");
        assert_eq!(
            seen[0].body,
            json!({"version": "1.0", "title": "Hub halted", "message": "Restart it.",
                   "attachments": [], "type": "failure"})
        );
    }

    #[tokio::test]
    async fn apprise_sends_by_key() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        let v1 = target(&format!("apprise://{}/KEY?tags=ops", mock.addr));
        v1.send(&client, &message(Priority::Normal)).await.unwrap();
        let v2 = target(&format!("apprise://{}/api/KEY?version=2", mock.addr));
        v2.send(&client, &message(Priority::Low)).await.unwrap();
        let seen = mock.take();
        assert_eq!(seen[0].uri, "/notify/KEY");
        assert_eq!(
            seen[0].body,
            json!({"title": "Hub halted", "body": "Restart it.", "type": "warning", "tag": "ops"})
        );
        assert_eq!(seen[1].uri, "/api/notify");
        assert_eq!(seen[1].headers["x-apprise-config-id"], "KEY");
        assert_eq!(seen[1].body["type"], "info");
    }

    #[tokio::test]
    async fn failures_keep_secrets_out() {
        let mock = Mock::start().await;
        let client = crate::client().unwrap();
        *mock.fail.lock().unwrap() = 1;
        let t = target(&format!("gotify://{}/AppToken", mock.addr));
        let e = format!(
            "{:#}",
            t.send(&client, &message(Priority::Low)).await.unwrap_err()
        );
        assert!(e.contains("500") && e.contains("down"), "{e}");
        // A closed port, under a token in the URL's path.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut t = target("tgram://123456789:SecretPart/1");
        let Kind::Telegram { api, .. } = &mut t.kind else {
            panic!()
        };
        *api = format!("http://{closed}");
        let e = format!(
            "{:#}",
            t.send(&client, &message(Priority::Low)).await.unwrap_err()
        );
        assert!(!e.contains("SecretPart"), "{e}");
        assert!(e.to_lowercase().contains("connect"), "{e}");
    }
}
