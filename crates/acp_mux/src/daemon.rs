use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _, channel::oneshot};
use serde_json::Value;
use smol::{
    channel::Sender,
    net::unix::{UnixListener, UnixStream},
};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{self, Write as _},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt as _, net::UnixStream as StdUnixStream},
    },
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    Config, PROTOCOL_VERSION,
    agent::{ProcessLauncher, write_messages},
    control::{ControlRequest, Handshake, HandshakeReply, HandshakeRequest, MIN_PROTOCOL_VERSION},
    gateway, notify, pid_path,
    router::{ClientId, Event, Flow, Router},
    socket_path, state_dir,
};

/// Hands out client ids shared by unix socket and gateway connections.
#[derive(Clone, Default)]
pub struct ClientIds(Arc<AtomicU64>);

impl ClientIds {
    pub fn next(&self) -> ClientId {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(version: &str) -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init()
        .ok();

    let state_dir = state_dir();
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    std::fs::set_permissions(&state_dir, Permissions::from_mode(0o700))?;

    let Some(_lock) = acquire_lock()? else {
        log::info!("another acp-mux daemon is already running");
        return Ok(());
    };

    let socket = socket_path();
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing {}", socket.display())),
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .with_context(|| format!("binding {}", socket.display()))?;
    std::fs::set_permissions(&socket, Permissions::from_mode(0o600))?;

    let config = match Config::load() {
        Ok(config) => config.unwrap_or_default(),
        Err(error) => {
            log::error!("ignoring invalid acp-mux config: {error:#}");
            Config::default()
        }
    };
    log::info!(
        "acp-mux daemon {version} (protocol {PROTOCOL_VERSION}) listening on {}",
        socket.display()
    );

    let result = smol::block_on(serve(listener, config, version.to_string()));
    std::fs::remove_file(&socket).ok();
    log::info!("acp-mux daemon exiting");
    result
}

/// Takes the lock that makes this the only daemon. Returns `None` when a
/// healthy daemon holds it, and waits briefly for one that is shutting down.
fn acquire_lock() -> Result<Option<File>> {
    let path = pid_path();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        // SAFETY: flock only operates on the file descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error).with_context(|| format!("locking {}", path.display()));
        }
        if StdUnixStream::connect(socket_path()).is_ok() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for the previous acp-mux daemon to exit"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(Some(file))
}

async fn serve(
    listener: std::os::unix::net::UnixListener,
    config: Config,
    version: String,
) -> Result<()> {
    let listener = UnixListener::try_from(listener)?;
    let (events, events_rx) = smol::channel::unbounded();
    let client_ids = ClientIds::default();

    let notices = match &config.notify {
        Some(notify_config) => match notify::ntfy(notify_config) {
            Ok(send) => {
                let (notices, receiver) = smol::channel::unbounded();
                let delay = Duration::from_secs(notify_config.permission_delay_seconds);
                smol::spawn(notify::run(receiver, delay, send)).detach();
                log::info!("publishing notifications to {}", notify_config.ntfy_url);
                Some(notices)
            }
            Err(error) => {
                log::error!("notifications are disabled: {error:#}");
                None
            }
        },
        None => None,
    };
    let _gateway_task = config.gateway.clone().map(|gateway_config| {
        let events = events.clone();
        let client_ids = client_ids.clone();
        let version = version.clone();
        smol::spawn(async move {
            if let Err(error) = gateway::serve(gateway_config, events, client_ids, version).await {
                log::error!("the web gateway stopped: {error:#}");
            }
        })
    });

    let mut router = Router::new(
        version.clone(),
        config,
        Box::new(ProcessLauncher),
        events.clone(),
        notices,
    );
    let _accept_task = smol::spawn(accept_loop(listener, events, client_ids, version));
    while let Ok(event) = events_rx.recv().await {
        if router.handle(event) == Flow::Exit {
            break;
        }
    }
    Ok(())
}

async fn accept_loop(
    listener: UnixListener,
    events: Sender<Event>,
    client_ids: ClientIds,
    version: String,
) {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                log::error!("accepting a connection failed: {error}");
                continue;
            }
        };
        match peer_uid(&stream) {
            Ok(peer) if peer == uid => {}
            Ok(peer) => {
                log::warn!("rejected a connection from uid {peer}");
                continue;
            }
            Err(error) => {
                log::warn!("rejected a connection with unknown credentials: {error}");
                continue;
            }
        }
        let client = client_ids.next();
        smol::spawn({
            let events = events.clone();
            let version = version.clone();
            async move {
                if let Err(error) = serve_connection(stream, client, events, version).await {
                    log::warn!("connection {client}: {error:#}");
                }
            }
        })
        .detach();
    }
}

async fn write_line(writer: &mut UnixStream, value: &impl serde::Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

async fn serve_connection(
    stream: UnixStream,
    client: ClientId,
    events: Sender<Event>,
    version: String,
) -> Result<()> {
    let mut reader = futures::io::BufReader::new(stream.clone());
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let refuse = |error: String| HandshakeReply {
        ok: false,
        daemon_version: Some(version.clone()),
        protocol: Some(PROTOCOL_VERSION),
        error: Some(error),
    };
    let handshake: Value = serde_json::from_str(&line).context("parsing the handshake")?;
    let protocol = handshake
        .get("acp_mux")
        .and_then(Value::as_u64)
        .and_then(|protocol| u32::try_from(protocol).ok())
        .filter(|protocol| (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(protocol));
    let Some(protocol) = protocol else {
        let error = format!(
            "client protocol {:?} is not supported by acp-mux daemon {version} \
             (protocols {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}); run `acp-mux restart`",
            handshake.get("acp_mux")
        );
        write_line(&mut writer, &refuse(error)).await?;
        return Ok(());
    };
    let handshake: Handshake = match serde_json::from_value(handshake) {
        Ok(handshake) => handshake,
        Err(error) => {
            write_line(&mut writer, &refuse(format!("invalid handshake: {error}"))).await?;
            return Ok(());
        }
    };
    // Older clients check that the daemon answers with their own version.
    let accepted = HandshakeReply {
        ok: true,
        daemon_version: Some(version.clone()),
        protocol: Some(protocol),
        error: None,
    };

    match handshake.request {
        HandshakeRequest::Attach { agent, command } => {
            let (outgoing, outgoing_rx) = smol::channel::unbounded();
            let (reply, reply_rx) = oneshot::channel();
            events
                .send(Event::Attach {
                    client,
                    agent_name: agent,
                    spec: command,
                    outgoing,
                    reply,
                })
                .await?;
            if let Err(error) = reply_rx.await? {
                write_line(&mut writer, &refuse(error)).await?;
                return Ok(());
            }
            write_line(&mut writer, &accepted).await?;
            pump_client(reader, writer, client, &events, outgoing_rx).await?;
        }
        HandshakeRequest::Observe { observe } => {
            if protocol < 2 {
                write_line(
                    &mut writer,
                    &refuse("observing requires protocol 2".to_string()),
                )
                .await?;
                return Ok(());
            }
            let (outgoing, outgoing_rx) = smol::channel::unbounded();
            events
                .send(Event::Observe {
                    client,
                    label: observe
                        .label
                        .unwrap_or_else(|| "local observer".to_string()),
                    outgoing,
                })
                .await?;
            write_line(&mut writer, &accepted).await?;
            pump_client(reader, writer, client, &events, outgoing_rx).await?;
        }
        HandshakeRequest::Control { control } => {
            let is_shutdown = control == ControlRequest::Shutdown;
            let (reply, reply_rx) = oneshot::channel();
            events
                .send(Event::Control {
                    request: control,
                    reply,
                })
                .await?;
            let response = reply_rx.await?;
            let written = async {
                write_line(&mut writer, &accepted).await?;
                write_line(&mut writer, &response).await
            }
            .await;
            if is_shutdown {
                events.send(Event::Exit).await.ok();
            }
            written?;
        }
    }
    Ok(())
}

/// Passes the client's lines to the router and the router's messages back
/// until the client disconnects.
async fn pump_client(
    reader: futures::io::BufReader<UnixStream>,
    writer: UnixStream,
    client: ClientId,
    events: &Sender<Event>,
    outgoing: smol::channel::Receiver<Value>,
) -> Result<()> {
    let writer_task = smol::spawn(write_messages(writer, outgoing));
    let mut lines = reader.lines();
    while let Some(line) = lines.next().await {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                log::info!("reading from client {client}: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(message) => {
                events
                    .send(Event::ClientMessage { client, message })
                    .await?
            }
            Err(error) => log::warn!("client {client} wrote invalid JSON: {error}"),
        }
    }
    events.send(Event::ClientDisconnected { client }).await.ok();
    writer_task.await;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &impl AsRawFd) -> io::Result<libc::uid_t> {
    // SAFETY: ucred is plain data, and getsockopt writes at most `length` bytes into it.
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(stream: &impl AsRawFd) -> io::Result<libc::uid_t> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: getpeereid only writes the two out parameters.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{AgentSpec, handshake};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::io::{BufRead as _, BufReader};

    /// Answers by position: the daemon numbers its requests to the agent 1, 2, 3...
    const FAKE_AGENT: &str = r#"
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"yes","name":"Yes","kind":"allow_once"}]}}'
read -r line
case "$line" in
  *'"optionId":"yes"'*) text="approved" ;;
  *) text="unexpected" ;;
esac
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\" $text\"}}}}"
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
read -r line
"#;

    struct TestClient {
        stream: StdUnixStream,
        reader: BufReader<StdUnixStream>,
    }

    fn fake_agent(script: &str) -> HandshakeRequest {
        HandshakeRequest::Attach {
            agent: "fake".into(),
            command: AgentSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env: [("PATH".to_string(), "/usr/bin:/bin".to_string())].into(),
                cwd: None,
                key_env: Vec::new(),
            },
        }
    }

    fn start_daemon() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("creating a temporary directory");
        let socket = directory.path().join("daemon.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("binding the test socket");
        std::thread::spawn(move || {
            smol::block_on(serve(listener, Config::default(), "test".into())).ok();
        });
        (directory, socket)
    }

    impl TestClient {
        fn connect(socket: &std::path::Path) -> Self {
            Self::connect_with(socket, fake_agent(FAKE_AGENT))
        }

        fn connect_with(socket: &std::path::Path, request: HandshakeRequest) -> Self {
            let stream = StdUnixStream::connect(socket).expect("connecting to the daemon");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("setting a read timeout");
            let reader = handshake(&stream, request).expect("handshake");
            Self { stream, reader }
        }

        fn send(&mut self, message: Value) {
            crate::control::write_line(&mut self.stream, &message).expect("writing to the daemon");
        }

        fn receive(&mut self) -> Value {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .expect("reading from the daemon");
            serde_json::from_str(&line).expect("parsing a message from the daemon")
        }
    }

    #[test]
    fn detached_turn_continues_and_is_replayed_on_attach() {
        let directory = tempfile::tempdir().expect("creating a temporary directory");
        let socket = directory.path().join("daemon.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("binding the test socket");
        std::thread::spawn(move || {
            smol::block_on(serve(listener, Config::default(), "test".into())).ok();
        });

        let mut first = TestClient::connect(&socket);
        first.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1}}));
        assert_eq!(
            first.receive()["result"]["_meta"]["zed.dev/acpMux"]["protocol"],
            PROTOCOL_VERSION
        );
        first.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}));
        assert_eq!(first.receive()["result"]["sessionId"], "s1");
        first.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {
                "sessionId": "s1", "prompt": [{"type": "text", "text": "go"}],
            }}),
        );
        assert_eq!(
            first.receive()["params"]["update"]["content"]["text"],
            "working"
        );
        assert_eq!(first.receive()["method"], "session/request_permission");
        drop(first);

        let mut second = TestClient::connect(&socket);
        second.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1}}));
        assert_eq!(
            second.receive()["result"]["agentCapabilities"]["loadSession"],
            true
        );
        second.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/load", "params": {
                "sessionId": "s1", "cwd": "/", "mcpServers": [],
            }}),
        );
        let replayed = (0..5).map(|_| second.receive()).collect::<Vec<_>>();
        assert_eq!(replayed[0]["params"]["update"]["content"]["text"], "go");
        assert_eq!(
            replayed[1]["params"]["update"]["content"]["text"],
            "working"
        );
        assert_eq!(
            replayed[2],
            json!({"jsonrpc": "2.0", "method": crate::TURN_STATE_METHOD, "params": {"sessionId": "s1", "state": "running"}})
        );
        assert_eq!(
            replayed[3],
            json!({"jsonrpc": "2.0", "id": 1, "result": {}})
        );
        assert_eq!(replayed[4]["method"], "session/request_permission");

        let permission_id = replayed[4]["id"].clone();
        second.send(json!({"jsonrpc": "2.0", "id": permission_id, "result": {
            "outcome": {"outcome": "selected", "optionId": "yes"},
        }}));
        assert_eq!(
            second.receive()["params"]["update"]["content"]["text"],
            " approved"
        );
        assert_eq!(
            second.receive(),
            json!({"jsonrpc": "2.0", "method": crate::TURN_STATE_METHOD, "params": {
                "sessionId": "s1", "state": "idle", "stopReason": "end_turn",
            }})
        );
    }

    #[test]
    fn accepts_attach_handshakes_from_older_clients() {
        let (_directory, socket) = start_daemon();
        let mut stream = StdUnixStream::connect(&socket).expect("connecting to the daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("setting a read timeout");
        let mut handshake = serde_json::to_value(Handshake {
            acp_mux: PROTOCOL_VERSION,
            request: fake_agent(FAKE_AGENT),
        })
        .expect("serializing the handshake");
        handshake["acp_mux"] = json!(1);
        crate::control::write_line(&mut stream, &handshake).expect("writing the handshake");
        let mut reader = BufReader::new(stream.try_clone().expect("cloning the stream"));
        let reply: HandshakeReply =
            serde_json::from_str(&crate::control::read_line(&mut reader).expect("reading"))
                .expect("parsing the reply");
        assert!(reply.ok);
        assert_eq!(reply.protocol, Some(1));
    }

    #[test]
    fn observers_share_sessions_with_attached_clients() {
        const AGENT: &str = r#"
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
read -r line
"#;
        let (_directory, socket) = start_daemon();
        let mut zed = TestClient::connect_with(&socket, fake_agent(AGENT));
        zed.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1}}));
        zed.receive();
        zed.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}));
        assert_eq!(zed.receive()["result"]["sessionId"], "s1");

        let mut phone = TestClient::connect_with(
            &socket,
            HandshakeRequest::Observe {
                observe: crate::control::ObserveRequest {
                    label: Some("phone".into()),
                },
            },
        );
        phone.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
        assert_eq!(
            phone.receive()["result"]["_meta"]["zed.dev/acpMux"]["observer"],
            true
        );
        phone.send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}));
        let list = phone.receive();
        assert_eq!(list["result"]["sessions"][0]["sessionId"], "s1");
        assert_eq!(
            list["result"]["sessions"][0]["_meta"]["zed.dev/acpMux"]["agent"],
            "fake"
        );
        phone.send(json!({"jsonrpc": "2.0", "id": 3, "method": "session/load", "params": {"sessionId": "s1", "cwd": "/", "mcpServers": []}}));
        assert_eq!(
            phone.receive(),
            json!({"jsonrpc": "2.0", "id": 3, "result": {}})
        );

        phone.send(
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {
                "sessionId": "s1", "prompt": [{"type": "text", "text": "from the phone"}],
            }}),
        );
        assert_eq!(
            zed.receive()["params"]["update"]["content"]["text"],
            "from the phone"
        );
        assert_eq!(zed.receive()["params"]["state"], "running");
        assert_eq!(
            zed.receive()["params"]["update"]["content"]["text"],
            "hello"
        );
        assert_eq!(
            zed.receive(),
            json!({"jsonrpc": "2.0", "method": crate::TURN_STATE_METHOD, "params": {
                "sessionId": "s1", "state": "idle", "stopReason": "end_turn",
            }})
        );
        assert_eq!(
            phone.receive()["params"]["update"]["content"]["text"],
            "hello"
        );
        assert_eq!(
            phone.receive(),
            json!({"jsonrpc": "2.0", "id": 4, "result": {"stopReason": "end_turn"}})
        );
    }
}
