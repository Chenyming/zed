#![cfg(unix)]

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead as _, BufReader, Write as _},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

const HELPER: &str = env!("CARGO_BIN_EXE_zed-acp-mux");
const FAKE_AGENT: &str = r#"
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"local-session"}}'
read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":"permission","method":"session/request_permission","params":{"sessionId":"local-session","toolCall":{"toolCallId":"tool"},"options":[{"optionId":"yes","name":"Yes","kind":"allow_once"}]}}'
read -r line
case "$line" in
  *'"optionId":"yes"'*) text=approved ;;
  *) text=unexpected ;;
esac
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"local-session\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"$text\"}}}}"
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
read -r line
"#;

struct LocalDaemon(tempfile::TempDir);

impl LocalDaemon {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join("config"))?;
        std::fs::write(
            directory.path().join("config/acp_mux.json"),
            r#"{"enabled":true}"#,
        )?;
        Ok(Self(directory))
    }

    fn control(&self, control: Value) -> Result<Value> {
        let mut stream =
            UnixStream::connect(self.0.path().join("server_state/acp-mux/daemon.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        writeln!(stream, "{}", json!({"acp_mux":2,"control":control}))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let handshake: Value = serde_json::from_str(&line)?;
        anyhow::ensure!(handshake["ok"] == true, "{handshake}");
        line.clear();
        reader.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }
}

impl Drop for LocalDaemon {
    fn drop(&mut self) {
        if let Err(error) = self.control(json!("shutdown")) {
            eprintln!("stopping the test daemon: {error:#}");
        }
    }
}

struct Client {
    child: Child,
    messages: Receiver<Result<Value>>,
}

impl Client {
    #[allow(
        clippy::disallowed_methods,
        reason = "This synchronous subprocess test uses std pipes directly, without conversion to smol"
    )]
    fn attach(daemon: &LocalDaemon, project: &Path) -> Result<Self> {
        let mut child = Command::new(HELPER)
            .arg("--user-data-dir")
            .arg(daemon.0.path())
            .args([
                "acp-mux",
                "attach",
                "--agent",
                "local-test",
                "--key-env",
                "PWD",
                "--",
                "/bin/sh",
                "-c",
                FAKE_AGENT,
            ])
            // A stale inherited PWD must not defeat project isolation.
            .env("PWD", "/wrong-project")
            .current_dir(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().context("capturing attach stdout")?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = line
                    .map_err(anyhow::Error::from)
                    .and_then(|line| Ok(serde_json::from_str(&line)?));
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        let mut client = Self { child, messages };
        client.send(json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":1}}))?;
        let initialized = client.receive()?;
        anyhow::ensure!(
            initialized["result"]["_meta"]["zed.dev/acpMux"].is_object(),
            "{initialized}"
        );
        Ok(client)
    }

    fn send(&mut self, message: Value) -> Result<()> {
        let stdin = self.child.stdin.as_mut().context("attach stdin closed")?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        Ok(())
    }

    fn receive(&self) -> Result<Value> {
        self.messages.recv_timeout(Duration::from_secs(10))?
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("stopping the test attach: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("waiting for the test attach: {error}");
        }
    }
}

#[test]
fn local_helper_reconnects_and_isolates_projects_and_data_directories() -> Result<()> {
    let daemon = LocalDaemon::new()?;
    let project = daemon.0.path().join("project");
    let other_project = daemon.0.path().join("other-project");
    std::fs::create_dir(&project)?;
    std::fs::create_dir(&other_project)?;

    let mut client = Client::attach(&daemon, &project)?;
    assert!(daemon.0.path().join("logs/acp-mux.log").is_file());
    client.send(json!({"jsonrpc":"2.0","id":"new","method":"session/new","params":{"cwd":project,"mcpServers":[]}}))?;
    assert_eq!(client.receive()?["result"]["sessionId"], "local-session");
    client.send(json!({"jsonrpc":"2.0","id":"prompt","method":"session/prompt","params":{"sessionId":"local-session","prompt":[{"type":"text","text":"run"}]}}))?;
    assert_eq!(client.receive()?["method"], "session/request_permission");
    let before = daemon.control(json!("list"))?;
    let original_pid = before["agents"][0]["pid"].clone();
    assert!(original_pid.is_number());
    drop(client);

    let mut reconnected = Client::attach(&daemon, &project)?;
    assert_eq!(
        daemon.control(json!("list"))?["agents"][0]["pid"],
        original_pid
    );
    reconnected.send(json!({"jsonrpc":"2.0","id":"load","method":"session/load","params":{"sessionId":"local-session","cwd":project,"mcpServers":[]}}))?;
    let mut permission = None;
    let mut loaded = false;
    let mut running = false;
    while permission.is_none() || !loaded || !running {
        let message = reconnected.receive()?;
        if message["method"] == "session/request_permission" {
            permission = Some(message["id"].clone());
        }
        loaded |= message["id"] == "load" && message.get("result").is_some();
        running |= message["method"] == "_zed.dev/acpMux/turn_state"
            && message["params"]["state"] == "running";
    }
    reconnected.send(json!({"jsonrpc":"2.0","id":permission.context("permission request")?,"result":{"outcome":{"outcome":"selected","optionId":"yes"}}}))?;
    let mut approved = false;
    let mut finished = false;
    while !approved || !finished {
        let message = reconnected.receive()?;
        approved |= message["params"]["update"]["content"]["text"] == "approved";
        finished |= message["method"] == "_zed.dev/acpMux/turn_state"
            && message["params"]["state"] == "idle";
    }

    let _other_client = Client::attach(&daemon, &other_project)?;
    assert_eq!(
        daemon.control(json!("list"))?["agents"]
            .as_array()
            .context("agents")?
            .len(),
        2
    );

    let separate_daemon = LocalDaemon::new()?;
    let _separate_client = Client::attach(&separate_daemon, &project)?;
    let separate = separate_daemon.control(json!("list"))?;
    assert_ne!(separate["agents"][0]["pid"], original_pid);
    assert!(
        separate["sessions"]
            .as_array()
            .context("sessions")?
            .is_empty()
    );
    Ok(())
}
