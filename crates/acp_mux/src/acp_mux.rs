//! Keeps external ACP agents running on the remote host independently of the
//! Zed connection that started them, so clients can detach and later re-attach
//! to the same sessions (similar to tmux).
#![cfg(unix)]

mod agent;
mod attach;
mod control;
mod daemon;
mod gateway;
mod notify;
mod router;
mod session;
mod snapshot;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use control::PROTOCOL_VERSION;

/// Method of the extension notification that tells an attached client whether a
/// turn it did not start itself is running.
pub const TURN_STATE_METHOD: &str = "_zed.dev/acpMux/turn_state";
/// Method of the extension request that names a session. ACP has no way to tell
/// an agent a session's name, so the daemon keeps it and shares it with clients.
pub const SET_TITLE_METHOD: &str = "_zed.dev/acpMux/set_title";
/// Method of the extension request that returns earlier entries of a session a
/// client observes, for clients that loaded only its latest entries by passing
/// `_meta["zed.dev/acpMux"].history.tail` to `session/load`.
pub const HISTORY_METHOD: &str = "_zed.dev/acpMux/history";
/// Key in the `_meta` of the `initialize` response that marks a muxed agent.
pub const META_KEY: &str = "zed.dev/acpMux";

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Connect stdio to an agent hosted by the acp-mux daemon, starting the daemon if needed.
    Attach {
        /// Name of the agent in Zed.
        #[arg(long)]
        agent: String,
        /// Environment variables whose values distinguish one agent configuration from another.
        #[arg(long = "key-env")]
        key_env: Vec<String>,
        /// The agent command and its arguments.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run the daemon that owns the agent processes and their sessions.
    Daemon,
    /// List agents and sessions hosted by the daemon.
    List,
    /// End a session, or every process of an agent.
    Kill {
        session_id: Option<String>,
        #[arg(
            long,
            conflicts_with = "session_id",
            required_unless_present = "session_id"
        )]
        agent: Option<String>,
    },
    /// Restart the daemon, interrupting every running turn.
    Restart,
}

pub fn run(command: Command, version: &str) -> Result<()> {
    match command {
        Command::Attach {
            agent,
            key_env,
            command,
        } => attach::run(agent, key_env, command),
        Command::Daemon => daemon::run(version),
        Command::List => control::list(),
        Command::Kill { session_id, agent } => {
            let target = match (session_id, agent) {
                (Some(session_id), _) => control::KillTarget::Session(session_id),
                (None, Some(agent)) => control::KillTarget::Agent(agent),
                (None, None) => bail!("either a session id or --agent is required"),
            };
            control::kill(target)
        }
        Command::Restart => control::restart(),
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    /// Agents routed through the daemon. All external agents when absent.
    pub agents: Option<Vec<String>>,
    /// Tool kinds (such as `read`) whose permission requests the daemon approves itself.
    pub auto_approve: Vec<String>,
    /// Serves the web client and its WebSocket API when present.
    pub gateway: Option<GatewayConfig>,
    /// Publishes notifications when present.
    pub notify: Option<NotifyConfig>,
    /// Whether sessions may be titled by the agent. When disabled, sessions are
    /// only named by users, through `_zed.dev/acpMux/set_title`, and titles the
    /// agent reports are dropped.
    pub auto_generate_thread_title: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct GatewayConfig {
    /// Address to listen on, such as `127.0.0.1:8765`. TLS is expected to be
    /// provided in front of it, by a VPN or a reverse proxy such as frp.
    pub listen: String,
    /// File holding the login token. Created with a random token when missing.
    #[serde(default)]
    pub token_file: Option<String>,
    /// Marks the login cookie `Secure`. Only disable it when the web client is
    /// reached over plain HTTP inside a VPN, since browsers drop `Secure`
    /// cookies set over HTTP.
    #[serde(default = "default_true")]
    pub cookie_secure: bool,
    /// Extra origins (such as `https://agent.example.com`) allowed to open the
    /// WebSocket, for proxies that rewrite the `Host` header.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct NotifyConfig {
    /// ntfy topic URL, such as `https://ntfy.sh/my-topic`.
    pub ntfy_url: String,
    /// File holding an ntfy access token, for protected topics.
    #[serde(default)]
    pub ntfy_token_file: Option<String>,
    /// How long a permission request may stay unanswered before notifying.
    #[serde(default = "default_permission_delay_seconds")]
    pub permission_delay_seconds: u64,
}

fn default_true() -> bool {
    true
}

fn default_permission_delay_seconds() -> u64 {
    30
}

impl Config {
    pub fn load() -> Result<Option<Self>> {
        let path = config_path();
        let contents = match std::fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let config = serde_json::from_slice(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(config))
    }

    pub fn wraps_agent(&self, name: &str) -> bool {
        self.enabled
            && self
                .agents
                .as_ref()
                .is_none_or(|agents| agents.iter().any(|agent| agent == name))
    }
}

/// Rewrites an agent command so that it runs through `acp-mux attach` using the
/// current executable, returning the new program and arguments.
pub fn wrap_command(
    agent_name: &str,
    program: &Path,
    args: &[String],
    key_env: impl IntoIterator<Item = String>,
) -> Result<(PathBuf, Vec<String>)> {
    let executable = std::env::current_exe().context("resolving the remote server executable")?;
    wrap_command_with_executable(executable, agent_name, program, args, key_env)
}

pub fn wrap_local_command(
    agent_name: &str,
    program: &Path,
    args: &[String],
    key_env: impl IntoIterator<Item = String>,
) -> Result<Option<(PathBuf, Vec<String>)>> {
    let Some(config) = Config::load()? else {
        return Ok(None);
    };
    if !config.wraps_agent(agent_name) {
        return Ok(None);
    }
    let executable = std::env::current_exe().context("resolving the Zed executable")?;
    let directory = executable
        .parent()
        .context("Zed executable has no parent")?;
    let helper = directory.join("zed-acp-mux");
    anyhow::ensure!(
        helper.is_file(),
        "acp-mux is enabled but {} is missing; build or install zed-acp-mux alongside Zed",
        helper.display()
    );
    // Attach records its actual cwd in PWD, so local projects never reuse a
    // process started with another project's working directory.
    let (helper, mut args) = wrap_command_with_executable(
        helper,
        agent_name,
        program,
        args,
        key_env.into_iter().chain(["PWD".to_string()]),
    )?;
    if let Some(directory) = paths::custom_data_dir() {
        args.splice(
            0..0,
            [
                "--user-data-dir".to_string(),
                directory.to_string_lossy().into_owned(),
            ],
        );
    }
    Ok(Some((helper, args)))
}

fn wrap_command_with_executable(
    executable: PathBuf,
    agent_name: &str,
    program: &Path,
    args: &[String],
    key_env: impl IntoIterator<Item = String>,
) -> Result<(PathBuf, Vec<String>)> {
    let mut key_env = key_env.into_iter().collect::<Vec<_>>();
    key_env.sort();
    key_env.dedup();
    let mut wrapped = vec![
        "acp-mux".to_string(),
        "attach".to_string(),
        "--agent".to_string(),
        agent_name.to_string(),
    ];
    for key in key_env {
        wrapped.push("--key-env".to_string());
        wrapped.push(key);
    }
    wrapped.push("--".to_string());
    wrapped.push(program.to_string_lossy().into_owned());
    wrapped.extend(args.iter().cloned());
    Ok((executable, wrapped))
}

fn state_dir() -> PathBuf {
    paths::remote_server_state_dir().join("acp-mux")
}

fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
}

fn pid_path() -> PathBuf {
    state_dir().join("daemon.pid")
}

fn log_path() -> PathBuf {
    paths::custom_data_dir()
        .map(|directory| directory.join("logs"))
        .unwrap_or_else(|| paths::logs_dir().clone())
        .join("acp-mux.log")
}

fn config_path() -> PathBuf {
    paths::config_dir().join("acp_mux.json")
}

fn default_token_path() -> PathBuf {
    paths::config_dir().join("acp_mux_token")
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => paths::home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory as _, Parser};
    use pretty_assertions::assert_eq;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn parses_wrapped_commands() {
        Cli::command().debug_assert();
        let (program, args) = wrap_command(
            "claude-acp",
            Path::new("/usr/bin/node"),
            &["agent.js".to_string(), "--acp".to_string()],
            ["B_KEY".to_string(), "A_KEY".to_string()],
        )
        .expect("wrapping the command");
        assert_eq!(
            args,
            [
                "acp-mux",
                "attach",
                "--agent",
                "claude-acp",
                "--key-env",
                "A_KEY",
                "--key-env",
                "B_KEY",
                "--",
                "/usr/bin/node",
                "agent.js",
                "--acp",
            ]
        );

        let cli = Cli::try_parse_from(
            std::iter::once(program.to_string_lossy().into_owned()).chain(args.into_iter().skip(1)),
        )
        .expect("parsing the wrapped command");
        let Command::Attach {
            agent,
            key_env,
            command,
        } = cli.command
        else {
            panic!("expected attach");
        };
        assert_eq!(agent, "claude-acp");
        assert_eq!(key_env, ["A_KEY", "B_KEY"]);
        assert_eq!(command, ["/usr/bin/node", "agent.js", "--acp"]);
    }

    #[test]
    fn kill_requires_a_target() {
        assert!(Cli::try_parse_from(["acp-mux", "kill"]).is_err());
        assert!(Cli::try_parse_from(["acp-mux", "kill", "session-1"]).is_ok());
        assert!(Cli::try_parse_from(["acp-mux", "kill", "--agent", "codex"]).is_ok());
    }

    #[test]
    fn wraps_only_configured_agents() {
        let all = Config {
            enabled: true,
            ..Config::default()
        };
        assert!(all.wraps_agent("anything"));
        let some = Config {
            agents: Some(vec!["codex-acp".to_string()]),
            ..all.clone()
        };
        assert!(some.wraps_agent("codex-acp"));
        assert!(!some.wraps_agent("claude-acp"));
        let disabled = Config {
            enabled: false,
            ..all
        };
        assert!(!disabled.wraps_agent("codex-acp"));
    }
}
