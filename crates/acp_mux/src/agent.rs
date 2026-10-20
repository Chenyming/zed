use anyhow::{Context as _, Result};
use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, io::BufReader};
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};
use std::{collections::HashMap, os::unix::process::CommandExt as _, process::Stdio};

use crate::{META_KEY, PROTOCOL_VERSION, control::AgentSpec, router::ClientId, router::Event};

pub type AgentId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentKey {
    pub name: String,
    pub command_hash: u64,
}

pub struct AgentHandle {
    pub outgoing: Sender<Value>,
    pub pid: Option<u32>,
    pub kill: Box<dyn Fn() + Send>,
}

pub trait AgentLauncher {
    fn launch(
        &mut self,
        agent: AgentId,
        spec: &AgentSpec,
        events: Sender<Event>,
    ) -> Result<AgentHandle>;
}

pub struct AgentState {
    pub key: AgentKey,
    pub program: String,
    pub handle: AgentHandle,
    pub init: InitState,
    pub calls: HashMap<i64, PendingCall>,
    pub pending_requests: Vec<PendingAgentRequest>,
    next_request_id: i64,
}

pub enum InitState {
    NotStarted,
    InFlight { waiters: Vec<(ClientId, Value)> },
    Ready { response: Value },
}

/// What to do with the agent's response to a request the daemon sent it.
pub enum PendingCall {
    Initialize,
    /// A request the daemon made on its own behalf; the response is only logged.
    Internal {
        method: String,
    },
    Forwarded {
        client: ClientId,
        client_request_id: Value,
        kind: CallKind,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallKind {
    Plain,
    NewSession { cwd: String },
    LoadSession { session_id: String },
    Prompt { session_id: String },
    SetMode { session_id: String, mode_id: Value },
    SetModel { session_id: String, model_id: Value },
    SetConfigOption { session_id: String },
    DeleteSession { session_id: String },
}

/// A request from the agent (such as `session/request_permission`) that waits
/// for an answer from one of the attached clients. The first answer wins.
pub struct PendingAgentRequest {
    pub agent_request_id: Value,
    pub session_id: Option<String>,
    pub message: Value,
    /// Clients the request was sent to, with the request id each of them sees.
    pub forwarded: Vec<(ClientId, i64)>,
}

impl AgentState {
    pub fn new(key: AgentKey, program: String, handle: AgentHandle) -> Self {
        Self {
            key,
            program,
            handle,
            init: InitState::NotStarted,
            calls: HashMap::new(),
            pending_requests: Vec::new(),
            next_request_id: 0,
        }
    }

    pub fn send(&self, message: Value) {
        if self.handle.outgoing.try_send(message).is_err() {
            log::warn!("agent {} is no longer accepting messages", self.key.name);
        }
    }

    pub fn request(&mut self, method: &str, params: Value, call: PendingCall) -> i64 {
        self.next_request_id += 1;
        let id = self.next_request_id;
        self.calls.insert(id, call);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    pub fn initialize_response(&self) -> Option<&Value> {
        match &self.init {
            InitState::Ready { response } => Some(response),
            _ => None,
        }
    }

    pub fn supports(&self, capability: &[&str]) -> bool {
        let Some(mut value) = self
            .initialize_response()
            .and_then(|response| response.get("agentCapabilities"))
        else {
            return false;
        };
        for key in capability {
            let Some(next) = value.get(key) else {
                return false;
            };
            value = next;
        }
        !value.is_null() && value != &Value::Bool(false)
    }
}

/// The parameters the daemon sends in its single `initialize` request. File
/// system and terminal access stay with the agent so that it behaves the same
/// whether or not a client is attached.
pub fn daemon_initialize_params(client_params: &Value, version: &str) -> Value {
    let mut params = client_params.clone();
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    let capabilities = object
        .entry("clientCapabilities")
        .or_insert_with(|| json!({}));
    if !capabilities.is_object() {
        *capabilities = json!({});
    }
    if let Some(capabilities) = capabilities.as_object_mut() {
        capabilities.insert(
            "fs".into(),
            json!({"readTextFile": false, "writeTextFile": false}),
        );
        capabilities.insert("terminal".into(), Value::Bool(false));
        capabilities.insert("auth".into(), json!({"terminal": false}));
        if let Some(meta) = capabilities.get_mut("_meta").and_then(Value::as_object_mut) {
            meta.remove("terminal_output");
            meta.remove("terminal-auth");
        }
    }
    object.insert(
        "clientInfo".into(),
        json!({"name": "acp-mux", "version": version}),
    );
    params
}

/// Rewrites the agent's cached `initialize` response for an attaching client:
/// the daemon implements loading, listing and closing sessions itself.
pub fn client_initialize_response(agent_response: &Value) -> Value {
    let mut response = agent_response.clone();
    let Some(object) = response.as_object_mut() else {
        return response;
    };
    let capabilities = object
        .entry("agentCapabilities")
        .or_insert_with(|| json!({}));
    if !capabilities.is_object() {
        *capabilities = json!({});
    }
    if let Some(capabilities) = capabilities.as_object_mut() {
        capabilities.insert("loadSession".into(), Value::Bool(true));
        let session_capabilities = capabilities
            .entry("sessionCapabilities")
            .or_insert_with(|| json!({}));
        if !session_capabilities.is_object() {
            *session_capabilities = json!({});
        }
        if let Some(session_capabilities) = session_capabilities.as_object_mut() {
            session_capabilities.insert("list".into(), json!({}));
            session_capabilities.insert("close".into(), json!({}));
            session_capabilities.remove("resume");
        }
    }
    let meta = object.entry("_meta").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(META_KEY.into(), json!({"protocol": PROTOCOL_VERSION}));
    }
    response
}

pub struct ProcessLauncher;

impl AgentLauncher for ProcessLauncher {
    fn launch(
        &mut self,
        agent: AgentId,
        spec: &AgentSpec,
        events: Sender<Event>,
    ) -> Result<AgentHandle> {
        let mut command = std::process::Command::new(&spec.program);
        command.args(&spec.args).env_clear().envs(&spec.env);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // A separate process group lets `kill` reach the agent's own children.
        command.process_group(0);
        let mut command = smol::process::Command::from(command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning agent {:?}", spec.program))?;
        let pid = child.id();
        let stdin = child.stdin.take().context("agent stdin was not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("agent stdout was not captured")?;
        log::info!("started agent {:?} with pid {pid}", spec.program);

        let (outgoing, outgoing_rx) = smol::channel::unbounded::<Value>();
        smol::spawn(write_messages(stdin, outgoing_rx)).detach();
        smol::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = futures::StreamExt::next(&mut lines).await {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        log::warn!("reading from agent {agent}: {error}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => {
                        if events
                            .send(Event::AgentMessage { agent, message })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => log::warn!("agent {agent} wrote invalid JSON: {error}"),
                }
            }
            let status = match child.status().await {
                Ok(status) => status.to_string(),
                Err(error) => format!("unknown ({error})"),
            };
            events.send(Event::AgentExited { agent, status }).await.ok();
        })
        .detach();

        let process_group = pid as libc::pid_t;
        Ok(AgentHandle {
            outgoing,
            pid: Some(pid),
            kill: Box::new(move || {
                // SAFETY: sending a signal has no memory-safety requirements.
                if unsafe { libc::killpg(process_group, libc::SIGTERM) } != 0 {
                    log::warn!(
                        "failed to signal agent process group {process_group}: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }),
        })
    }
}

pub async fn write_messages(
    mut writer: impl futures::AsyncWrite + Unpin,
    messages: Receiver<Value>,
) {
    while let Ok(message) = messages.recv().await {
        let mut line = match serde_json::to_vec(&message) {
            Ok(line) => line,
            Err(error) => {
                log::error!("serializing message: {error}");
                continue;
            }
        };
        line.push(b'\n');
        if let Err(error) = writer.write_all(&line).await {
            log::info!("write failed: {error}");
            return;
        }
        if let Err(error) = writer.flush().await {
            log::info!("flush failed: {error}");
            return;
        }
    }
    writer.close().await.ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rewrites_initialize_response_for_clients() {
        let response = client_initialize_response(&json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": true},
                "sessionCapabilities": {"resume": {}},
            },
            "authMethods": [],
        }));

        assert_eq!(
            response,
            json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": true},
                    "sessionCapabilities": {"list": {}, "close": {}},
                },
                "authMethods": [],
                "_meta": {"zed.dev/acpMux": {"protocol": PROTOCOL_VERSION}},
            })
        );
    }

    #[test]
    fn disables_client_side_tools_when_initializing_the_agent() {
        let params = daemon_initialize_params(
            &json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": true,
                    "auth": {"terminal": true},
                    "elicitation": {"form": {}},
                    "_meta": {"terminal_output": true, "terminal-auth": true, "other": 1},
                },
                "clientInfo": {"name": "zed", "version": "1"},
            }),
            "9.9.9",
        );

        assert_eq!(
            params,
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "auth": {"terminal": false},
                    "elicitation": {"form": {}},
                    "_meta": {"other": 1},
                },
                "clientInfo": {"name": "acp-mux", "version": "9.9.9"},
            })
        );
    }
}
