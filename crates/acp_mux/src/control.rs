use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    io::{BufReader, Write as _},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use crate::{attach, pid_path, socket_path};

/// Version of the line protocol between `attach`/control clients and the daemon.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Handshake {
    pub acp_mux: u32,
    #[serde(flatten)]
    pub request: HandshakeRequest,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum HandshakeRequest {
    Attach { agent: String, command: AgentSpec },
    Control { control: ControlRequest },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct AgentSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub key_env: Vec<String>,
}

impl AgentSpec {
    /// Identifies agent processes that can be shared. The full environment is
    /// excluded because it contains per-connection values such as `SSH_CLIENT`.
    pub fn command_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.program.hash(&mut hasher);
        self.args.hash(&mut hasher);
        let mut keys = self.key_env.clone();
        keys.sort();
        keys.dedup();
        for key in keys {
            key.hash(&mut hasher);
            self.env.get(&key).hash(&mut hasher);
        }
        hasher.finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    List,
    Kill(KillTarget),
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum KillTarget {
    Session(String),
    Agent(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HandshakeReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ListResponse {
    pub agents: Vec<AgentSummary>,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentSummary {
    pub name: String,
    pub pid: Option<u32>,
    pub program: String,
    pub clients: usize,
    pub sessions: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub agent: String,
    pub cwd: String,
    pub title: Option<String>,
    pub state: SessionState,
    pub attached: bool,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    WaitingForPermission,
    Dead,
}

impl SessionState {
    fn label(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Running => "running",
            SessionState::WaitingForPermission => "waiting",
            SessionState::Dead => "dead",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct KillResponse {
    pub killed: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ShutdownResponse {
    /// Sessions whose running turn is interrupted by the shutdown.
    pub interrupted: Vec<String>,
}

pub fn write_line(writer: &mut impl std::io::Write, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

pub fn read_line(reader: &mut impl std::io::BufRead) -> Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        bail!("the acp-mux daemon closed the connection");
    }
    Ok(line)
}

/// Sends a handshake and checks the daemon's reply, returning a reader that
/// keeps any bytes the daemon sent after the reply.
pub fn handshake(stream: &UnixStream, request: HandshakeRequest) -> Result<BufReader<UnixStream>> {
    let mut writer = stream.try_clone()?;
    write_line(
        &mut writer,
        &Handshake {
            acp_mux: PROTOCOL_VERSION,
            request,
        },
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let reply: HandshakeReply =
        serde_json::from_str(&read_line(&mut reader)?).context("parsing the daemon handshake")?;
    if !reply.ok {
        bail!(
            "acp-mux daemon refused the connection: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        );
    }
    if reply.protocol != Some(PROTOCOL_VERSION) {
        bail!(
            "acp-mux daemon (version {}) speaks protocol {:?}, expected {PROTOCOL_VERSION}; run `acp-mux restart`",
            reply.daemon_version.as_deref().unwrap_or("unknown"),
            reply.protocol,
        );
    }
    Ok(reader)
}

fn request(control: ControlRequest) -> Result<serde_json::Value> {
    let stream = UnixStream::connect(socket_path()).context("the acp-mux daemon is not running")?;
    let mut reader = handshake(&stream, HandshakeRequest::Control { control })?;
    let line = read_line(&mut reader)?;
    let response: serde_json::Value = serde_json::from_str(&line)?;
    if let Some(error) = response.get("error").and_then(|error| error.as_str()) {
        return Err(anyhow!("{error}"));
    }
    Ok(response)
}

pub fn list() -> Result<()> {
    let response: ListResponse = serde_json::from_value(request(ControlRequest::List)?)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "AGENTS")?;
    if response.agents.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for agent in &response.agents {
        writeln!(
            stdout,
            "  {:<20} pid {:<8} clients {:<3} sessions {:<3} {}",
            agent.name,
            agent
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            agent.clients,
            agent.sessions,
            agent.program,
        )?;
    }
    writeln!(stdout, "SESSIONS")?;
    if response.sessions.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for session in &response.sessions {
        writeln!(
            stdout,
            "  {:<38} {:<16} {:<8} {:<9} {} {} {}",
            session.session_id,
            session.agent,
            session.state.label(),
            if session.attached {
                "attached"
            } else {
                "detached"
            },
            session.updated_at,
            session.cwd,
            session.title.as_deref().unwrap_or(""),
        )?;
    }
    Ok(())
}

pub fn kill(target: KillTarget) -> Result<()> {
    let response: KillResponse =
        serde_json::from_value(request(ControlRequest::Kill(target.clone()))?)?;
    if response.killed == 0 {
        bail!("nothing matched {target:?}");
    }
    println!("killed {}", response.killed);
    Ok(())
}

pub fn restart() -> Result<()> {
    match request(ControlRequest::Shutdown) {
        Ok(response) => {
            let response: ShutdownResponse = serde_json::from_value(response)?;
            for session_id in &response.interrupted {
                println!("interrupted running session {session_id}");
            }
        }
        Err(error) => {
            // An incompatible daemon cannot be asked to shut down over the socket.
            log::info!("asking the daemon to shut down failed: {error:#}");
            terminate_daemon_from_pid_file()?;
        }
    }
    wait_for_daemon_exit()?;
    let stream = attach::connect_or_spawn_daemon()?;
    drop(stream);
    println!("acp-mux daemon restarted");
    Ok(())
}

fn terminate_daemon_from_pid_file() -> Result<()> {
    let contents = match std::fs::read_to_string(pid_path()) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let pid: libc::pid_t = contents
        .trim()
        .parse()
        .context("parsing the daemon pid file")?;
    // SAFETY: sending a signal has no memory-safety requirements.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("signalling the acp-mux daemon");
        }
    }
    Ok(())
}

fn wait_for_daemon_exit() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(socket_path()).is_ok() {
        if Instant::now() > deadline {
            bail!("timed out waiting for the acp-mux daemon to exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
