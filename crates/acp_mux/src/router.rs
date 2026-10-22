use futures::channel::oneshot;
use serde_json::{Value, json};
use smol::channel::Sender;
use std::{collections::HashMap, mem, time::SystemTime};

use crate::{
    Config, HISTORY_METHOD, META_KEY, PROTOCOL_VERSION, SET_TITLE_METHOD, TURN_STATE_METHOD,
    agent::{
        AgentId, AgentKey, AgentLauncher, AgentState, CallKind, InitState, PendingAgentRequest,
        PendingCall, client_initialize_response, daemon_initialize_params,
    },
    control::{
        AgentSpec, AgentSummary, ControlRequest, KillResponse, KillTarget, ListResponse,
        ShutdownResponse,
    },
    notify::Notice,
    session::{MuxSession, QueuedPrompt, RunningTurn, TurnState},
};

pub type ClientId = u64;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const REQUEST_CANCELLED: i64 = -32800;
const RESOURCE_NOT_FOUND: i64 = -32002;

/// Entries in a page of history when the client doesn't say.
const DEFAULT_HISTORY_PAGE: usize = 100;
/// Most entries in a page of history, whatever the client asks for.
const MAX_HISTORY_PAGE: usize = 1000;
/// Pages of history stop before exceeding this much JSON, so that a few large
/// tool calls don't make a page slow over a mobile connection.
const MAX_HISTORY_PAGE_BYTES: usize = 1024 * 1024;

pub enum Event {
    Attach {
        client: ClientId,
        agent_name: String,
        spec: AgentSpec,
        outgoing: Sender<Value>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// A client that is not tied to an agent, such as a web or mobile client.
    /// It can only list and use existing sessions.
    Observe {
        client: ClientId,
        label: String,
        outgoing: Sender<Value>,
    },
    ClientMessage {
        client: ClientId,
        message: Value,
    },
    ClientDisconnected {
        client: ClientId,
    },
    AgentMessage {
        agent: AgentId,
        message: Value,
    },
    AgentExited {
        agent: AgentId,
        status: String,
    },
    Control {
        request: ControlRequest,
        reply: oneshot::Sender<Value>,
    },
    /// Sent after the reply to a shutdown request was written, so the daemon
    /// does not exit before the control client hears back.
    Exit,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

struct ClientState {
    /// The agent this client was started for, or `None` for an observer.
    agent: Option<AgentId>,
    /// Identifies the client in logs.
    label: String,
    outgoing: Sender<Value>,
    next_request_id: i64,
    /// Requests forwarded from an agent to this client, keyed by the id the client sees.
    forwarded: HashMap<i64, (AgentId, String)>,
}

/// Sessions are keyed by agent name and session id, because a session can
/// outlive the agent process instance that created it.
type SessionKey = (String, String);

enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
    },
    Invalid,
}

fn classify(message: &Value) -> Incoming {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").filter(|id| !id.is_null());
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        },
        (Some(method), None) => Incoming::Notification {
            method: method.to_string(),
            params,
        },
        (None, Some(id)) => Incoming::Response { id: id.clone() },
        (None, None) => Incoming::Invalid,
    }
}

fn request_id_key(id: &Value) -> String {
    id.to_string()
}

fn session_id_of(params: &Value) -> Option<String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The number of latest entries a client asked `session/load` to replay, when
/// it pages through earlier ones with [`HISTORY_METHOD`].
fn history_tail(params: &Value) -> Option<usize> {
    let tail = params
        .get("_meta")?
        .get(META_KEY)?
        .get("history")?
        .get("tail")?
        .as_u64()?;
    Some((tail as usize).clamp(1, MAX_HISTORY_PAGE))
}

fn response_message(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_message(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn notification_message(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn with_id(mut message: Value, id: Value) -> Value {
    if let Some(object) = message.as_object_mut() {
        object.insert("id".into(), id);
    }
    message
}

/// Removes the title from a `session_info_update`, keeping its other fields.
fn drop_title(update: &mut Value) {
    if update.get("sessionUpdate").and_then(Value::as_str) == Some("session_info_update")
        && let Some(update) = update.as_object_mut()
    {
        update.remove("title");
    }
}

fn session_update_message(session_id: &str, update: &Value) -> Value {
    notification_message(
        "session/update",
        json!({"sessionId": session_id, "update": update}),
    )
}

fn turn_state_running(session_id: &str) -> Value {
    notification_message(
        TURN_STATE_METHOD,
        json!({"sessionId": session_id, "state": "running"}),
    )
}

fn turn_state_idle(session_id: &str, stop_reason: &str) -> Value {
    notification_message(
        TURN_STATE_METHOD,
        json!({"sessionId": session_id, "state": "idle", "stopReason": stop_reason}),
    )
}

fn cancel_request_message(request_id: Value) -> Value {
    notification_message("$/cancel_request", json!({"requestId": request_id}))
}

/// What a notification says about a request that waits for the user.
fn request_detail(message: &Value) -> String {
    let params = message.get("params").unwrap_or(&Value::Null);
    params
        .get("toolCall")
        .and_then(|tool_call| tool_call.get("title"))
        .or_else(|| params.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("需要你的回答")
        .to_string()
}

fn notice_key(agent: AgentId, request_key: &str) -> String {
    format!("{agent}:{request_key}")
}

pub struct Router {
    version: String,
    config: Config,
    launcher: Box<dyn AgentLauncher>,
    events: Sender<Event>,
    notices: Option<Sender<Notice>>,
    next_agent_id: AgentId,
    agents: HashMap<AgentId, AgentState>,
    clients: HashMap<ClientId, ClientState>,
    sessions: HashMap<SessionKey, MuxSession>,
}

impl Router {
    pub fn new(
        version: String,
        config: Config,
        launcher: Box<dyn AgentLauncher>,
        events: Sender<Event>,
        notices: Option<Sender<Notice>>,
    ) -> Self {
        Self {
            version,
            config,
            launcher,
            events,
            notices,
            next_agent_id: 0,
            agents: HashMap::new(),
            clients: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Attach {
                client,
                agent_name,
                spec,
                outgoing,
                reply,
            } => {
                let result = self.attach_client(client, agent_name, spec, outgoing);
                if reply.send(result).is_err() {
                    log::info!("client {client} went away during the handshake");
                    self.disconnect_client(client);
                }
            }
            Event::Observe {
                client,
                label,
                outgoing,
            } => {
                log::info!("observer {client} connected ({label})");
                self.clients.insert(
                    client,
                    ClientState {
                        agent: None,
                        label,
                        outgoing,
                        next_request_id: 0,
                        forwarded: HashMap::new(),
                    },
                );
            }
            Event::ClientMessage { client, message } => self.handle_client_message(client, message),
            Event::ClientDisconnected { client } => self.disconnect_client(client),
            Event::AgentMessage { agent, message } => self.handle_agent_message(agent, message),
            Event::AgentExited { agent, status } => self.handle_agent_exit(agent, &status),
            Event::Control { request, reply } => {
                let response = self.handle_control(request);
                if reply.send(response).is_err() {
                    log::info!("control client went away before the response");
                }
            }
            Event::Exit => return Flow::Exit,
        }
        Flow::Continue
    }

    fn send_to_client(clients: &HashMap<ClientId, ClientState>, client: ClientId, message: Value) {
        if let Some(state) = clients.get(&client)
            && state.outgoing.try_send(message).is_err()
        {
            log::info!("client {client} is no longer accepting messages");
        }
    }

    fn notify(&self, notice: Notice) {
        if let Some(notices) = &self.notices
            && notices.try_send(notice).is_err()
        {
            log::warn!("the notifier stopped");
        }
    }

    /// The agent a client was started for and that agent's name. `None` for
    /// observers.
    fn client_agent(&self, client: ClientId) -> Option<(AgentId, String)> {
        let agent = self.clients.get(&client)?.agent?;
        let name = self.agents.get(&agent)?.key.name.clone();
        Some((agent, name))
    }

    fn client_label(&self, client: ClientId) -> &str {
        self.clients
            .get(&client)
            .map_or("disconnected", |state| state.label.as_str())
    }

    /// Finds the session a client refers to. Clients started for an agent only
    /// see that agent's sessions; observers see every session, preferring a live
    /// one when an id is reused across agents.
    fn session_key(&self, client: ClientId, session_id: &str) -> Option<SessionKey> {
        if let Some((_, agent_name)) = self.client_agent(client) {
            let key = (agent_name, session_id.to_string());
            return self.sessions.contains_key(&key).then_some(key);
        }
        if self
            .clients
            .get(&client)
            .is_none_or(|state| state.agent.is_some())
        {
            return None;
        }
        self.sessions
            .iter()
            .filter(|((_, id), _)| id == session_id)
            .max_by_key(|(_, session)| (!session.dead, session.updated_at))
            .map(|(key, _)| key.clone())
    }

    fn is_waiting_for_input(&self, session: &MuxSession) -> bool {
        self.agents.get(&session.agent).is_some_and(|agent| {
            agent
                .pending_requests
                .iter()
                .any(|request| request.session_id.as_ref() == Some(&session.session_id))
        })
    }

    fn attach_client(
        &mut self,
        client: ClientId,
        agent_name: String,
        spec: AgentSpec,
        outgoing: Sender<Value>,
    ) -> Result<(), String> {
        let key = AgentKey {
            name: agent_name,
            command_hash: spec.command_hash(),
        };
        let existing = self
            .agents
            .iter()
            .find(|(_, agent)| agent.key == key)
            .map(|(id, _)| *id);
        let agent = match existing {
            Some(agent) => agent,
            None => {
                self.next_agent_id += 1;
                let agent = self.next_agent_id;
                let handle = self
                    .launcher
                    .launch(agent, &spec, self.events.clone())
                    .map_err(|error| format!("{error:#}"))?;
                self.agents
                    .insert(agent, AgentState::new(key.clone(), spec.program, handle));
                agent
            }
        };
        log::info!("client {client} attached to agent {} ({agent})", key.name);
        self.clients.insert(
            client,
            ClientState {
                agent: Some(agent),
                label: format!("attach {}", key.name),
                outgoing,
                next_request_id: 0,
                forwarded: HashMap::new(),
            },
        );
        Ok(())
    }

    fn handle_client_message(&mut self, client: ClientId, message: Value) {
        if !self.clients.contains_key(&client) {
            return;
        }
        match classify(&message) {
            Incoming::Request { id, method, params } => {
                self.handle_client_request(client, id, &method, params, message)
            }
            Incoming::Notification { method, params } => {
                self.handle_client_notification(client, &method, params, message)
            }
            Incoming::Response { id } => self.handle_client_response(client, id, message),
            Incoming::Invalid => log::warn!("client {client} sent an invalid message"),
        }
    }

    fn handle_client_request(
        &mut self,
        client: ClientId,
        id: Value,
        method: &str,
        params: Value,
        message: Value,
    ) {
        let client_agent = self.client_agent(client);
        match method {
            "initialize" => match client_agent {
                Some((agent, _)) => self.handle_initialize(client, agent, id, &params),
                None => self.initialize_observer(client, id, &params),
            },
            "session/new" => {
                let Some((agent, _)) = client_agent else {
                    self.reject_observer(client, id);
                    return;
                };
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.forward_request(client, agent, id, message, CallKind::NewSession { cwd });
            }
            "session/list" => {
                let agent_name = client_agent.map(|(_, name)| name);
                self.handle_list(client, agent_name.as_deref(), id, &params);
            }
            "session/load" | "session/resume" => self.handle_load(client, client_agent, id, params),
            "session/close" => self.handle_close(client, id, &params),
            "session/prompt" => self.handle_prompt(client, client_agent, id, params, message),
            SET_TITLE_METHOD => self.handle_set_title(client, id, &params),
            HISTORY_METHOD => self.handle_history(client, id, &params),
            _ => {
                let session_id = session_id_of(&params);
                let key = session_id
                    .as_deref()
                    .and_then(|session_id| self.session_key(client, session_id));
                let target = match (&key, &client_agent) {
                    (Some(key), _) => {
                        let Some(session) = self.sessions.get(key) else {
                            return;
                        };
                        if session.dead {
                            Self::send_to_client(
                                &self.clients,
                                client,
                                error_message(
                                    id,
                                    INTERNAL_ERROR,
                                    "The agent process for this session has exited",
                                ),
                            );
                            return;
                        }
                        session.agent
                    }
                    (None, Some((agent, _))) => *agent,
                    (None, None) => {
                        self.reject_observer(client, id);
                        return;
                    }
                };
                let kind = match (method, key.map(|(_, session_id)| session_id).or(session_id)) {
                    ("session/set_mode", Some(session_id)) => CallKind::SetMode {
                        session_id,
                        mode_id: params.get("modeId").cloned().unwrap_or(Value::Null),
                    },
                    ("session/set_model", Some(session_id)) => CallKind::SetModel {
                        session_id,
                        model_id: params.get("modelId").cloned().unwrap_or(Value::Null),
                    },
                    ("session/set_config_option", Some(session_id)) => {
                        CallKind::SetConfigOption { session_id }
                    }
                    ("session/delete", Some(session_id)) => CallKind::DeleteSession { session_id },
                    _ => CallKind::Plain,
                };
                self.forward_request(client, target, id, message, kind);
            }
        }
    }

    /// Names a session for every client. The request is not forwarded, since
    /// agents can't be told a session's name.
    fn handle_set_title(&mut self, client: ClientId, id: Value, params: &Value) {
        let Some(key) =
            session_id_of(params).and_then(|session_id| self.session_key(client, &session_id))
        else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, INVALID_PARAMS, "Unknown session"),
            );
            return;
        };
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty());
        if let Some(session) = self.sessions.get_mut(&key) {
            session.titled_by_user = title.is_some();
        }
        self.share_session_change(
            &key,
            json!({"sessionUpdate": "session_info_update", "title": title}),
            client,
        );
        Self::send_to_client(&self.clients, client, response_message(id, json!({})));
    }

    /// Answers [`HISTORY_METHOD`] with the entries of a session the client
    /// observes that come before `before`, most recent last.
    fn handle_history(&mut self, client: ClientId, id: Value, params: &Value) {
        let session = session_id_of(params)
            .and_then(|session_id| self.session_key(client, &session_id))
            .and_then(|key| self.sessions.get(&key))
            .filter(|session| session.observers.contains(&client));
        let Some(session) = session else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(
                    id,
                    INVALID_PARAMS,
                    "Unknown session, or not loaded by this client",
                ),
            );
            return;
        };
        let Some(before) = params.get("before").and_then(Value::as_u64) else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, INVALID_PARAMS, "Missing before"),
            );
            return;
        };
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_HISTORY_PAGE, |limit| {
                (limit as usize).clamp(1, MAX_HISTORY_PAGE)
            });
        let (start, entries) = session.snapshot.entries_before(
            usize::try_from(before).unwrap_or(usize::MAX),
            limit,
            MAX_HISTORY_PAGE_BYTES,
        );
        Self::send_to_client(
            &self.clients,
            client,
            response_message(id, json!({"updates": entries, "start": start})),
        );
    }

    /// Observers cannot start agents or sessions, only use existing sessions.
    fn reject_observer(&self, client: ClientId, id: Value) {
        Self::send_to_client(
            &self.clients,
            client,
            error_message(
                id,
                METHOD_NOT_FOUND,
                "Method not available to observers, which can only use existing sessions",
            ),
        );
    }

    fn initialize_observer(&self, client: ClientId, id: Value, params: &Value) {
        let protocol_version = params
            .get("protocolVersion")
            .cloned()
            .unwrap_or_else(|| json!(1));
        let response = json!({
            "protocolVersion": protocol_version,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": {"list": {}, "close": {}},
            },
            "agentInfo": {"name": "acp-mux", "version": self.version},
            "authMethods": [],
            "_meta": {META_KEY: {"protocol": PROTOCOL_VERSION, "observer": true}},
        });
        Self::send_to_client(&self.clients, client, response_message(id, response));
    }

    fn forward_request(
        &mut self,
        client: ClientId,
        target: AgentId,
        client_request_id: Value,
        message: Value,
        kind: CallKind,
    ) -> Option<i64> {
        let Some(agent) = self.agents.get_mut(&target) else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(
                    client_request_id,
                    INTERNAL_ERROR,
                    "The agent is not running",
                ),
            );
            return None;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        Some(agent.request(
            &method,
            params,
            PendingCall::Forwarded {
                client,
                client_request_id,
                kind,
            },
        ))
    }

    fn handle_initialize(&mut self, client: ClientId, agent: AgentId, id: Value, params: &Value) {
        let Some(agent) = self.agents.get_mut(&agent) else {
            return;
        };
        match &mut agent.init {
            InitState::Ready { response } => {
                let response = client_initialize_response(response);
                Self::send_to_client(&self.clients, client, response_message(id, response));
            }
            InitState::InFlight { waiters } => waiters.push((client, id)),
            InitState::NotStarted => {
                agent.init = InitState::InFlight {
                    waiters: vec![(client, id)],
                };
                agent.request(
                    "initialize",
                    daemon_initialize_params(params, &self.version),
                    PendingCall::Initialize,
                );
            }
        }
    }

    /// Lists one agent's sessions, or every session for observers.
    fn handle_list(
        &mut self,
        client: ClientId,
        agent_name: Option<&str>,
        id: Value,
        params: &Value,
    ) {
        let cwd = params.get("cwd").and_then(Value::as_str);
        let mut sessions = self
            .sessions
            .values()
            .filter(|session| agent_name.is_none_or(|name| session.agent_name == name))
            .filter(|session| cwd.is_none_or(|cwd| session.cwd == cwd))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let sessions = sessions
            .into_iter()
            .map(|session| session.list_entry(self.is_waiting_for_input(session)))
            .collect::<Vec<_>>();
        Self::send_to_client(
            &self.clients,
            client,
            response_message(id, json!({"sessions": sessions})),
        );
    }

    fn handle_load(
        &mut self,
        client: ClientId,
        client_agent: Option<(AgentId, String)>,
        id: Value,
        params: Value,
    ) {
        let Some(session_id) = session_id_of(&params) else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, INVALID_PARAMS, "Missing sessionId"),
            );
            return;
        };
        if let Some(key) = self.session_key(client, &session_id) {
            self.attach_session(client, id, &key, history_tail(&params));
            return;
        }

        let agent_supports_load = client_agent.as_ref().is_some_and(|(agent, _)| {
            self.agents
                .get(agent)
                .is_some_and(|agent| agent.supports(&["loadSession"]))
        });
        let Some((agent, agent_name)) = client_agent.filter(|_| agent_supports_load) else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, RESOURCE_NOT_FOUND, "Session not found"),
            );
            return;
        };

        // The agent replays the history it persisted itself as `session/update`
        // notifications, which are recorded and forwarded as they arrive.
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.sessions.insert(
            (agent_name.clone(), session_id.clone()),
            MuxSession::new(session_id.clone(), agent, agent_name, cwd, Some(client)),
        );
        let message =
            json!({"jsonrpc": "2.0", "id": id, "method": "session/load", "params": params});
        self.forward_request(
            client,
            agent,
            id,
            message,
            CallKind::LoadSession { session_id },
        );
    }

    /// Adds `client` to the observers of an existing session and brings it up to
    /// date: replayed history, the running turn, then any requests waiting for an
    /// answer. The running-turn notification must come before the requests,
    /// because Zed cancels outstanding permission requests when it starts
    /// mirroring a turn.
    ///
    /// With `tail`, only the last entries of the conversation are replayed (and
    /// all of the session-wide state); the response's `_meta` says where they
    /// start, so the client can ask for earlier ones with [`HISTORY_METHOD`].
    fn attach_session(
        &mut self,
        client: ClientId,
        id: Value,
        key: &SessionKey,
        tail: Option<usize>,
    ) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        session.add_observer(client);
        let agent = session.agent;
        let session_id = session.session_id.clone();
        let mut history = None;
        match tail {
            Some(tail) => {
                let total = session.snapshot.entry_count();
                let (start, entries) =
                    session
                        .snapshot
                        .entries_before(total, tail, MAX_HISTORY_PAGE_BYTES);
                for update in entries.iter().chain(session.snapshot.latest_updates()) {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        session_update_message(&session_id, update),
                    );
                }
                history = Some(json!({"start": start, "total": total}));
            }
            None => {
                for update in session.snapshot.updates() {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        session_update_message(&session_id, update),
                    );
                }
            }
        }
        if let TurnState::Running(turn) = &mut session.turn
            && turn.originator != client
        {
            Self::send_to_client(&self.clients, client, turn_state_running(&session_id));
            if !turn.notified.contains(&client) {
                turn.notified.push(client);
            }
        }
        let mut load_response = session.load_response();
        if let (Some(history), Some(response)) = (history, load_response.as_object_mut()) {
            response.insert("_meta".into(), json!({META_KEY: {"history": history}}));
        }
        Self::send_to_client(&self.clients, client, response_message(id, load_response));
        log::info!(
            "{} is observing session {session_id}",
            self.client_label(client)
        );
        self.forward_pending_requests(agent, &session_id, client);
    }

    fn forward_pending_requests(&mut self, agent: AgentId, session_id: &str, client: ClientId) {
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        for request in agent_state.pending_requests.iter_mut().filter(|request| {
            request.session_id.as_deref() == Some(session_id)
                && !request
                    .forwarded
                    .iter()
                    .any(|(forwarded, _)| *forwarded == client)
        }) {
            Self::forward_agent_request(&mut self.clients, agent, request, client);
        }
    }

    fn forward_agent_request(
        clients: &mut HashMap<ClientId, ClientState>,
        agent: AgentId,
        request: &mut PendingAgentRequest,
        client: ClientId,
    ) {
        let Some(state) = clients.get_mut(&client) else {
            return;
        };
        state.next_request_id += 1;
        let client_request_id = state.next_request_id;
        let message = with_id(request.message.clone(), json!(client_request_id));
        if state.outgoing.try_send(message).is_err() {
            return;
        }
        state.forwarded.insert(
            client_request_id,
            (agent, request_id_key(&request.agent_request_id)),
        );
        request.forwarded.push((client, client_request_id));
    }

    /// Withdraws a request from the clients it was forwarded to, except `keep`
    /// (the client that answered it), so their prompts disappear.
    fn withdraw_request(
        clients: &mut HashMap<ClientId, ClientState>,
        request: &PendingAgentRequest,
        keep: Option<ClientId>,
    ) {
        for (client, client_request_id) in &request.forwarded {
            if Some(*client) == keep {
                continue;
            }
            if let Some(state) = clients.get_mut(client) {
                state.forwarded.remove(client_request_id);
                state
                    .outgoing
                    .try_send(cancel_request_message(json!(client_request_id)))
                    .ok();
            }
        }
    }

    /// Forgets the requests of `session_id` forwarded to `client`, which stopped
    /// observing it. They stay pending for the other observers and are sent
    /// again when the client loads the session.
    fn forget_forwarded_requests(&mut self, agent: AgentId, session_id: &str, client: ClientId) {
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        for request in &mut agent_state.pending_requests {
            if request.session_id.as_deref() != Some(session_id) {
                continue;
            }
            request.forwarded.retain(|(forwarded, client_request_id)| {
                if *forwarded != client {
                    return true;
                }
                if let Some(state) = self.clients.get_mut(&client) {
                    state.forwarded.remove(client_request_id);
                }
                false
            });
        }
    }

    fn handle_close(&mut self, client: ClientId, id: Value, params: &Value) {
        let key =
            session_id_of(params).and_then(|session_id| self.session_key(client, &session_id));
        if let Some(key) = key
            && let Some(session) = self.sessions.get_mut(&key)
            && session.observers.contains(&client)
        {
            session.remove_observer(client);
            let agent = session.agent;
            log::info!(
                "{} stopped observing session {}",
                self.client_label(client),
                key.1
            );
            self.forget_forwarded_requests(agent, &key.1, client);
        }
        Self::send_to_client(&self.clients, client, response_message(id, json!({})));
    }

    fn handle_prompt(
        &mut self,
        client: ClientId,
        client_agent: Option<(AgentId, String)>,
        id: Value,
        params: Value,
        message: Value,
    ) {
        let key =
            session_id_of(&params).and_then(|session_id| self.session_key(client, &session_id));
        let Some(key) = key else {
            match client_agent {
                Some((agent, _)) => {
                    self.forward_request(client, agent, id, message, CallKind::Plain);
                }
                None => Self::send_to_client(
                    &self.clients,
                    client,
                    error_message(id, RESOURCE_NOT_FOUND, "Session not found"),
                ),
            }
            return;
        };
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        if session.dead {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(
                    id,
                    INTERNAL_ERROR,
                    "The agent process for this session has exited",
                ),
            );
            return;
        }
        if session.is_running() {
            // Clients that mirror a running turn cancel it before sending a
            // prompt; others (such as Zed without the acp-mux changes) send it
            // right away, so the running turn is cancelled for them.
            session.queued_prompts.push_back(QueuedPrompt {
                client,
                request_id: id,
                params,
            });
            if let Some(agent) = self.agents.get(&session.agent) {
                agent.send(notification_message(
                    "session/cancel",
                    json!({"sessionId": key.1}),
                ));
            }
            return;
        }
        self.start_prompt(&key, client, id, params);
    }

    /// Sends a prompt to the agent. The other observers see the user's message
    /// and learn that a turn is running, as if they had sent it themselves.
    fn start_prompt(&mut self, key: &SessionKey, client: ClientId, id: Value, params: Value) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        let Some(agent) = self.agents.get_mut(&session.agent) else {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, INTERNAL_ERROR, "The agent is not running"),
            );
            return;
        };
        let user_updates = session.record_prompt(&params);
        let agent_request_id = agent.request(
            "session/prompt",
            params,
            PendingCall::Forwarded {
                client,
                client_request_id: id,
                kind: CallKind::Prompt {
                    session_id: session.session_id.clone(),
                },
            },
        );
        let others = session
            .observers
            .iter()
            .copied()
            .filter(|observer| *observer != client)
            .collect::<Vec<_>>();
        for observer in &others {
            for update in &user_updates {
                Self::send_to_client(
                    &self.clients,
                    *observer,
                    session_update_message(&session.session_id, update),
                );
            }
            Self::send_to_client(
                &self.clients,
                *observer,
                turn_state_running(&session.session_id),
            );
        }
        session.turn = TurnState::Running(RunningTurn {
            agent_request_id,
            originator: client,
            started_at: SystemTime::now(),
            notified: others,
        });
    }

    fn handle_client_notification(
        &mut self,
        client: ClientId,
        method: &str,
        params: Value,
        message: Value,
    ) {
        if method == "$/cancel_request" {
            let Some(request_id) = params.get("requestId") else {
                return;
            };
            for (agent_id, agent_state) in &self.agents {
                let call = agent_state.calls.iter().find(|(_, call)| {
                    matches!(call, PendingCall::Forwarded { client: call_client, client_request_id, .. }
                        if *call_client == client && client_request_id == request_id)
                });
                if let Some((agent_request_id, _)) = call {
                    log::debug!("client {client} cancelled a request to agent {agent_id}");
                    agent_state.send(cancel_request_message(json!(agent_request_id)));
                    return;
                }
            }
            for session in self.sessions.values_mut() {
                if let Some(index) = session
                    .queued_prompts
                    .iter()
                    .position(|queued| queued.client == client && &queued.request_id == request_id)
                    && let Some(queued) = session.queued_prompts.remove(index)
                {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        error_message(queued.request_id, REQUEST_CANCELLED, "Request cancelled"),
                    );
                    return;
                }
            }
            return;
        }

        let target = session_id_of(&params)
            .and_then(|session_id| self.session_key(client, &session_id))
            .and_then(|key| self.sessions.get(&key))
            .map(|session| session.agent)
            .or_else(|| self.client_agent(client).map(|(agent, _)| agent));
        match target.and_then(|target| self.agents.get(&target)) {
            Some(agent) => agent.send(message),
            None => log::debug!("dropping {method} from client {client}: no agent to receive it"),
        }
    }

    fn handle_client_response(&mut self, client: ClientId, id: Value, message: Value) {
        let Some(client_request_id) = id.as_i64() else {
            log::warn!("client {client} answered unknown request {id}");
            return;
        };
        let Some((agent, request_key)) = self
            .clients
            .get_mut(&client)
            .and_then(|state| state.forwarded.remove(&client_request_id))
        else {
            // Another observer answered first, or the agent withdrew the request.
            log::debug!("client {client} answered request {id}, which is no longer pending");
            return;
        };
        let label = self.client_label(client).to_string();
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        let Some(index) = agent_state
            .pending_requests
            .iter()
            .position(|request| request_id_key(&request.agent_request_id) == request_key)
        else {
            return;
        };
        let request = agent_state.pending_requests.remove(index);
        log::info!(
            "{label} (client {client}) answered {} in session {:?}: {}",
            request
                .message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("a request"),
            request.session_id,
            message
                .get("result")
                .or_else(|| message.get("error"))
                .unwrap_or(&Value::Null)
        );
        agent_state.send(with_id(message, request.agent_request_id.clone()));
        Self::withdraw_request(&mut self.clients, &request, Some(client));
        self.notify(Notice::InputResolved {
            key: notice_key(agent, &request_key),
        });
    }

    fn handle_agent_message(&mut self, agent: AgentId, message: Value) {
        if !self.agents.contains_key(&agent) {
            return;
        }
        match classify(&message) {
            Incoming::Response { id } => {
                let call = id.as_i64().and_then(|agent_request_id| {
                    let call = self
                        .agents
                        .get_mut(&agent)?
                        .calls
                        .remove(&agent_request_id)?;
                    Some((agent_request_id, call))
                });
                match call {
                    Some((agent_request_id, call)) => {
                        self.handle_agent_response(agent, agent_request_id, call, message)
                    }
                    None => log::warn!("agent {agent} answered unknown request {id}"),
                }
            }
            Incoming::Request { id, method, params } => {
                self.handle_agent_request(agent, id, &method, params, message)
            }
            Incoming::Notification { method, params } => {
                self.handle_agent_notification(agent, &method, params, message)
            }
            Incoming::Invalid => log::warn!("agent {agent} sent an invalid message"),
        }
    }

    fn handle_agent_response(
        &mut self,
        agent: AgentId,
        agent_request_id: i64,
        call: PendingCall,
        message: Value,
    ) {
        let Some(agent_name) = self.agents.get(&agent).map(|agent| agent.key.name.clone()) else {
            return;
        };
        match call {
            PendingCall::Initialize => self.finish_initialize(agent, &message),
            PendingCall::Internal { method } => {
                if let Some(error) = message.get("error") {
                    log::warn!("agent {agent_name} failed {method}: {error}");
                }
            }
            PendingCall::Forwarded {
                client,
                client_request_id,
                kind,
            } => {
                let result = message
                    .get("result")
                    .filter(|_| message.get("error").is_none());
                let mut next_prompt = None;
                match kind {
                    CallKind::Plain => {}
                    CallKind::NewSession { cwd } => {
                        if let Some(session_id) = result.and_then(session_id_of_result) {
                            let observer = self.clients.contains_key(&client).then_some(client);
                            let mut session = MuxSession::new(
                                session_id.clone(),
                                agent,
                                agent_name.clone(),
                                cwd,
                                observer,
                            );
                            if let Some(result) = result {
                                session.set_session_config(result);
                            }
                            log::info!("agent {agent_name} created session {session_id}");
                            self.sessions.insert((agent_name, session_id), session);
                        }
                    }
                    CallKind::LoadSession { session_id } => {
                        let key = (agent_name, session_id);
                        match result {
                            Some(result) => {
                                if let Some(session) = self.sessions.get_mut(&key) {
                                    session.set_session_config(result);
                                }
                            }
                            None => {
                                if self
                                    .sessions
                                    .get(&key)
                                    .is_some_and(|session| session.agent == agent)
                                {
                                    self.sessions.remove(&key);
                                }
                            }
                        }
                    }
                    CallKind::Prompt { session_id } => {
                        let key = (agent_name, session_id);
                        self.finish_turn(&key, agent_request_id, client, &message);
                        next_prompt = Some(key);
                    }
                    CallKind::SetMode {
                        session_id,
                        mode_id,
                    } => {
                        if result.is_some() {
                            self.share_session_change(
                                &(agent_name, session_id),
                                json!({"sessionUpdate": "current_mode_update", "currentModeId": mode_id}),
                                client,
                            );
                        }
                    }
                    CallKind::SetModel {
                        session_id,
                        model_id,
                    } => {
                        if result.is_some()
                            && let Some(session) = self.sessions.get_mut(&(agent_name, session_id))
                        {
                            session.set_current_model(model_id);
                        }
                    }
                    CallKind::SetConfigOption { session_id } => {
                        if let Some(config_options) =
                            result.and_then(|result| result.get("configOptions"))
                        {
                            self.share_session_change(
                                &(agent_name, session_id),
                                json!({
                                    "sessionUpdate": "config_option_update",
                                    "configOptions": config_options,
                                }),
                                client,
                            );
                        }
                    }
                    CallKind::DeleteSession { session_id } => {
                        if result.is_some() {
                            self.sessions.remove(&(agent_name, session_id));
                        }
                    }
                }
                Self::send_to_client(&self.clients, client, with_id(message, client_request_id));
                if let Some(key) = next_prompt {
                    self.start_queued_prompt(&key);
                }
            }
        }
    }

    /// Records a change one client made to a session and tells its other
    /// observers, because agents do not notify about changes their client asked for.
    fn share_session_change(&mut self, key: &SessionKey, update: Value, author: ClientId) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        session.record_update(&update);
        let message = session_update_message(&session.session_id, &update);
        for observer in &session.observers {
            if *observer != author {
                Self::send_to_client(&self.clients, *observer, message.clone());
            }
        }
    }

    fn finish_initialize(&mut self, agent: AgentId, message: &Value) {
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        let waiters = match mem::replace(&mut agent_state.init, InitState::NotStarted) {
            InitState::InFlight { waiters } => waiters,
            other => {
                agent_state.init = other;
                Vec::new()
            }
        };
        match message
            .get("result")
            .filter(|_| message.get("error").is_none())
        {
            Some(result) => {
                agent_state.init = InitState::Ready {
                    response: result.clone(),
                };
                let response = client_initialize_response(result);
                for (client, id) in waiters {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        response_message(id, response.clone()),
                    );
                }
            }
            None => {
                let error = message.get("error").cloned().unwrap_or_else(
                    || json!({"code": INTERNAL_ERROR, "message": "initialize failed"}),
                );
                log::warn!(
                    "agent {} failed to initialize: {error}",
                    agent_state.key.name
                );
                for (client, id) in waiters {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        json!({"jsonrpc": "2.0", "id": id, "error": error}),
                    );
                }
            }
        }
    }

    fn finish_turn(
        &mut self,
        key: &SessionKey,
        agent_request_id: i64,
        originator: ClientId,
        message: &Value,
    ) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        let Some(turn) = session
            .running_turn_mut()
            .filter(|turn| turn.agent_request_id == agent_request_id)
        else {
            return;
        };
        let notified = mem::take(&mut turn.notified);
        let stop_reason = match message.get("result") {
            Some(result) if message.get("error").is_none() => result
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn")
                .to_string(),
            _ => "error".to_string(),
        };
        log::info!("turn in session {} ended: {stop_reason}", key.1);
        for client in notified {
            // The client that sent the prompt learns about the end from its response.
            if client != originator {
                Self::send_to_client(
                    &self.clients,
                    client,
                    turn_state_idle(&session.session_id, &stop_reason),
                );
            }
        }
        let notice = Notice::TurnEnded {
            agent: session.agent_name.clone(),
            title: session.display_title(),
            stop_reason: stop_reason.clone(),
        };
        session.turn = TurnState::Idle {
            last_stop_reason: Some(stop_reason),
        };
        self.notify(notice);
    }

    fn start_queued_prompt(&mut self, key: &SessionKey) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        if session.is_running() || session.dead {
            return;
        }
        if let Some(queued) = session.queued_prompts.pop_front() {
            self.start_prompt(key, queued.client, queued.request_id, queued.params);
        }
    }

    fn auto_approve_option(&self, params: &Value) -> Option<Value> {
        let kind = params
            .get("toolCall")
            .and_then(|tool_call| tool_call.get("kind"))
            .and_then(Value::as_str)?;
        if !self
            .config
            .auto_approve
            .iter()
            .any(|allowed| allowed == kind)
        {
            return None;
        }
        let options = params.get("options").and_then(Value::as_array)?;
        ["allow_once", "allow_always"].iter().find_map(|wanted| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(wanted))
                .and_then(|option| option.get("optionId").cloned())
        })
    }

    fn any_client_of(&self, agent: AgentId) -> Option<ClientId> {
        self.clients
            .iter()
            .filter(|(_, state)| state.agent == Some(agent))
            .map(|(client, _)| *client)
            .min()
    }

    fn handle_agent_request(
        &mut self,
        agent: AgentId,
        id: Value,
        method: &str,
        params: Value,
        mut message: Value,
    ) {
        let Some(agent_name) = self.agents.get(&agent).map(|agent| agent.key.name.clone()) else {
            return;
        };
        if method.starts_with("fs/") || method.starts_with("terminal/") {
            if let Some(agent_state) = self.agents.get(&agent) {
                agent_state.send(error_message(id, METHOD_NOT_FOUND, "Method not found"));
            }
            return;
        }
        if method == "session/request_permission"
            && let Some(option_id) = self.auto_approve_option(&params)
        {
            log::info!(
                "auto-approved a permission request in session {:?} with option {option_id}",
                session_id_of(&params)
            );
            if let Some(agent_state) = self.agents.get(&agent) {
                agent_state.send(response_message(
                    id,
                    json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
                ));
            }
            return;
        }

        let session_id = session_id_of(&params);
        let mut owner = None;
        if session_id.is_none()
            && let Some(parent_request_id) = params.get("requestId").and_then(Value::as_i64)
            && let Some(PendingCall::Forwarded {
                client,
                client_request_id,
                ..
            }) = self
                .agents
                .get(&agent)
                .and_then(|agent_state| agent_state.calls.get(&parent_request_id))
        {
            owner = Some(*client);
            if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) {
                params.insert("requestId".into(), client_request_id.clone());
            }
        }

        let session = session_id
            .as_ref()
            .and_then(|session_id| self.sessions.get(&(agent_name.clone(), session_id.clone())));
        let targets = match (session, owner) {
            (Some(session), _) => session.observers.clone(),
            (None, Some(owner)) => vec![owner],
            (None, None) => self.any_client_of(agent).into_iter().collect(),
        };
        let notice = session.map(|session| Notice::InputRequested {
            key: notice_key(agent, &request_id_key(&id)),
            agent: agent_name,
            title: session.display_title(),
            detail: request_detail(&message),
        });
        if targets.is_empty() && notice.is_none() {
            if let Some(agent_state) = self.agents.get(&agent) {
                agent_state.send(error_message(
                    id,
                    INTERNAL_ERROR,
                    "No client is attached to answer this request",
                ));
            }
            return;
        }

        let mut request = PendingAgentRequest {
            agent_request_id: id,
            session_id,
            message,
            forwarded: Vec::new(),
        };
        for target in targets {
            Self::forward_agent_request(&mut self.clients, agent, &mut request, target);
        }
        if let Some(agent_state) = self.agents.get_mut(&agent) {
            agent_state.pending_requests.push(request);
        }
        if let Some(notice) = notice {
            self.notify(notice);
        }
    }

    fn handle_agent_notification(
        &mut self,
        agent: AgentId,
        method: &str,
        params: Value,
        message: Value,
    ) {
        let Some(agent_name) = self.agents.get(&agent).map(|agent| agent.key.name.clone()) else {
            return;
        };
        if method == "$/cancel_request" {
            let Some(request_id) = params.get("requestId") else {
                return;
            };
            let key = request_id_key(request_id);
            let Some(agent_state) = self.agents.get_mut(&agent) else {
                return;
            };
            if let Some(index) = agent_state
                .pending_requests
                .iter()
                .position(|request| request_id_key(&request.agent_request_id) == key)
            {
                let request = agent_state.pending_requests.remove(index);
                Self::withdraw_request(&mut self.clients, &request, None);
                self.notify(Notice::InputResolved {
                    key: notice_key(agent, &key),
                });
            }
            return;
        }

        let auto_generate_title = self.config.auto_generate_thread_title;
        let session = session_id_of(&params)
            .and_then(|session_id| self.sessions.get_mut(&(agent_name, session_id)));
        match session {
            Some(session) => {
                let mut message = message;
                if method == "session/update"
                    && let Some(update) = message
                        .get_mut("params")
                        .and_then(|params| params.get_mut("update"))
                {
                    if !auto_generate_title || session.titled_by_user {
                        drop_title(update);
                    }
                    session.record_update(update);
                }
                for observer in &session.observers {
                    Self::send_to_client(&self.clients, *observer, message.clone());
                }
            }
            None => {
                for (client, state) in &self.clients {
                    if state.agent == Some(agent)
                        && state.outgoing.try_send(message.clone()).is_err()
                    {
                        log::info!("client {client} is no longer accepting messages");
                    }
                }
            }
        }
    }

    fn disconnect_client(&mut self, client: ClientId) {
        let Some(state) = self.clients.remove(&client) else {
            return;
        };
        log::info!("{} (client {client}) disconnected", state.label);
        for session in self.sessions.values_mut() {
            session.remove_observer(client);
        }
        for agent_state in self.agents.values_mut() {
            if let InitState::InFlight { waiters } = &mut agent_state.init {
                waiters.retain(|(waiter, _)| *waiter != client);
            }
            let agent_name = agent_state.key.name.clone();
            let outgoing = agent_state.handle.outgoing.clone();
            agent_state.pending_requests.retain_mut(|request| {
                request
                    .forwarded
                    .retain(|(forwarded, _)| *forwarded != client);
                let still_answerable = !request.forwarded.is_empty()
                    || request.session_id.as_ref().is_some_and(|session_id| {
                        self.sessions
                            .contains_key(&(agent_name.clone(), session_id.clone()))
                    });
                if !still_answerable {
                    outgoing
                        .try_send(error_message(
                            request.agent_request_id.clone(),
                            INTERNAL_ERROR,
                            "The client disconnected",
                        ))
                        .ok();
                }
                still_answerable
            });
        }
    }

    fn handle_agent_exit(&mut self, agent: AgentId, status: &str) {
        let Some(agent_state) = self.agents.remove(&agent) else {
            return;
        };
        log::warn!("agent {} exited: {status}", agent_state.key.name);

        if let InitState::InFlight { waiters } = agent_state.init {
            for (client, id) in waiters {
                Self::send_to_client(
                    &self.clients,
                    client,
                    error_message(id, INTERNAL_ERROR, "The agent process exited"),
                );
            }
        }
        for (_, call) in agent_state.calls {
            if let PendingCall::Forwarded {
                client,
                client_request_id,
                ..
            } = call
            {
                Self::send_to_client(
                    &self.clients,
                    client,
                    error_message(
                        client_request_id,
                        INTERNAL_ERROR,
                        "The agent process exited",
                    ),
                );
            }
        }
        for request in &agent_state.pending_requests {
            Self::withdraw_request(&mut self.clients, request, None);
            self.notify(Notice::InputResolved {
                key: notice_key(agent, &request_id_key(&request.agent_request_id)),
            });
        }
        for session in self.sessions.values_mut() {
            if session.agent != agent {
                continue;
            }
            session.dead = true;
            if let TurnState::Running(turn) = mem::replace(
                &mut session.turn,
                TurnState::Idle {
                    last_stop_reason: Some("error".to_string()),
                },
            ) {
                for client in turn.notified {
                    Self::send_to_client(
                        &self.clients,
                        client,
                        turn_state_idle(&session.session_id, "error"),
                    );
                }
            }
            for queued in session.queued_prompts.drain(..) {
                Self::send_to_client(
                    &self.clients,
                    queued.client,
                    error_message(
                        queued.request_id,
                        INTERNAL_ERROR,
                        "The agent process exited",
                    ),
                );
            }
        }

        // Clients started for the exited agent cannot continue without it, so
        // close their connections and let Zed report the exit. Observers stay
        // connected and can still replay the session.
        let orphaned = self
            .clients
            .iter()
            .filter(|(_, state)| state.agent == Some(agent))
            .map(|(client, _)| *client)
            .collect::<Vec<_>>();
        for client in orphaned {
            self.disconnect_client(client);
        }
    }

    fn handle_control(&mut self, request: ControlRequest) -> Value {
        let response = match request {
            ControlRequest::List => serde_json::to_value(self.list_response()),
            ControlRequest::Kill(KillTarget::Session(session_id)) => {
                serde_json::to_value(KillResponse {
                    killed: self.kill_session(&session_id),
                })
            }
            ControlRequest::Kill(KillTarget::Agent(name)) => {
                let mut killed = 0;
                for agent_state in self.agents.values().filter(|agent| agent.key.name == name) {
                    log::info!("killing agent {name} on request");
                    (agent_state.handle.kill)();
                    killed += 1;
                }
                serde_json::to_value(KillResponse { killed })
            }
            ControlRequest::Shutdown => {
                let interrupted = self
                    .sessions
                    .values()
                    .filter(|session| session.is_running())
                    .map(|session| session.session_id.clone())
                    .collect::<Vec<_>>();
                log::info!("shutting down; interrupting running sessions {interrupted:?}");
                for agent_state in self.agents.values() {
                    (agent_state.handle.kill)();
                }
                serde_json::to_value(ShutdownResponse { interrupted })
            }
        };
        response.unwrap_or_else(|error| json!({"error": format!("serializing response: {error}")}))
    }

    fn list_response(&self) -> ListResponse {
        let mut agents = self
            .agents
            .iter()
            .map(|(id, agent)| AgentSummary {
                name: agent.key.name.clone(),
                pid: agent.handle.pid,
                program: agent.program.clone(),
                clients: self
                    .clients
                    .values()
                    .filter(|client| client.agent == Some(*id))
                    .count(),
                sessions: self
                    .sessions
                    .values()
                    .filter(|session| session.agent == *id && !session.dead)
                    .count(),
            })
            .collect::<Vec<_>>();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        let mut sessions = self.sessions.values().collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let sessions = sessions
            .into_iter()
            .map(|session| session.summary(self.is_waiting_for_input(session)))
            .collect();
        let observers = self
            .clients
            .values()
            .filter(|client| client.agent.is_none())
            .count();
        ListResponse {
            agents,
            sessions,
            observers,
        }
    }

    fn kill_session(&mut self, session_id: &str) -> usize {
        let keys = self
            .sessions
            .keys()
            .filter(|(_, id)| id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in &keys {
            let Some(mut session) = self.sessions.remove(key) else {
                continue;
            };
            log::info!("killing session {session_id} on request");
            let agent = session.agent;
            if let Some(agent_state) = self.agents.get_mut(&agent) {
                if session.is_running() {
                    agent_state.send(notification_message(
                        "session/cancel",
                        json!({"sessionId": session_id}),
                    ));
                }
                if agent_state.supports(&["sessionCapabilities", "close"]) {
                    agent_state.request(
                        "session/close",
                        json!({"sessionId": session_id}),
                        PendingCall::Internal {
                            method: "session/close".to_string(),
                        },
                    );
                }
                let (cancelled, kept) = mem::take(&mut agent_state.pending_requests)
                    .into_iter()
                    .partition::<Vec<_>, _>(|request| {
                        request.session_id.as_deref() == Some(session_id)
                    });
                agent_state.pending_requests = kept;
                for request in cancelled {
                    Self::withdraw_request(&mut self.clients, &request, None);
                    let is_permission = request.message.get("method").and_then(Value::as_str)
                        == Some("session/request_permission");
                    let key = notice_key(agent, &request_id_key(&request.agent_request_id));
                    agent_state.send(if is_permission {
                        response_message(
                            request.agent_request_id,
                            json!({"outcome": {"outcome": "cancelled"}}),
                        )
                    } else {
                        error_message(
                            request.agent_request_id,
                            REQUEST_CANCELLED,
                            "Request cancelled",
                        )
                    });
                    if let Some(notices) = &self.notices {
                        notices.try_send(Notice::InputResolved { key }).ok();
                    }
                }
            }
            if let TurnState::Running(turn) = &session.turn {
                for client in &turn.notified {
                    Self::send_to_client(
                        &self.clients,
                        *client,
                        turn_state_idle(session_id, "cancelled"),
                    );
                }
            }
            for queued in session.queued_prompts.drain(..) {
                Self::send_to_client(
                    &self.clients,
                    queued.client,
                    error_message(queued.request_id, REQUEST_CANCELLED, "Session was killed"),
                );
            }
        }
        keys.len()
    }
}

fn session_id_of_result(result: &Value) -> Option<String> {
    result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHandle;
    use anyhow::Result;
    use pretty_assertions::assert_eq;
    use smol::channel::Receiver;
    use std::{cell::RefCell, rc::Rc};

    #[derive(Default, Clone)]
    struct FakeLauncher {
        launched: Rc<RefCell<Vec<(AgentId, Receiver<Value>)>>>,
    }

    impl AgentLauncher for FakeLauncher {
        fn launch(
            &mut self,
            agent: AgentId,
            _spec: &AgentSpec,
            _events: Sender<Event>,
        ) -> Result<AgentHandle> {
            let (outgoing, incoming) = smol::channel::unbounded();
            self.launched.borrow_mut().push((agent, incoming));
            Ok(AgentHandle {
                outgoing,
                pid: None,
                kill: Box::new(|| {}),
            })
        }
    }

    struct Harness {
        router: Router,
        launcher: FakeLauncher,
        clients: HashMap<ClientId, Receiver<Value>>,
        notices: Receiver<Notice>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_config(Config::default())
        }

        fn with_config(config: Config) -> Self {
            let launcher = FakeLauncher::default();
            let (events, _) = smol::channel::unbounded();
            let (notices, notices_receiver) = smol::channel::unbounded();
            Self {
                router: Router::new(
                    "1.0.0".into(),
                    config,
                    Box::new(launcher.clone()),
                    events,
                    Some(notices),
                ),
                launcher,
                clients: HashMap::new(),
                notices: notices_receiver,
            }
        }

        fn observe(&mut self, client: ClientId) {
            let (outgoing, incoming) = smol::channel::unbounded();
            self.router.handle(Event::Observe {
                client,
                label: format!("test observer {client}"),
                outgoing,
            });
            self.clients.insert(client, incoming);
        }

        fn notices(&self) -> Vec<Notice> {
            std::iter::from_fn(|| self.notices.try_recv().ok()).collect()
        }

        fn attach(&mut self, client: ClientId) {
            let (outgoing, incoming) = smol::channel::unbounded();
            self.router
                .attach_client(
                    client,
                    "claude".into(),
                    AgentSpec {
                        program: "claude-acp".into(),
                        ..Default::default()
                    },
                    outgoing,
                )
                .ok();
            self.clients.insert(client, incoming);
        }

        fn from_client(&mut self, client: ClientId, message: Value) {
            self.router.handle(Event::ClientMessage { client, message });
        }

        fn from_agent(&mut self, message: Value) {
            self.router
                .handle(Event::AgentMessage { agent: 1, message });
        }

        fn disconnect(&mut self, client: ClientId) {
            self.router.handle(Event::ClientDisconnected { client });
            self.clients.remove(&client);
        }

        fn client_messages(&self, client: ClientId) -> Vec<Value> {
            let receiver = &self.clients[&client];
            std::iter::from_fn(|| receiver.try_recv().ok()).collect()
        }

        fn agent_messages(&self) -> Vec<Value> {
            let launched = self.launcher.launched.borrow();
            let receiver = &launched[0].1;
            std::iter::from_fn(|| receiver.try_recv().ok()).collect()
        }

        fn initialize(&mut self, client: ClientId) {
            self.from_client(
                client,
                json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1}}),
            );
            let requests = self.agent_messages();
            if let Some(request) = requests.first() {
                self.from_agent(response_message(
                    request["id"].clone(),
                    json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": false}}),
                ));
            }
            self.client_messages(client);
        }

        fn new_session(&mut self, client: ClientId, session_id: &str) {
            self.from_client(
                client,
                json!({"jsonrpc": "2.0", "id": "new", "method": "session/new", "params": {"cwd": "/project", "mcpServers": []}}),
            );
            let request = self.agent_messages().remove(0);
            self.from_agent(response_message(
                request["id"].clone(),
                json!({"sessionId": session_id, "modes": {"currentModeId": "ask", "availableModes": []}}),
            ));
            self.client_messages(client);
        }

        fn prompt(&mut self, client: ClientId, session_id: &str, text: &str) -> Value {
            self.from_client(
                client,
                json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}],
                }}),
            );
            self.agent_messages().remove(0)["id"].clone()
        }

        fn update(&mut self, session_id: &str, text: &str) {
            self.from_agent(notification_message(
                "session/update",
                json!({"sessionId": session_id, "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                }}),
            ));
        }

        fn load(&mut self, client: ClientId, session_id: &str) {
            self.from_client(
                client,
                json!({"jsonrpc": "2.0", "id": "load", "method": "session/load", "params": {
                    "sessionId": session_id, "cwd": "/project", "mcpServers": [],
                }}),
            );
        }
    }

    fn permission_request(id: Value, session_id: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": session_id,
            "toolCall": {"toolCallId": "t1", "kind": "edit"},
            "options": [
                {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"},
            ],
        }})
    }

    #[test]
    fn initializes_the_agent_once_and_rewrites_capabilities() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.attach(2);
        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true},
            }}),
        );
        harness.from_client(
            2,
            json!({"jsonrpc": "2.0", "id": "init", "method": "initialize", "params": {"protocolVersion": 1}}),
        );

        let requests = harness.agent_messages();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(
            requests[0]["params"]["clientCapabilities"]["terminal"],
            false
        );
        assert_eq!(
            requests[0]["params"]["clientCapabilities"]["fs"],
            json!({"readTextFile": false, "writeTextFile": false})
        );

        harness.from_agent(response_message(
            requests[0]["id"].clone(),
            json!({"protocolVersion": 1, "agentCapabilities": {}}),
        ));
        let expected = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": {"list": {}, "close": {}},
            },
            "_meta": {"zed.dev/acpMux": {"protocol": PROTOCOL_VERSION}},
        });
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(json!(0), expected.clone())]
        );
        assert_eq!(
            harness.client_messages(2),
            vec![response_message(json!("init"), expected.clone())]
        );

        harness.attach(3);
        harness.from_client(
            3,
            json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {"protocolVersion": 1}}),
        );
        assert!(harness.agent_messages().is_empty());
        assert_eq!(
            harness.client_messages(3),
            vec![response_message(json!(5), expected)]
        );
    }

    #[test]
    fn maps_request_ids_between_clients_and_the_agent() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.attach(2);
        harness.initialize(1);
        harness.initialize(2);

        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 3, "method": "authenticate", "params": {"methodId": "a"}}),
        );
        harness.from_client(
            2,
            json!({"jsonrpc": "2.0", "id": 3, "method": "authenticate", "params": {"methodId": "b"}}),
        );
        let requests = harness.agent_messages();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0]["id"], requests[1]["id"]);

        harness.from_agent(response_message(
            requests[1]["id"].clone(),
            json!({"b": true}),
        ));
        harness.from_agent(response_message(
            requests[0]["id"].clone(),
            json!({"a": true}),
        ));
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(json!(3), json!({"a": true}))]
        );
        assert_eq!(
            harness.client_messages(2),
            vec![response_message(json!(3), json!({"b": true}))]
        );
    }

    #[test]
    fn rejects_client_side_tool_requests_from_the_agent() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.from_agent(json!({"jsonrpc": "2.0", "id": 9, "method": "fs/read_text_file", "params": {"sessionId": "s", "path": "/a"}}));
        assert_eq!(
            harness.agent_messages(),
            vec![error_message(
                json!(9),
                METHOD_NOT_FOUND,
                "Method not found"
            )]
        );
        assert!(harness.client_messages(1).is_empty());
    }

    #[test]
    fn intercepts_close_and_keeps_the_session_running() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.prompt(1, "s1", "hello");

        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/close", "params": {"sessionId": "s1"}}),
        );
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(json!(4), json!({}))]
        );
        assert!(harness.agent_messages().is_empty());

        harness.update("s1", "detached output");
        assert!(harness.client_messages(1).is_empty());
        let session = &harness.router.sessions[&("claude".to_string(), "s1".to_string())];
        assert!(session.observers.is_empty());
        assert!(session.is_running());
    }

    fn set_title(session_id: &str, title: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": "title", "method": SET_TITLE_METHOD, "params": {
            "sessionId": session_id,
            "title": title,
        }})
    }

    fn agent_title(session_id: &str, title: &str) -> Value {
        notification_message(
            "session/update",
            json!({"sessionId": session_id, "update": {
                "sessionUpdate": "session_info_update",
                "title": title,
                "updatedAt": "2026-01-01T00:00:00Z",
            }}),
        )
    }

    fn listed_titles(harness: &mut Harness, client: ClientId) -> Vec<Value> {
        harness.from_client(
            client,
            json!({"jsonrpc": "2.0", "id": "list", "method": "session/list", "params": {}}),
        );
        harness.client_messages(client)[0]["result"]["sessions"]
            .as_array()
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|session| session["title"].clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn lists_sessions() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.from_client(1, set_title("s1", "Fix it"));
        harness.client_messages(1);

        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 8, "method": "session/list", "params": {"cwd": "/project"}}),
        );
        let response = harness.client_messages(1).remove(0);
        let sessions = response["result"]["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["sessionId"], "s1");
        assert_eq!(sessions[0]["cwd"], "/project");
        assert_eq!(sessions[0]["title"], "Fix it");

        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/list", "params": {"cwd": "/elsewhere"}}),
        );
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(json!(9), json!({"sessions": []}))]
        );
    }

    #[test]
    fn sessions_are_only_titled_by_users_by_default() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.observe(2);
        harness.load(2, "s1");
        harness.client_messages(2);

        // The agent's title is dropped, but the rest of its update still arrives.
        harness.from_agent(agent_title("s1", "Agent title"));
        let forwarded = notification_message(
            "session/update",
            json!({"sessionId": "s1", "update": {
                "sessionUpdate": "session_info_update",
                "updatedAt": "2026-01-01T00:00:00Z",
            }}),
        );
        assert_eq!(harness.client_messages(1), vec![forwarded.clone()]);
        assert_eq!(harness.client_messages(2), vec![forwarded]);
        assert_eq!(listed_titles(&mut harness, 2), vec![Value::Null]);

        // A user's title reaches every other client and replays, but not the agent.
        harness.from_client(1, set_title("s1", " Login fix "));
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(json!("title"), json!({}))]
        );
        assert_eq!(
            harness.client_messages(2),
            vec![session_update_message(
                "s1",
                &json!({"sessionUpdate": "session_info_update", "title": "Login fix"}),
            )]
        );
        assert_eq!(harness.agent_messages(), Vec::<Value>::new());
        assert_eq!(listed_titles(&mut harness, 2), vec![json!("Login fix")]);
        harness.observe(3);
        harness.load(3, "s1");
        assert!(
            harness
                .client_messages(3)
                .iter()
                .any(|message| message["params"]["update"]["title"] == "Login fix")
        );

        harness.from_client(2, set_title("missing", "Other"));
        assert_eq!(
            harness.client_messages(2)[0]["error"]["code"],
            json!(INVALID_PARAMS)
        );
    }

    #[test]
    fn agent_titles_apply_until_a_user_names_the_session_when_enabled() {
        let mut harness = Harness::with_config(Config {
            auto_generate_thread_title: true,
            ..Config::default()
        });
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");

        harness.from_agent(agent_title("s1", "Agent title"));
        harness.client_messages(1);
        assert_eq!(listed_titles(&mut harness, 1), vec![json!("Agent title")]);

        harness.from_client(1, set_title("s1", "Login fix"));
        harness.client_messages(1);
        harness.from_agent(agent_title("s1", "Newer agent title"));
        harness.client_messages(1);
        assert_eq!(listed_titles(&mut harness, 1), vec![json!("Login fix")]);
    }

    #[test]
    fn replays_and_reports_the_running_turn_to_a_new_client() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        let prompt_id = harness.prompt(1, "s1", "do it");
        harness.update("s1", "work");
        harness.update("s1", "ing");
        harness.client_messages(1);
        harness.disconnect(1);

        harness.update("s1", " more");
        harness.from_agent(permission_request(json!("perm"), "s1"));

        harness.attach(2);
        harness.initialize(2);
        harness.load(2, "s1");
        let messages = harness.client_messages(2);
        assert_eq!(
            messages,
            vec![
                notification_message(
                    "session/update",
                    json!({"sessionId": "s1", "update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "do it"}}}),
                ),
                notification_message(
                    "session/update",
                    json!({"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working more"}}}),
                ),
                turn_state_running("s1"),
                response_message(
                    json!("load"),
                    json!({"modes": {"currentModeId": "ask", "availableModes": []}})
                ),
                with_id(permission_request(json!("perm"), "s1"), json!(1)),
            ]
        );

        harness.from_client(
            2,
            response_message(
                json!(1),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}}),
            ),
        );
        assert_eq!(
            harness.agent_messages(),
            vec![response_message(
                json!("perm"),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}})
            )]
        );

        harness.from_agent(response_message(
            prompt_id,
            json!({"stopReason": "end_turn"}),
        ));
        assert_eq!(
            harness.client_messages(2),
            vec![turn_state_idle("s1", "end_turn")]
        );
    }

    #[test]
    fn does_not_report_a_running_turn_to_the_client_that_started_it() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        let prompt_id = harness.prompt(1, "s1", "hi");
        harness.load(1, "s1");
        let messages = harness.client_messages(1);
        assert!(!messages.contains(&turn_state_running("s1")));

        harness.from_agent(response_message(
            prompt_id,
            json!({"stopReason": "end_turn"}),
        ));
        assert_eq!(
            harness.client_messages(1),
            vec![response_message(
                json!(7),
                json!({"stopReason": "end_turn"})
            )]
        );
    }

    #[test]
    fn several_clients_observe_a_session_and_the_first_answer_wins() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.prompt(1, "s1", "hi");
        harness.from_agent(permission_request(json!(11), "s1"));
        assert_eq!(
            harness.client_messages(1),
            vec![with_id(permission_request(json!(11), "s1"), json!(1))]
        );

        harness.attach(2);
        harness.initialize(2);
        harness.load(2, "s1");
        assert!(harness.client_messages(1).is_empty());
        let messages = harness.client_messages(2);
        assert_eq!(
            messages.last(),
            Some(&with_id(permission_request(json!(11), "s1"), json!(1)))
        );

        harness.update("s1", "for everyone");
        assert_eq!(harness.client_messages(1).len(), 1);
        assert_eq!(harness.client_messages(2).len(), 1);

        harness.from_client(
            2,
            response_message(
                json!(1),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}}),
            ),
        );
        assert_eq!(
            harness.agent_messages(),
            vec![response_message(
                json!(11),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}})
            )]
        );
        assert_eq!(
            harness.client_messages(1),
            vec![cancel_request_message(json!(1))]
        );

        // A late answer from the other client is ignored.
        harness.from_client(
            1,
            response_message(
                json!(1),
                json!({"outcome": {"outcome": "selected", "optionId": "no"}}),
            ),
        );
        assert!(harness.agent_messages().is_empty());
    }

    #[test]
    fn observers_list_every_session_but_cannot_start_sessions() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.observe(9);

        harness.from_client(
            9,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}),
        );
        let initialize = harness.client_messages(9).remove(0);
        assert_eq!(
            initialize["result"]["_meta"]["zed.dev/acpMux"]["observer"],
            true
        );
        assert!(harness.agent_messages().is_empty());

        harness.from_client(
            9,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
        );
        let list = harness.client_messages(9).remove(0);
        assert_eq!(list["result"]["sessions"][0]["sessionId"], "s1");
        assert_eq!(
            list["result"]["sessions"][0]["_meta"]["zed.dev/acpMux"],
            json!({"agent": "claude", "state": "idle", "observers": 1})
        );

        for (id, method) in [(3, "session/new"), (4, "authenticate")] {
            harness.from_client(
                9,
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"cwd": "/", "mcpServers": []}}),
            );
            assert_eq!(
                harness.client_messages(9)[0]["error"]["code"],
                METHOD_NOT_FOUND
            );
        }
        harness.load(9, "missing");
        assert_eq!(
            harness.client_messages(9)[0]["error"]["code"],
            RESOURCE_NOT_FOUND
        );
        assert!(harness.agent_messages().is_empty());
    }

    #[test]
    fn prompts_from_one_observer_are_mirrored_to_the_others() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.observe(9);
        harness.load(9, "s1");
        harness.client_messages(9);

        harness.from_client(
            9,
            json!({"jsonrpc": "2.0", "id": "phone", "method": "session/prompt", "params": {
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": "from the phone"}],
            }}),
        );
        let prompt = harness.agent_messages().remove(0);
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(
            harness.client_messages(1),
            vec![
                session_update_message(
                    "s1",
                    &json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "from the phone"}}),
                ),
                turn_state_running("s1"),
            ]
        );
        assert!(harness.client_messages(9).is_empty());

        harness.update("s1", "answer");
        assert_eq!(harness.client_messages(1).len(), 1);
        assert_eq!(harness.client_messages(9).len(), 1);

        harness.from_agent(response_message(
            prompt["id"].clone(),
            json!({"stopReason": "end_turn"}),
        ));
        assert_eq!(
            harness.client_messages(9),
            vec![response_message(
                json!("phone"),
                json!({"stopReason": "end_turn"})
            )]
        );
        assert_eq!(
            harness.client_messages(1),
            vec![turn_state_idle("s1", "end_turn")]
        );
    }

    #[test]
    fn mode_and_config_changes_reach_the_other_observers() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.observe(9);
        harness.load(9, "s1");
        harness.client_messages(9);

        harness.from_client(
            9,
            json!({"jsonrpc": "2.0", "id": 5, "method": "session/set_config_option", "params": {
                "sessionId": "s1", "configId": "model", "value": "fast",
            }}),
        );
        let request = harness.agent_messages().remove(0);
        let config_options = json!([{"id": "model", "type": "select", "currentValue": "fast"}]);
        harness.from_agent(response_message(
            request["id"].clone(),
            json!({"configOptions": config_options}),
        ));
        assert_eq!(
            harness.client_messages(9),
            vec![response_message(
                json!(5),
                json!({"configOptions": config_options})
            )]
        );
        assert_eq!(
            harness.client_messages(1),
            vec![session_update_message(
                "s1",
                &json!({"sessionUpdate": "config_option_update", "configOptions": config_options}),
            )]
        );

        harness.from_client(
            9,
            json!({"jsonrpc": "2.0", "id": 6, "method": "session/set_mode", "params": {
                "sessionId": "s1", "modeId": "code",
            }}),
        );
        let request = harness.agent_messages().remove(0);
        harness.from_agent(response_message(request["id"].clone(), json!({})));
        assert_eq!(
            harness.client_messages(1),
            vec![session_update_message(
                "s1",
                &json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}),
            )]
        );
        harness.client_messages(9);

        // A client that opens the session later sees the current values.
        harness.observe(10);
        harness.load(10, "s1");
        let response = harness.client_messages(10).pop().expect("a load response");
        assert_eq!(response["result"]["configOptions"], config_options);
        assert_eq!(response["result"]["modes"]["currentModeId"], "code");
    }

    #[test]
    fn closing_or_disconnecting_an_observer_keeps_requests_for_the_others() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.observe(9);
        harness.load(9, "s1");
        harness.from_agent(permission_request(json!(5), "s1"));
        harness.client_messages(1);
        harness.client_messages(9);

        harness.disconnect(9);
        assert!(harness.agent_messages().is_empty());
        harness.from_client(
            1,
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/close", "params": {"sessionId": "s1"}}),
        );
        harness.client_messages(1);
        assert!(harness.agent_messages().is_empty());

        harness.observe(10);
        harness.load(10, "s1");
        assert_eq!(
            harness.client_messages(10).last(),
            Some(&with_id(permission_request(json!(5), "s1"), json!(1)))
        );
    }

    #[test]
    fn reports_notices_for_requests_and_finished_turns() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        let prompt_id = harness.prompt(1, "s1", "hi");
        harness.from_agent(permission_request(json!(5), "s1"));
        harness.from_client(
            1,
            response_message(
                json!(1),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}}),
            ),
        );
        harness.from_agent(response_message(
            prompt_id,
            json!({"stopReason": "end_turn"}),
        ));

        assert_eq!(
            harness.notices(),
            vec![
                Notice::InputRequested {
                    key: "1:5".into(),
                    agent: "claude".into(),
                    title: "/project".into(),
                    detail: "需要你的回答".into(),
                },
                Notice::InputResolved { key: "1:5".into() },
                Notice::TurnEnded {
                    agent: "claude".into(),
                    title: "/project".into(),
                    stop_reason: "end_turn".into(),
                },
            ]
        );
    }

    #[test]
    fn keeps_running_a_prompt_after_its_client_disconnects() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        let prompt_id = harness.prompt(1, "s1", "long task");
        harness.disconnect(1);

        assert!(harness.agent_messages().is_empty());
        harness.update("s1", "still going");
        harness.from_agent(response_message(
            prompt_id,
            json!({"stopReason": "end_turn"}),
        ));

        let session = &harness.router.sessions[&("claude".to_string(), "s1".to_string())];
        assert_eq!(
            session.turn,
            TurnState::Idle {
                last_stop_reason: Some("end_turn".to_string())
            }
        );
        assert_eq!(session.snapshot.updates().count(), 2);
    }

    #[test]
    fn cancels_the_running_turn_before_a_prompt_from_another_client() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        let first_prompt = harness.prompt(1, "s1", "first");
        harness.disconnect(1);

        harness.attach(2);
        harness.initialize(2);
        harness.load(2, "s1");
        harness.client_messages(2);
        harness.from_client(
            2,
            json!({"jsonrpc": "2.0", "id": 20, "method": "session/prompt", "params": {
                "sessionId": "s1", "prompt": [{"type": "text", "text": "second"}],
            }}),
        );
        assert_eq!(
            harness.agent_messages(),
            vec![notification_message(
                "session/cancel",
                json!({"sessionId": "s1"})
            )]
        );

        harness.from_agent(response_message(
            first_prompt,
            json!({"stopReason": "cancelled"}),
        ));
        assert_eq!(
            harness.client_messages(2),
            vec![turn_state_idle("s1", "cancelled")]
        );
        let requests = harness.agent_messages();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "session/prompt");
        assert_eq!(requests[0]["params"]["prompt"][0]["text"], "second");

        harness.from_agent(response_message(
            requests[0]["id"].clone(),
            json!({"stopReason": "end_turn"}),
        ));
        assert_eq!(
            harness.client_messages(2),
            vec![response_message(
                json!(20),
                json!({"stopReason": "end_turn"})
            )]
        );
    }

    #[test]
    fn auto_approves_configured_tool_kinds() {
        let mut harness = Harness::with_config(Config {
            enabled: true,
            auto_approve: vec!["edit".to_string()],
            ..Config::default()
        });
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.from_agent(permission_request(json!(3), "s1"));
        assert_eq!(
            harness.agent_messages(),
            vec![response_message(
                json!(3),
                json!({"outcome": {"outcome": "selected", "optionId": "yes"}})
            )]
        );
        assert!(harness.client_messages(1).is_empty());
    }

    #[test]
    fn unknown_sessions_are_not_found_when_the_agent_cannot_load() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.load(1, "missing");
        assert_eq!(
            harness.client_messages(1),
            vec![error_message(
                json!("load"),
                RESOURCE_NOT_FOUND,
                "Session not found"
            )]
        );
    }

    #[test]
    fn agent_exit_fails_requests_and_marks_sessions_dead() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.prompt(1, "s1", "hi");
        harness.router.handle(Event::AgentExited {
            agent: 1,
            status: "signal 9".into(),
        });

        assert_eq!(
            harness.client_messages(1),
            vec![error_message(
                json!(7),
                INTERNAL_ERROR,
                "The agent process exited"
            )]
        );
        assert!(harness.router.clients.is_empty());
        let session = &harness.router.sessions[&("claude".to_string(), "s1".to_string())];
        assert!(session.dead);
        assert!(!session.is_running());
    }

    fn message_update(session_id: &str, message_id: usize) -> Value {
        notification_message(
            "session/update",
            json!({"sessionId": session_id, "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": message_id.to_string(),
                "content": {"type": "text", "text": format!("message {message_id}")},
            }}),
        )
    }

    fn load_tail(harness: &mut Harness, client: ClientId, session_id: &str, tail: u64) {
        harness.from_client(
            client,
            json!({"jsonrpc": "2.0", "id": "load", "method": "session/load", "params": {
                "sessionId": session_id, "cwd": "/project", "mcpServers": [],
                "_meta": {META_KEY: {"history": {"tail": tail}}},
            }}),
        );
    }

    fn history(harness: &mut Harness, client: ClientId, params: Value) -> Value {
        harness.from_client(
            client,
            json!({"jsonrpc": "2.0", "id": "history", "method": HISTORY_METHOD, "params": params}),
        );
        harness.client_messages(client).remove(0)
    }

    /// An attached client with session `s1`, whose conversation is a user
    /// message followed by agent messages 1 to `messages`, and a plan.
    fn session_with_messages(messages: usize) -> Harness {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.prompt(1, "s1", "go");
        for message_id in 1..=messages {
            harness.from_agent(message_update("s1", message_id));
        }
        harness.from_agent(notification_message(
            "session/update",
            json!({"sessionId": "s1", "update": {"sessionUpdate": "plan", "entries": []}}),
        ));
        harness.client_messages(1);
        harness
    }

    #[test]
    fn loads_only_the_latest_entries_when_asked() {
        let mut harness = session_with_messages(4);
        harness.observe(2);
        load_tail(&mut harness, 2, "s1", 2);
        assert_eq!(
            harness.client_messages(2),
            vec![
                message_update("s1", 3),
                message_update("s1", 4),
                session_update_message("s1", &json!({"sessionUpdate": "plan", "entries": []})),
                turn_state_running("s1"),
                response_message(
                    json!("load"),
                    json!({
                        "modes": {"currentModeId": "ask", "availableModes": []},
                        "_meta": {META_KEY: {"history": {"start": 3, "total": 5}}},
                    })
                ),
            ]
        );

        // Without a tail, loading replays everything and has no history `_meta`.
        harness.observe(3);
        harness.load(3, "s1");
        let messages = harness.client_messages(3);
        assert_eq!(messages.len(), 8);
        assert_eq!(
            messages.last(),
            Some(&response_message(
                json!("load"),
                json!({"modes": {"currentModeId": "ask", "availableModes": []}})
            ))
        );
    }

    #[test]
    fn pages_through_earlier_entries() {
        let mut harness = session_with_messages(4);
        harness.observe(2);
        load_tail(&mut harness, 2, "s1", 2);
        harness.client_messages(2);

        let user_message = json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "go"},
        });
        let entry =
            |message_id: usize| message_update("s1", message_id)["params"]["update"].clone();
        assert_eq!(
            history(
                &mut harness,
                2,
                json!({"sessionId": "s1", "before": 3, "limit": 2})
            ),
            response_message(
                json!("history"),
                json!({"updates": [entry(1), entry(2)], "start": 1})
            )
        );
        assert_eq!(
            history(
                &mut harness,
                2,
                json!({"sessionId": "s1", "before": 1, "limit": 2})
            ),
            response_message(
                json!("history"),
                json!({"updates": [user_message], "start": 0})
            )
        );
        assert_eq!(
            history(&mut harness, 2, json!({"sessionId": "s1", "before": 0})),
            response_message(json!("history"), json!({"updates": [], "start": 0}))
        );
    }

    #[test]
    fn history_pages_show_tool_calls_as_they_are_now() {
        let mut harness = session_with_messages(0);
        let tool_call = |status: &str| {
            notification_message(
                "session/update",
                json!({"sessionId": "s1", "update": {
                    "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": status,
                }}),
            )
        };
        harness.from_agent(tool_call("in_progress"));
        harness.from_agent(message_update("s1", 1));
        harness.client_messages(1);
        harness.observe(2);
        load_tail(&mut harness, 2, "s1", 1);
        harness.client_messages(2);

        // The update reaches the observer live even though it hasn't loaded the
        // tool call, and the page it loads later already includes it.
        harness.from_agent(tool_call("completed"));
        assert_eq!(harness.client_messages(2), vec![tool_call("completed")]);
        let page = history(
            &mut harness,
            2,
            json!({"sessionId": "s1", "before": 2, "limit": 1}),
        );
        assert_eq!(
            page["result"],
            json!({"updates": [{
                "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            }], "start": 1})
        );
    }

    #[test]
    fn only_clients_observing_a_session_get_its_history() {
        let mut harness = session_with_messages(2);
        harness.observe(2);
        let error = |message: &str| error_message(json!("history"), INVALID_PARAMS, message);
        let not_loaded = "Unknown session, or not loaded by this client";
        assert_eq!(
            history(&mut harness, 2, json!({"sessionId": "s1", "before": 3})),
            error(not_loaded)
        );

        load_tail(&mut harness, 2, "s1", 1);
        harness.client_messages(2);
        assert_eq!(
            history(&mut harness, 2, json!({"sessionId": "s1"})),
            error("Missing before")
        );
        assert_eq!(
            history(
                &mut harness,
                2,
                json!({"sessionId": "missing", "before": 3})
            ),
            error(not_loaded)
        );

        harness.from_client(
            2,
            json!({"jsonrpc": "2.0", "id": "close", "method": "session/close", "params": {"sessionId": "s1"}}),
        );
        harness.client_messages(2);
        assert_eq!(
            history(&mut harness, 2, json!({"sessionId": "s1", "before": 3})),
            error(not_loaded)
        );
    }
}
