//! A SOCKS5 proxy on the loopback interface through the Tor client, for
//! code in the process that brings its own sockets, such as the light
//! client: CONNECT, and Tor's RESOLVE extension, only with its own
//! username and password, each destination host on circuits of its own.

use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use rand_core::{OsRng, RngCore};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::Stream;

/// How long a client has to say what it wants.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// How long a stream or a name has to be found over Tor.
const REACH: Duration = Duration::from_secs(60);

const USER_PASSWORD: u8 = 0x02;
const NO_METHOD: u8 = 0xff;
const CONNECT: u8 = 0x01;
/// Tor's extension: the addresses of a name, in the reply.
const RESOLVE: u8 = 0xf0;
const SUCCEEDED: u8 = 0x00;
const HOST_UNREACHABLE: u8 = 0x04;
const COMMAND_UNSUPPORTED: u8 = 0x07;
const ADDRESS_UNSUPPORTED: u8 = 0x08;

/// A SOCKS5 proxy served through Tor ([`crate::Tor::socks`]) until
/// dropped.
pub struct Socks {
    address: SocketAddr,
    username: String,
    password: String,
    serving: tokio::task::AbortHandle,
}

impl Socks {
    /// Where it listens, on the loopback interface.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The username it takes, random.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The password it takes, random: only code given it uses the proxy,
    /// not other applications on the device.
    pub fn password(&self) -> &str {
        &self.password
    }
}

impl Drop for Socks {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Where the proxy's streams go: Tor, or anything else in tests.
pub(crate) trait Exit: Clone + Send + Sync + 'static {
    /// A stream to `host`, a name or an address, at `port`.
    fn connect(
        &self,
        host: String,
        port: u16,
    ) -> impl Future<Output = Result<Box<dyn Stream>, String>> + Send;

    /// The addresses of `host`.
    fn resolve(&self, host: String) -> impl Future<Output = Result<Vec<IpAddr>, String>> + Send;
}

/// A proxy to `exit` at a free loopback port, served on the current Tokio
/// runtime.
pub(crate) async fn serve(exit: impl Exit) -> std::io::Result<Socks> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let (username, password) = (random(), random());
    let credentials = Arc::new((username.clone(), password.clone()));
    let serving = tokio::spawn(async move {
        loop {
            let client = match listener.accept().await {
                Ok((client, _)) => client,
                Err(e) => {
                    // Out of descriptors, say: not in a busy loop.
                    tracing::debug!("SOCKS: accepting: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (exit, credentials) = (exit.clone(), credentials.clone());
            tokio::spawn(async move {
                if let Err(why) = relay(client, &exit, &credentials).await {
                    tracing::debug!("SOCKS: {why}");
                }
            });
        }
    });
    Ok(Socks {
        address,
        username,
        password,
        serving: serving.abort_handle(),
    })
}

/// 128 random bits, in hex.
fn random() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a client asked for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Connect(String, u16),
    Resolve(String),
}

/// Serve `client` through `exit`, if it knows the credentials.
async fn relay(
    mut client: TcpStream,
    exit: &impl Exit,
    (username, password): &(String, String),
) -> Result<(), String> {
    let request = tokio::time::timeout(HANDSHAKE, handshake(&mut client, username, password))
        .await
        .map_err(|_| "no request in time".to_owned())??;
    match request {
        Request::Connect(host, port) => {
            let stream = tokio::time::timeout(REACH, exit.connect(host.clone(), port))
                .await
                .unwrap_or_else(|_| Err("no stream in time".into()));
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(why) => {
                    reply(&mut client, HOST_UNREACHABLE, None).await?;
                    return Err(format!("{host}:{port}: {why}"));
                }
            };
            reply(&mut client, SUCCEEDED, None).await?;
            tokio::io::copy_bidirectional(&mut client, &mut stream)
                .await
                .map_err(|e| format!("{host}:{port}: {e}"))?;
            Ok(())
        }
        Request::Resolve(host) => {
            let found = tokio::time::timeout(REACH, exit.resolve(host.clone()))
                .await
                .unwrap_or_else(|_| Err("no answer in time".into()));
            match found.as_deref() {
                Ok([ip, ..]) => reply(&mut client, SUCCEEDED, Some(*ip)).await,
                Ok([]) => reply(&mut client, HOST_UNREACHABLE, None).await,
                Err(why) => {
                    reply(&mut client, HOST_UNREACHABLE, None).await?;
                    Err(format!("resolving {host}: {why}"))
                }
            }
        }
    }
}

/// Greet `client`, check its credentials and read what it asks for. A
/// request refused is answered.
async fn handshake(
    client: &mut TcpStream,
    username: &str,
    password: &str,
) -> Result<Request, String> {
    let [version, count] = read::<2>(client).await?;
    if version != 5 {
        return Err(format!("SOCKS version {version}"));
    }
    let offered = read_vec(client, count).await?;
    if !offered.contains(&USER_PASSWORD) {
        write(client, &[5, NO_METHOD]).await?;
        return Err("no username and password offered".into());
    }
    write(client, &[5, USER_PASSWORD]).await?;
    // RFC 1929.
    let [version, length] = read::<2>(client).await?;
    if version != 1 {
        return Err(format!("username and password version {version}"));
    }
    let name = read_vec(client, length).await?;
    let [length] = read::<1>(client).await?;
    let given = read_vec(client, length).await?;
    let known = same(&name, username.as_bytes()) & same(&given, password.as_bytes());
    write(client, &[1, if known { 0 } else { 1 }]).await?;
    if !known {
        return Err("wrong username or password".into());
    }
    let [version, command, _, kind] = read::<4>(client).await?;
    if version != 5 {
        return Err(format!("SOCKS version {version}"));
    }
    let host = match kind {
        1 => Ipv4Addr::from(read::<4>(client).await?).to_string(),
        3 => {
            let [length] = read::<1>(client).await?;
            let name = read_vec(client, length).await?;
            String::from_utf8(name).map_err(|_| "a name not in UTF-8".to_owned())?
        }
        4 => Ipv6Addr::from(read::<16>(client).await?).to_string(),
        _ => {
            reply(client, ADDRESS_UNSUPPORTED, None).await?;
            return Err(format!("address type {kind}"));
        }
    };
    let port = u16::from_be_bytes(read::<2>(client).await?);
    match command {
        CONNECT => Ok(Request::Connect(host, port)),
        RESOLVE => Ok(Request::Resolve(host)),
        _ => {
            reply(client, COMMAND_UNSUPPORTED, None).await?;
            Err(format!("command {command}"))
        }
    }
}

/// Whether `a` is `b`, in time independent of where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// Answer `code`, with `ip` as the bound address: the one resolved.
async fn reply(client: &mut TcpStream, code: u8, ip: Option<IpAddr>) -> Result<(), String> {
    let mut answer = vec![5, code, 0];
    match ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)) {
        IpAddr::V4(ip) => {
            answer.push(1);
            answer.extend(ip.octets());
        }
        IpAddr::V6(ip) => {
            answer.push(4);
            answer.extend(ip.octets());
        }
    }
    answer.extend([0, 0]);
    write(client, &answer).await
}

async fn read<const N: usize>(client: &mut TcpStream) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    client
        .read_exact(&mut bytes)
        .await
        .map_err(|e| format!("reading: {e}"))?;
    Ok(bytes)
}

async fn read_vec(client: &mut TcpStream, length: u8) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; length.into()];
    client
        .read_exact(&mut bytes)
        .await
        .map_err(|e| format!("reading: {e}"))?;
    Ok(bytes)
}

async fn write(client: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    client
        .write_all(bytes)
        .await
        .map_err(|e| format!("writing: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams straight to the loopback interface, and names it knows.
    #[derive(Clone)]
    struct Loopback;

    impl Exit for Loopback {
        async fn connect(&self, host: String, port: u16) -> Result<Box<dyn Stream>, String> {
            let ip = match host.as_str() {
                "echo.example" | "127.0.0.1" => Ipv4Addr::LOCALHOST,
                _ => return Err(format!("no route to {host}")),
            };
            let stream = TcpStream::connect((ip, port))
                .await
                .map_err(|e| e.to_string())?;
            Ok(Box::new(stream))
        }

        async fn resolve(&self, host: String) -> Result<Vec<IpAddr>, String> {
            match host.as_str() {
                "seed.example" => Ok(vec![
                    "192.0.2.7".parse().unwrap(),
                    "2001:db8::7".parse().unwrap(),
                ]),
                "v6.example" => Ok(vec!["2001:db8::6".parse().unwrap()]),
                _ => Err(format!("no name {host}")),
            }
        }
    }

    /// An echo server's port.
    async fn echo() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut from, mut to) = stream.split();
                    let _ = tokio::io::copy(&mut from, &mut to).await;
                });
            }
        });
        port
    }

    /// A client through `socks` with these credentials, its request's
    /// address `to` as SOCKS encodes it: the reply code and bound address.
    async fn ask(
        socks: &Socks,
        user: &str,
        password: &str,
        command: u8,
        to: &[u8],
    ) -> (TcpStream, u8, Vec<u8>) {
        let mut client = TcpStream::connect(socks.address()).await.unwrap();
        client.write_all(&[5, 1, USER_PASSWORD]).await.unwrap();
        assert_eq!(read::<2>(&mut client).await.unwrap(), [5, USER_PASSWORD]);
        let mut auth = vec![1, user.len() as u8];
        auth.extend(user.as_bytes());
        auth.push(password.len() as u8);
        auth.extend(password.as_bytes());
        client.write_all(&auth).await.unwrap();
        let [_, status] = read::<2>(&mut client).await.unwrap();
        if status != 0 {
            return (client, 0xff, vec![]);
        }
        let mut request = vec![5, command, 0];
        request.extend(to);
        client.write_all(&request).await.unwrap();
        let [_, code, _, kind] = read::<4>(&mut client).await.unwrap();
        let bound = match kind {
            1 => read_vec(&mut client, 4).await.unwrap(),
            _ => read_vec(&mut client, 16).await.unwrap(),
        };
        read::<2>(&mut client).await.unwrap();
        (client, code, bound)
    }

    fn name(host: &str, port: u16) -> Vec<u8> {
        let mut to = vec![3, host.len() as u8];
        to.extend(host.as_bytes());
        to.extend(port.to_be_bytes());
        to
    }

    #[tokio::test]
    async fn streams_go_through_it_by_name_or_address() {
        let (socks, port) = (serve(Loopback).await.unwrap(), echo().await);
        assert!(socks.address().ip().is_loopback());
        let (user, password) = (socks.username().to_owned(), socks.password().to_owned());
        assert_eq!((user.len(), password.len()), (32, 32));
        assert_ne!(user, password);

        let mut address = vec![1, 127, 0, 0, 1];
        address.extend(port.to_be_bytes());
        for to in [name("echo.example", port), address] {
            let (mut client, code, _) = ask(&socks, &user, &password, CONNECT, &to).await;
            assert_eq!(code, SUCCEEDED);
            client.write_all(b"ping").await.unwrap();
            assert_eq!(&read::<4>(&mut client).await.unwrap(), b"ping");
        }
        let (_, code, _) = ask(
            &socks,
            &user,
            &password,
            CONNECT,
            &name("far.example", port),
        )
        .await;
        assert_eq!(code, HOST_UNREACHABLE);
    }

    #[tokio::test]
    async fn names_resolve_through_it_to_their_first_address() {
        let socks = serve(Loopback).await.unwrap();
        let (user, password) = (socks.username().to_owned(), socks.password().to_owned());
        let (_, code, bound) =
            ask(&socks, &user, &password, RESOLVE, &name("seed.example", 0)).await;
        assert_eq!((code, bound), (SUCCEEDED, vec![192, 0, 2, 7]));
        let (_, code, bound) = ask(&socks, &user, &password, RESOLVE, &name("v6.example", 0)).await;
        let v6: Ipv6Addr = "2001:db8::6".parse().unwrap();
        assert_eq!((code, bound), (SUCCEEDED, v6.octets().to_vec()));
        let (_, code, _) = ask(&socks, &user, &password, RESOLVE, &name("none.example", 0)).await;
        assert_eq!(code, HOST_UNREACHABLE);
        // Other commands, BIND say, are refused.
        let (_, code, _) = ask(&socks, &user, &password, 0x02, &name("seed.example", 0)).await;
        assert_eq!(code, COMMAND_UNSUPPORTED);
    }

    #[tokio::test]
    async fn only_its_credentials_open_it() {
        let (socks, port) = (serve(Loopback).await.unwrap(), echo().await);
        let (user, password) = (socks.username().to_owned(), socks.password().to_owned());
        let to = name("echo.example", port);
        for (user, password) in [
            (user.as_str(), "wrong"),
            ("wrong", password.as_str()),
            ("", ""),
        ] {
            let (mut client, code, _) = ask(&socks, user, password, CONNECT, &to).await;
            assert_eq!(code, 0xff, "refused");
            assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0, "closed");
        }
        // Without offering a username and password, none.
        let mut client = TcpStream::connect(socks.address()).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        assert_eq!(read::<2>(&mut client).await.unwrap(), [5, NO_METHOD]);
    }

    /// Through Arti: a name resolved, and a stream opened.
    #[tokio::test]
    #[ignore = "reaches the Tor network"]
    async fn it_resolves_and_connects_over_tor() {
        let dir = tempfile::tempdir().unwrap();
        let socks = crate::Tor::new(dir.path()).unwrap().socks().await.unwrap();
        let (user, password) = (socks.username().to_owned(), socks.password().to_owned());
        let host = "check.torproject.org";
        let (_, code, bound) = ask(&socks, &user, &password, RESOLVE, &name(host, 0)).await;
        assert_eq!(code, SUCCEEDED);
        assert!(bound.iter().any(|b| *b != 0), "{bound:?}");
        let (_, code, _) = ask(&socks, &user, &password, CONNECT, &name(host, 443)).await;
        assert_eq!(code, SUCCEEDED);
    }

    #[tokio::test]
    async fn it_stops_once_dropped() {
        let socks = serve(Loopback).await.unwrap();
        let address = socks.address();
        drop(socks);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(TcpStream::connect(address).await.is_err());
    }
}
