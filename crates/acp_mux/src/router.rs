use futures::channel::oneshot;
use serde_json::{Value, json};
use smol::channel::Sender;
use std::{collections::HashMap, mem, time::SystemTime};

use crate::{
    Config, TURN_STATE_METHOD,
    agent::{
        AgentId, AgentKey, AgentLauncher, AgentState, CallKind, InitState, PendingAgentRequest,
        PendingCall, client_initialize_response, daemon_initialize_params,
    },
    control::{
        AgentSpec, AgentSummary, ControlRequest, KillResponse, KillTarget, ListResponse,
        ShutdownResponse,
    },
    session::{MuxSession, QueuedPrompt, RunningTurn, TurnState},
};

pub type ClientId = u64;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const REQUEST_CANCELLED: i64 = -32800;
const RESOURCE_NOT_FOUND: i64 = -32002;

pub enum Event {
    Attach {
        client: ClientId,
        agent_name: String,
        spec: AgentSpec,
        outgoing: Sender<Value>,
        reply: oneshot::Sender<Result<(), String>>,
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
    agent: AgentId,
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

pub struct Router {
    version: String,
    config: Config,
    launcher: Box<dyn AgentLauncher>,
    events: Sender<Event>,
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
    ) -> Self {
        Self {
            version,
            config,
            launcher,
            events,
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

    fn client_agent(&self, client: ClientId) -> Option<(AgentId, String)> {
        let agent = self.clients.get(&client)?.agent;
        let name = self.agents.get(&agent)?.key.name.clone();
        Some((agent, name))
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
                agent,
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
        let Some((agent, agent_name)) = self.client_agent(client) else {
            return;
        };
        match method {
            "initialize" => self.handle_initialize(client, agent, id, &params),
            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.forward_request(client, agent, id, message, CallKind::NewSession { cwd });
            }
            "session/list" => self.handle_list(client, &agent_name, id, &params),
            "session/load" | "session/resume" => {
                self.handle_load(client, agent, agent_name, id, params)
            }
            "session/close" => self.handle_close(client, &agent_name, id, &params),
            "session/prompt" => self.handle_prompt(client, agent, agent_name, id, params, message),
            _ => {
                let session_id = session_id_of(&params);
                let mut target = agent;
                if let Some(session) = session_id
                    .as_ref()
                    .and_then(|session_id| self.sessions.get(&(agent_name, session_id.clone())))
                {
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
                    target = session.agent;
                }
                let kind = match (method, session_id) {
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

    fn handle_list(&mut self, client: ClientId, agent_name: &str, id: Value, params: &Value) {
        let cwd = params.get("cwd").and_then(Value::as_str);
        let mut sessions = self
            .sessions
            .values()
            .filter(|session| session.agent_name == agent_name)
            .filter(|session| cwd.is_none_or(|cwd| session.cwd == cwd))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let sessions = sessions
            .into_iter()
            .map(MuxSession::list_entry)
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
        agent: AgentId,
        agent_name: String,
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
        let key = (agent_name.clone(), session_id.clone());
        if self.sessions.contains_key(&key) {
            self.attach_session(client, id, &key);
            return;
        }

        let agent_supports_load = self
            .agents
            .get(&agent)
            .is_some_and(|agent| agent.supports(&["loadSession"]));
        if !agent_supports_load {
            Self::send_to_client(
                &self.clients,
                client,
                error_message(id, RESOURCE_NOT_FOUND, "Session not found"),
            );
            return;
        }

        // The agent replays the history it persisted itself as `session/update`
        // notifications, which are recorded and forwarded as they arrive.
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.sessions.insert(
            key,
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

    /// Makes `client` the observer of an existing session (like `tmux attach -d`)
    /// and brings it up to date: replayed history, the running turn, then any
    /// requests waiting for an answer. The running-turn notification must come
    /// before the requests, because the client cancels outstanding permission
    /// requests when it starts mirroring a turn.
    fn attach_session(&mut self, client: ClientId, id: Value, key: &SessionKey) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        let previous = session.observer.replace(client);
        let agent = session.agent;
        let session_id = session.session_id.clone();
        for update in session.snapshot.updates() {
            Self::send_to_client(
                &self.clients,
                client,
                notification_message(
                    "session/update",
                    json!({"sessionId": session_id, "update": update}),
                ),
            );
        }
        if let TurnState::Running(turn) = &mut session.turn
            && turn.originator != client
        {
            Self::send_to_client(&self.clients, client, turn_state_running(&session_id));
            if !turn.notified.contains(&client) {
                turn.notified.push(client);
            }
        }
        let load_response = session.load_response();
        Self::send_to_client(&self.clients, client, response_message(id, load_response));

        if let Some(previous) = previous.filter(|previous| *previous != client) {
            log::info!("client {client} took over session {session_id} from client {previous}");
            self.unforward_requests(agent, Some(&session_id), previous, true);
        }
        self.forward_pending_requests(agent, key);
    }

    fn forward_pending_requests(&mut self, agent: AgentId, key: &SessionKey) {
        let Some(observer) = self.sessions.get(key).and_then(|session| session.observer) else {
            return;
        };
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        for request in agent_state.pending_requests.iter_mut().filter(|request| {
            request.forwarded_to.is_none() && request.session_id.as_ref() == Some(&key.1)
        }) {
            Self::forward_agent_request(&mut self.clients, agent, request, observer);
        }
    }

    fn forward_agent_request(
        clients: &mut HashMap<ClientId, ClientState>,
        agent: AgentId,
        request: &mut PendingAgentRequest,
        client: ClientId,
    ) -> bool {
        let Some(state) = clients.get_mut(&client) else {
            return false;
        };
        state.next_request_id += 1;
        let client_request_id = state.next_request_id;
        state.forwarded.insert(
            client_request_id,
            (agent, request_id_key(&request.agent_request_id)),
        );
        let message = with_id(request.message.clone(), json!(client_request_id));
        if state.outgoing.try_send(message).is_err() {
            state.forwarded.remove(&client_request_id);
            return false;
        }
        request.forwarded_to = Some((client, client_request_id));
        true
    }

    /// Takes back requests forwarded to `client`, so they can be sent to the
    /// next client that attaches.
    fn unforward_requests(
        &mut self,
        agent: AgentId,
        session_id: Option<&str>,
        client: ClientId,
        notify_client: bool,
    ) {
        let Some(agent_state) = self.agents.get_mut(&agent) else {
            return;
        };
        for request in &mut agent_state.pending_requests {
            let Some((forwarded_client, client_request_id)) = request.forwarded_to else {
                continue;
            };
            if forwarded_client != client
                || session_id
                    .is_some_and(|session_id| request.session_id.as_deref() != Some(session_id))
            {
                continue;
            }
            request.forwarded_to = None;
            if let Some(state) = self.clients.get_mut(&client) {
                state.forwarded.remove(&client_request_id);
                if notify_client {
                    state
                        .outgoing
                        .try_send(cancel_request_message(json!(client_request_id)))
                        .ok();
                }
            }
        }
    }

    fn handle_close(&mut self, client: ClientId, agent_name: &str, id: Value, params: &Value) {
        let session_id = session_id_of(params);
        let detached = session_id.as_ref().and_then(|session_id| {
            let session = self
                .sessions
                .get_mut(&(agent_name.to_string(), session_id.clone()))?;
            if session.observer != Some(client) {
                return None;
            }
            session.observer = None;
            Some(session.agent)
        });
        if let (Some(agent), Some(session_id)) = (detached, &session_id) {
            log::info!("client {client} detached from session {session_id}");
            self.unforward_requests(agent, Some(session_id), client, false);
        }
        Self::send_to_client(&self.clients, client, response_message(id, json!({})));
    }

    fn handle_prompt(
        &mut self,
        client: ClientId,
        agent: AgentId,
        agent_name: String,
        id: Value,
        params: Value,
        message: Value,
    ) {
        let Some(session_id) = session_id_of(&params) else {
            self.forward_request(client, agent, id, message, CallKind::Plain);
            return;
        };
        let key = (agent_name, session_id.clone());
        let Some(session) = self.sessions.get_mut(&key) else {
            self.forward_request(client, agent, id, message, CallKind::Plain);
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
            // Only clients that cannot mirror a running turn send a prompt now.
            session.queued_prompts.push_back(QueuedPrompt {
                client,
                request_id: id,
                params,
            });
            if let Some(agent) = self.agents.get(&session.agent) {
                agent.send(notification_message(
                    "session/cancel",
                    json!({"sessionId": session_id}),
                ));
            }
            return;
        }
        self.start_prompt(&key, client, id, params);
    }

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
        session.record_prompt(&params);
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
        session.turn = TurnState::Running(RunningTurn {
            agent_request_id,
            originator: client,
            started_at: SystemTime::now(),
            notified: Vec::new(),
        });
    }

    fn handle_client_notification(
        &mut self,
        client: ClientId,
        method: &str,
        params: Value,
        message: Value,
    ) {
        let Some((agent, agent_name)) = self.client_agent(client) else {
            return;
        };
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
            .and_then(|session_id| self.sessions.get(&(agent_name, session_id)))
            .map(|session| session.agent)
            .unwrap_or(agent);
        if let Some(agent) = self.agents.get(&target) {
            agent.send(message);
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
            log::warn!("client {client} answered unknown request {id}");
            return;
        };
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
        if request.message.get("method").and_then(Value::as_str)
            == Some("session/request_permission")
        {
            log::info!(
                "client {client} answered a permission request in session {:?}: {}",
                request.session_id,
                message
                    .get("result")
                    .or_else(|| message.get("error"))
                    .unwrap_or(&Value::Null)
            );
        }
        agent_state.send(with_id(message, request.agent_request_id));
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
                        if result.is_some()
                            && let Some(session) = self.sessions.get_mut(&(agent_name, session_id))
                        {
                            session.set_current_mode(mode_id);
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
                            && let Some(session) = self.sessions.get_mut(&(agent_name, session_id))
                        {
                            session.set_config_options(config_options.clone());
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
        session.turn = TurnState::Idle {
            last_stop_reason: Some(stop_reason),
        };
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
            .filter(|(_, state)| state.agent == agent)
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
            .and_then(|session_id| self.sessions.get(&(agent_name, session_id.clone())));
        let known_session = session.is_some();
        let target = match (session, owner) {
            (Some(session), _) => session.observer,
            (None, Some(owner)) => Some(owner),
            (None, None) => self.any_client_of(agent),
        };
        if target.is_none() && !known_session {
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
            forwarded_to: None,
        };
        if let Some(target) = target {
            Self::forward_agent_request(&mut self.clients, agent, &mut request, target);
        }
        if let Some(agent_state) = self.agents.get_mut(&agent) {
            agent_state.pending_requests.push(request);
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
                if let Some((client, client_request_id)) = request.forwarded_to
                    && let Some(state) = self.clients.get_mut(&client)
                {
                    state.forwarded.remove(&client_request_id);
                    state
                        .outgoing
                        .try_send(cancel_request_message(json!(client_request_id)))
                        .ok();
                }
            }
            return;
        }

        let session = session_id_of(&params)
            .and_then(|session_id| self.sessions.get_mut(&(agent_name, session_id)));
        match session {
            Some(session) => {
                if method == "session/update"
                    && let Some(update) = params.get("update")
                {
                    session.record_update(update);
                }
                if let Some(observer) = session.observer {
                    Self::send_to_client(&self.clients, observer, message);
                }
            }
            None => {
                for (client, state) in &self.clients {
                    if state.agent == agent && state.outgoing.try_send(message.clone()).is_err() {
                        log::info!("client {client} is no longer accepting messages");
                    }
                }
            }
        }
    }

    fn disconnect_client(&mut self, client: ClientId) {
        if self.clients.remove(&client).is_none() {
            return;
        }
        log::info!("client {client} disconnected");
        for session in self.sessions.values_mut() {
            if session.observer == Some(client) {
                session.observer = None;
            }
        }
        for agent_state in self.agents.values_mut() {
            if let InitState::InFlight { waiters } = &mut agent_state.init {
                waiters.retain(|(waiter, _)| *waiter != client);
            }
            let agent_name = agent_state.key.name.clone();
            let outgoing = agent_state.handle.outgoing.clone();
            agent_state.pending_requests.retain_mut(|request| {
                if request
                    .forwarded_to
                    .is_some_and(|(forwarded_client, _)| forwarded_client == client)
                {
                    request.forwarded_to = None;
                }
                let waits_for_attach = request.forwarded_to.is_some()
                    || request.session_id.as_ref().is_some_and(|session_id| {
                        self.sessions
                            .contains_key(&(agent_name.clone(), session_id.clone()))
                    });
                if !waits_for_attach {
                    outgoing
                        .try_send(error_message(
                            request.agent_request_id.clone(),
                            INTERNAL_ERROR,
                            "The client disconnected",
                        ))
                        .ok();
                }
                waits_for_attach
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
        for request in agent_state.pending_requests {
            if let Some((client, client_request_id)) = request.forwarded_to
                && let Some(state) = self.clients.get_mut(&client)
            {
                state.forwarded.remove(&client_request_id);
                state
                    .outgoing
                    .try_send(cancel_request_message(json!(client_request_id)))
                    .ok();
            }
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

        // Clients of the exited agent cannot continue without it, so close their
        // connections and let Zed report the exit.
        let orphaned = self
            .clients
            .iter()
            .filter(|(_, state)| state.agent == agent)
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
                    .filter(|client| client.agent == *id)
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
            .map(|session| {
                let waiting = self.agents.get(&session.agent).is_some_and(|agent| {
                    agent
                        .pending_requests
                        .iter()
                        .any(|request| request.session_id.as_ref() == Some(&session.session_id))
                });
                session.summary(waiting)
            })
            .collect();
        ListResponse { agents, sessions }
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
            if let Some(agent_state) = self.agents.get_mut(&session.agent) {
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
                    if let Some((client, client_request_id)) = request.forwarded_to
                        && let Some(state) = self.clients.get_mut(&client)
                    {
                        state.forwarded.remove(&client_request_id);
                        state
                            .outgoing
                            .try_send(cancel_request_message(json!(client_request_id)))
                            .ok();
                    }
                    let is_permission = request.message.get("method").and_then(Value::as_str)
                        == Some("session/request_permission");
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
    }

    impl Harness {
        fn new() -> Self {
            Self::with_config(Config::default())
        }

        fn with_config(config: Config) -> Self {
            let launcher = FakeLauncher::default();
            let (events, _) = smol::channel::unbounded();
            Self {
                router: Router::new("1.0.0".into(), config, Box::new(launcher.clone()), events),
                launcher,
                clients: HashMap::new(),
            }
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
            "_meta": {"zed.dev/acpMux": {"protocol": 1}},
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
        assert_eq!(session.observer, None);
        assert!(session.is_running());
    }

    #[test]
    fn lists_sessions() {
        let mut harness = Harness::new();
        harness.attach(1);
        harness.initialize(1);
        harness.new_session(1, "s1");
        harness.from_agent(notification_message(
            "session/update",
            json!({"sessionId": "s1", "update": {"sessionUpdate": "session_info_update", "title": "Fix it"}}),
        ));
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
    fn a_new_client_takes_over_the_session_and_its_permission_requests() {
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
        assert_eq!(
            harness.client_messages(1),
            vec![cancel_request_message(json!(1))]
        );
        let messages = harness.client_messages(2);
        assert_eq!(
            messages.last(),
            Some(&with_id(permission_request(json!(11), "s1"), json!(1)))
        );

        harness.update("s1", "only for the new observer");
        assert!(harness.client_messages(1).is_empty());
        assert_eq!(harness.client_messages(2).len(), 1);

        // A late answer from the displaced client is ignored.
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
            agents: None,
            auto_approve: vec!["edit".to_string()],
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
}
