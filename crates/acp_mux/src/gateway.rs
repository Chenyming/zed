use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _};
use rand::{TryRngCore as _, rngs::OsRng};
use serde_json::{Value, json};
use smol::{
    channel::{Receiver, Sender},
    net::{TcpListener, TcpStream},
};
use std::{
    collections::{HashMap, VecDeque},
    io::Write as _,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt as _,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    GatewayConfig, PROTOCOL_VERSION, daemon::ClientIds, default_token_path, expand_home,
    router::Event,
};

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const PING_INTERVAL: Duration = Duration::from_secs(25);
const LOGIN_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURES_PER_WINDOW: usize = 10;
const MIN_TOKEN_LENGTH: usize = 16;
const SESSION_COOKIE: &str = "acp_mux_session";

struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
}

const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        body: include_str!("../web/index.html"),
    },
    Asset {
        path: "/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../web/app.js"),
    },
    Asset {
        path: "/style.css",
        content_type: "text/css; charset=utf-8",
        body: include_str!("../web/style.css"),
    },
    Asset {
        path: "/sw.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../web/sw.js"),
    },
    Asset {
        path: "/manifest.webmanifest",
        content_type: "application/manifest+json",
        body: include_str!("../web/manifest.webmanifest"),
    },
    Asset {
        path: "/icon.svg",
        content_type: "image/svg+xml",
        body: include_str!("../web/icon.svg"),
    },
];

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; connect-src 'self'; \
    img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'none'";

struct Gateway {
    token: String,
    config: GatewayConfig,
    version: String,
    /// Login tokens handed out by `/api/v1/login`, with their expiry.
    logins: Mutex<HashMap<String, Instant>>,
    /// Recent failed logins. Behind a proxy every request comes from the same
    /// address, so failures are limited globally rather than per address.
    failures: Mutex<VecDeque<Instant>>,
}

pub async fn serve(
    config: GatewayConfig,
    events: Sender<Event>,
    client_ids: ClientIds,
    version: String,
) -> Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("listening on {}", config.listen))?;
    run(listener, config, events, client_ids, version).await
}

async fn run(
    listener: TcpListener,
    config: GatewayConfig,
    events: Sender<Event>,
    client_ids: ClientIds,
    version: String,
) -> Result<()> {
    let token_path = config
        .token_file
        .as_deref()
        .map(expand_home)
        .unwrap_or_else(default_token_path);
    let token = load_or_create_token(&token_path)?;
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        log::warn!(
            "the web gateway listens on {address}, which is reachable from other hosts; \
             put it behind a VPN or an HTTPS proxy"
        );
    }
    log::info!(
        "web gateway listening on {address}; login token in {}",
        token_path.display()
    );
    let gateway = Arc::new(Gateway {
        token,
        config,
        version,
        logins: Mutex::new(HashMap::new()),
        failures: Mutex::new(VecDeque::new()),
    });
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                log::error!("accepting a gateway connection failed: {error}");
                continue;
            }
        };
        smol::spawn({
            let gateway = gateway.clone();
            let events = events.clone();
            let client_ids = client_ids.clone();
            async move {
                if let Err(error) =
                    handle_connection(stream, peer, gateway, events, client_ids).await
                {
                    log::debug!("gateway connection from {peer}: {error:#}");
                }
            }
        })
        .detach();
    }
}

fn load_or_create_token(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let token = contents.trim().to_string();
            if token.len() < MIN_TOKEN_LENGTH {
                bail!(
                    "the token in {} must be at least {MIN_TOKEN_LENGTH} characters",
                    path.display()
                );
            }
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let token = random_token()?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            writeln!(file, "{token}")?;
            log::info!("created a login token in {}", path.display());
            Ok(token)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| anyhow!("generating a token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
    }

    fn bearer_token(&self) -> Option<&str> {
        let value = self.header("authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }
}

/// Reads one HTTP/1.1 request. Returns `None` when the client closed the
/// connection without sending one.
async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            bail!("request head too large");
        }
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            bail!("connection closed in the middle of a request");
        }
        buffer.extend_from_slice(&chunk[..count]);
    };

    let head = std::str::from_utf8(&buffer[..head_end]).context("request head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context("missing method")?.to_string();
    let target = parts.next().context("missing request target")?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect::<Vec<_>>();

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .context("invalid Content-Length")?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        bail!("request body too large");
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            bail!("connection closed in the middle of a request body");
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(content_length);

    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, reason: &'static str, body: Value) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json",
            headers: vec![("Cache-Control", "no-store".to_string())],
            body: body.to_string().into_bytes(),
        }
    }

    fn error(status: u16, reason: &'static str, message: &str) -> Self {
        Self::json(status, reason, json!({"error": message}))
    }

    async fn write(self, stream: &mut TcpStream) -> Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
             X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&self.body).await?;
        stream.flush().await?;
        Ok(())
    }
}

impl Gateway {
    fn is_authenticated(&self, request: &Request) -> bool {
        if let Some(token) = request.bearer_token()
            && (constant_time_eq(token, &self.token) || self.is_valid_login(token))
        {
            return true;
        }
        request
            .cookie(SESSION_COOKIE)
            .is_some_and(|login| self.is_valid_login(login))
    }

    fn is_valid_login(&self, login: &str) -> bool {
        let Ok(mut logins) = self.logins.lock() else {
            return false;
        };
        let now = Instant::now();
        logins.retain(|_, expiry| *expiry > now);
        logins.keys().any(|known| constant_time_eq(known, login))
    }

    fn too_many_failures(&self) -> bool {
        let Ok(mut failures) = self.failures.lock() else {
            return true;
        };
        let now = Instant::now();
        while failures
            .front()
            .is_some_and(|failure| now.duration_since(*failure) > FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        failures.len() >= MAX_FAILURES_PER_WINDOW
    }

    fn record_failure(&self) {
        if let Ok(mut failures) = self.failures.lock() {
            failures.push_back(Instant::now());
        }
    }

    fn cookie(&self, value: &str, max_age: u64) -> String {
        let secure = if self.config.cookie_secure {
            "; Secure"
        } else {
            ""
        };
        format!(
            "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
        )
    }

    fn login(&self, request: &Request, peer: &str) -> Response {
        if self.too_many_failures() {
            log::warn!("rejected a login from {peer}: too many failed attempts");
            return Response::error(429, "Too Many Requests", "too many failed attempts");
        }
        let token = serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body.get("token")?.as_str().map(str::to_string));
        if !token.is_some_and(|token| constant_time_eq(&token, &self.token)) {
            self.record_failure();
            log::warn!("failed login from {peer}");
            return Response::error(401, "Unauthorized", "invalid token");
        };
        let login = match random_token() {
            Ok(login) => login,
            Err(error) => {
                log::error!("{error:#}");
                return Response::error(500, "Internal Server Error", "cannot create a login");
            }
        };
        if let Ok(mut logins) = self.logins.lock() {
            logins.insert(login.clone(), Instant::now() + LOGIN_LIFETIME);
        }
        log::info!("login from {peer}");
        let mut response = Response::json(200, "OK", json!({"ok": true, "token": login}));
        response
            .headers
            .push(("Set-Cookie", self.cookie(&login, LOGIN_LIFETIME.as_secs())));
        response
    }

    fn logout(&self, request: &Request) -> Response {
        if let Ok(mut logins) = self.logins.lock() {
            for login in [request.cookie(SESSION_COOKIE), request.bearer_token()]
                .into_iter()
                .flatten()
            {
                logins.remove(login);
            }
        }
        let mut response = Response::json(200, "OK", json!({"ok": true}));
        response.headers.push(("Set-Cookie", self.cookie("", 0)));
        response
    }

    /// Browsers attach cookies to WebSocket handshakes from any page, so only
    /// pages served by this gateway (or explicitly allowed origins) may connect.
    /// Native apps send no `Origin`.
    fn is_allowed_origin(&self, request: &Request) -> bool {
        let Some(origin) = request.header("origin") else {
            return true;
        };
        let origin = origin.trim_end_matches('/');
        if self
            .config
            .allowed_origins
            .iter()
            .any(|allowed| allowed.trim_end_matches('/') == origin)
        {
            return true;
        }
        let Some((_, origin_host)) = origin.split_once("://") else {
            return false;
        };
        [request.header("host"), request.header("x-forwarded-host")]
            .into_iter()
            .flatten()
            .any(|host| host.eq_ignore_ascii_case(origin_host))
    }
}

fn describe_peer(request: &Request, peer: SocketAddr) -> String {
    match request.header("x-forwarded-for") {
        Some(forwarded) => format!("{forwarded} via {peer}"),
        None => peer.to_string(),
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    gateway: Arc<Gateway>,
    events: Sender<Event>,
    client_ids: ClientIds,
) -> Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let peer = describe_peer(&request, peer);

    let response = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/v1/acp") => {
            return upgrade(stream, request, peer, gateway, events, client_ids).await;
        }
        ("GET", "/api/v1/health") => Response::json(
            200,
            "OK",
            json!({
                "ok": true,
                "version": gateway.version,
                "protocol": PROTOCOL_VERSION,
                "authenticated": gateway.is_authenticated(&request),
            }),
        ),
        ("POST", "/api/v1/login") => gateway.login(&request, &peer),
        ("POST", "/api/v1/logout") => gateway.logout(&request),
        ("GET", path) => {
            let path = if path == "/index.html" { "/" } else { path };
            match ASSETS.iter().find(|asset| asset.path == path) {
                Some(asset) => Response {
                    status: 200,
                    reason: "OK",
                    content_type: asset.content_type,
                    headers: vec![
                        ("Cache-Control", "no-cache".to_string()),
                        (
                            "Content-Security-Policy",
                            CONTENT_SECURITY_POLICY.to_string(),
                        ),
                    ],
                    body: asset.body.as_bytes().to_vec(),
                },
                None => Response::error(404, "Not Found", "not found"),
            }
        }
        _ => Response::error(405, "Method Not Allowed", "method not allowed"),
    };
    response.write(&mut stream).await
}

async fn upgrade(
    mut stream: TcpStream,
    request: Request,
    peer: String,
    gateway: Arc<Gateway>,
    events: Sender<Event>,
    client_ids: ClientIds,
) -> Result<()> {
    let is_websocket = request
        .header("upgrade")
        .is_some_and(|upgrade| upgrade.eq_ignore_ascii_case("websocket"));
    let key = request.header("sec-websocket-key");
    let (true, Some(key), Some("13")) =
        (is_websocket, key, request.header("sec-websocket-version"))
    else {
        return Response::error(400, "Bad Request", "expected a WebSocket handshake")
            .write(&mut stream)
            .await;
    };
    if !gateway.is_authenticated(&request) {
        return Response::error(401, "Unauthorized", "login required")
            .write(&mut stream)
            .await;
    }
    if !gateway.is_allowed_origin(&request) {
        log::warn!(
            "rejected a WebSocket from {peer} with origin {:?}",
            request.header("origin")
        );
        return Response::error(403, "Forbidden", "origin not allowed")
            .write(&mut stream)
            .await;
    }

    let accept = derive_accept_key(key.as_bytes());
    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    stream.flush().await?;
    let socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;

    let client = client_ids.next();
    let (outgoing, outgoing_rx) = smol::channel::unbounded();
    let agent = request
        .header("user-agent")
        .map(|agent| agent.chars().take(60).collect::<String>())
        .unwrap_or_default();
    events
        .send(Event::Observe {
            client,
            label: format!("web {peer} {agent}"),
            outgoing,
        })
        .await?;
    bridge(socket, client, events, outgoing_rx).await;
    Ok(())
}

/// Passes ACP messages between a WebSocket and the router until either side
/// closes. Each text frame carries one JSON-RPC message.
async fn bridge(
    socket: WebSocketStream<TcpStream>,
    client: u64,
    events: Sender<Event>,
    outgoing: Receiver<Value>,
) {
    let (mut sink, mut source) = socket.split();
    let writer = smol::spawn(async move {
        let mut ping = smol::Timer::interval(PING_INTERVAL);
        loop {
            let message = outgoing.recv();
            let tick = ping.next();
            futures::pin_mut!(message);
            let sent = match futures::future::select(message, tick).await {
                futures::future::Either::Left((Ok(value), _)) => {
                    sink.send(Message::text(value.to_string())).await
                }
                futures::future::Either::Left((Err(_), _)) => {
                    sink.send(Message::Close(None)).await.ok();
                    return;
                }
                futures::future::Either::Right(_) => {
                    // Keeps proxies such as frp from closing an idle connection.
                    sink.send(Message::Ping(Default::default())).await
                }
            };
            if let Err(error) = sent {
                log::debug!("writing to web client {client}: {error}");
                return;
            }
        }
    });

    while let Some(message) = source.next().await {
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<Value>(text.as_str()) {
                Ok(message) => {
                    if events
                        .send(Event::ClientMessage { client, message })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => log::warn!("web client {client} sent invalid JSON: {error}"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(error) => {
                log::debug!("reading from web client {client}: {error}");
                break;
            }
        }
    }
    events.send(Event::ClientDisconnected { client }).await.ok();
    writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::tungstenite::client::IntoClientRequest as _;
    use pretty_assertions::assert_eq;

    struct TestGateway {
        address: SocketAddr,
        token: String,
        events: Receiver<Event>,
        _directory: tempfile::TempDir,
        _task: smol::Task<Result<()>>,
    }

    fn start(allowed_origins: Vec<String>) -> TestGateway {
        let directory = tempfile::tempdir().expect("creating a temporary directory");
        let token_path = directory.path().join("token");
        let listener = smol::block_on(TcpListener::bind("127.0.0.1:0")).expect("binding");
        let address = listener.local_addr().expect("local address");
        let (events, events_receiver) = smol::channel::unbounded();
        let config = GatewayConfig {
            listen: address.to_string(),
            token_file: Some(token_path.to_string_lossy().into_owned()),
            cookie_secure: true,
            allowed_origins,
        };
        let task = smol::spawn(run(
            listener,
            config,
            events,
            ClientIds::default(),
            "test".into(),
        ));
        let token = loop {
            if let Ok(token) = std::fs::read_to_string(&token_path) {
                break token.trim().to_string();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        TestGateway {
            address,
            token,
            events: events_receiver,
            _directory: directory,
            _task: task,
        }
    }

    async fn http(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.expect("connecting");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("writing the request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("reading the response");
        response
    }

    /// Reads only the status line, for upgraded connections that stay open.
    async fn status_line(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.expect("connecting");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("writing the request");
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            if stream.read(&mut byte).await.expect("reading") == 0 {
                break;
            }
            line.push(byte[0]);
        }
        String::from_utf8_lossy(&line).trim().to_string()
    }

    fn login_request(token: &str) -> String {
        let body = json!({"token": token}).to_string();
        format!(
            "POST /api/v1/login HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn serves_the_web_client_and_requires_a_login() {
        let gateway = start(Vec::new());
        smol::block_on(async {
            let index = http(gateway.address, "GET / HTTP/1.1\r\nHost: test\r\n\r\n").await;
            assert!(index.starts_with("HTTP/1.1 200 OK"));
            assert!(index.contains("Content-Security-Policy"));

            let health = http(gateway.address, "GET /api/v1/health HTTP/1.1\r\n\r\n").await;
            assert!(health.contains("\"authenticated\":false"));

            let rejected = http(gateway.address, &login_request("wrong-token-wrong-token")).await;
            assert!(rejected.starts_with("HTTP/1.1 401"));

            let accepted = http(gateway.address, &login_request(&gateway.token)).await;
            assert!(accepted.starts_with("HTTP/1.1 200 OK"));
            let cookie = accepted
                .lines()
                .find_map(|line| line.strip_prefix("Set-Cookie: "))
                .expect("a login cookie");
            assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
            let login = cookie
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
                .map(|(_, value)| value.to_string())
                .expect("a login value");

            let health = http(
                gateway.address,
                &format!("GET /api/v1/health HTTP/1.1\r\nCookie: {SESSION_COOKIE}={login}\r\n\r\n"),
            )
            .await;
            assert!(health.contains("\"authenticated\":true"));
        });
    }

    #[test]
    fn limits_failed_logins() {
        let gateway = start(Vec::new());
        smol::block_on(async {
            for _ in 0..MAX_FAILURES_PER_WINDOW {
                http(gateway.address, &login_request("wrong-token-wrong-token")).await;
            }
            let limited = http(gateway.address, &login_request(&gateway.token)).await;
            assert!(limited.starts_with("HTTP/1.1 429"));
        });
    }

    #[test]
    fn bridges_websocket_messages_to_the_router() {
        let gateway = start(Vec::new());
        smol::block_on(async {
            let stream = TcpStream::connect(gateway.address)
                .await
                .expect("connecting");
            let mut request = format!("ws://{}/api/v1/acp", gateway.address)
                .into_client_request()
                .expect("a WebSocket request");
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {}", gateway.token)
                    .parse()
                    .expect("a header value"),
            );
            let (mut socket, _) = async_tungstenite::client_async(request, stream)
                .await
                .expect("the WebSocket handshake");

            let Ok(Event::Observe {
                client, outgoing, ..
            }) = gateway.events.recv().await
            else {
                panic!("expected an observer");
            };
            socket
                .send(Message::text(
                    r#"{"jsonrpc":"2.0","id":1,"method":"session/list"}"#,
                ))
                .await
                .expect("sending");
            let Ok(Event::ClientMessage {
                client: sender,
                message,
            }) = gateway.events.recv().await
            else {
                panic!("expected a client message");
            };
            assert_eq!(sender, client);
            assert_eq!(message["method"], "session/list");

            outgoing
                .send(json!({"jsonrpc": "2.0", "id": 1, "result": {"sessions": []}}))
                .await
                .expect("sending to the client");
            let reply = socket.next().await.expect("a reply").expect("a frame");
            assert_eq!(
                serde_json::from_str::<Value>(reply.to_text().expect("text")).expect("JSON"),
                json!({"jsonrpc": "2.0", "id": 1, "result": {"sessions": []}})
            );

            socket.close(None).await.ok();
            loop {
                match gateway.events.recv().await {
                    Ok(Event::ClientDisconnected { client: closed }) => {
                        assert_eq!(closed, client);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => panic!("the gateway stopped"),
                }
            }
        });
    }

    #[test]
    fn rejects_websockets_without_a_login_or_from_other_origins() {
        let gateway = start(vec!["https://agent.example.com".into()]);
        smol::block_on(async {
            let handshake = |extra: String| {
                format!(
                    "GET /api/v1/acp HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n{extra}\r\n"
                )
            };
            let bearer = format!("Authorization: Bearer {}\r\n", gateway.token);

            let anonymous = http(gateway.address, &handshake(String::new())).await;
            assert!(anonymous.starts_with("HTTP/1.1 401"));

            let foreign = http(
                gateway.address,
                &handshake(format!("{bearer}Origin: https://evil.example.com\r\n")),
            )
            .await;
            assert!(foreign.starts_with("HTTP/1.1 403"));

            for origin in ["https://agent.example.com", "http://127.0.0.1", ""] {
                let origin_header = if origin.is_empty() {
                    String::new()
                } else {
                    format!("Origin: {origin}\r\n")
                };
                assert_eq!(
                    status_line(
                        gateway.address,
                        &handshake(format!("{bearer}{origin_header}"))
                    )
                    .await,
                    "HTTP/1.1 101 Switching Protocols",
                    "origin {origin:?}"
                );
            }
        });
    }

    #[test]
    fn compares_tokens_without_leaking_their_length_early() {
        assert!(constant_time_eq("abcdef", "abcdef"));
        assert!(!constant_time_eq("abcdef", "abcdeg"));
        assert!(!constant_time_eq("abc", "abcdef"));
    }
}
