use serde_json::{Map, Value, json};
use std::{collections::VecDeque, time::SystemTime};

use crate::{
    agent::AgentId,
    control::{SessionState, SessionSummary},
    router::ClientId,
    snapshot::SessionSnapshot,
};

pub struct MuxSession {
    pub session_id: String,
    pub agent: AgentId,
    pub agent_name: String,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: SystemTime,
    pub snapshot: SessionSnapshot,
    pub turn: TurnState,
    pub observer: Option<ClientId>,
    /// Set when the agent process exited. The session can still be replayed.
    pub dead: bool,
    /// Prompts that arrived while a turn was running, sent once it ends.
    pub queued_prompts: VecDeque<QueuedPrompt>,
    modes: Option<Value>,
    models: Option<Value>,
    config_options: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnState {
    Idle { last_stop_reason: Option<String> },
    Running(RunningTurn),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunningTurn {
    pub agent_request_id: i64,
    pub originator: ClientId,
    pub started_at: SystemTime,
    /// Clients that were told this turn is running and must be told when it ends.
    pub notified: Vec<ClientId>,
}

pub struct QueuedPrompt {
    pub client: ClientId,
    pub request_id: Value,
    pub params: Value,
}

impl MuxSession {
    pub fn new(
        session_id: String,
        agent: AgentId,
        agent_name: String,
        cwd: String,
        observer: Option<ClientId>,
    ) -> Self {
        Self {
            session_id,
            agent,
            agent_name,
            cwd,
            title: None,
            updated_at: SystemTime::now(),
            snapshot: SessionSnapshot::default(),
            turn: TurnState::Idle {
                last_stop_reason: None,
            },
            observer,
            dead: false,
            queued_prompts: VecDeque::new(),
            modes: None,
            models: None,
            config_options: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.turn, TurnState::Running(_))
    }

    pub fn running_turn_mut(&mut self) -> Option<&mut RunningTurn> {
        match &mut self.turn {
            TurnState::Running(turn) => Some(turn),
            TurnState::Idle { .. } => None,
        }
    }

    pub fn record_update(&mut self, update: &Value) {
        self.updated_at = SystemTime::now();
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("session_info_update") => {
                if let Some(title) = update.get("title") {
                    self.title = title.as_str().map(str::to_string);
                }
            }
            Some("current_mode_update") => {
                if let (Some(modes), Some(mode_id)) = (
                    self.modes.as_mut().and_then(Value::as_object_mut),
                    update.get("currentModeId"),
                ) {
                    modes.insert("currentModeId".into(), mode_id.clone());
                }
            }
            Some("config_option_update") => {
                if let Some(config_options) = update.get("configOptions") {
                    self.config_options = Some(config_options.clone());
                }
            }
            _ => {}
        }
        self.snapshot.apply(update.clone());
    }

    /// Records the user's prompt so that replaying the session shows what was asked.
    pub fn record_prompt(&mut self, params: &Value) {
        let message_id = params.get("messageId").cloned();
        let Some(blocks) = params.get("prompt").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            let mut update = json!({
                "sessionUpdate": "user_message_chunk",
                "content": block,
            });
            if let (Some(message_id), Some(update)) = (&message_id, update.as_object_mut()) {
                update.insert("messageId".into(), message_id.clone());
            }
            self.record_update(&update);
        }
    }

    /// Remembers the modes, models and config options from a `session/new` or
    /// `session/load` response, so later loads can answer with them.
    pub fn set_session_config(&mut self, result: &Value) {
        if let Some(modes) = result.get("modes").filter(|modes| !modes.is_null()) {
            self.modes = Some(modes.clone());
        }
        if let Some(models) = result.get("models").filter(|models| !models.is_null()) {
            self.models = Some(models.clone());
        }
        if let Some(config_options) = result
            .get("configOptions")
            .filter(|config_options| !config_options.is_null())
        {
            self.config_options = Some(config_options.clone());
        }
    }

    pub fn set_current_mode(&mut self, mode_id: Value) {
        if let Some(modes) = self.modes.as_mut().and_then(Value::as_object_mut) {
            modes.insert("currentModeId".into(), mode_id);
        }
    }

    pub fn set_current_model(&mut self, model_id: Value) {
        if let Some(models) = self.models.as_mut().and_then(Value::as_object_mut) {
            models.insert("currentModelId".into(), model_id);
        }
    }

    pub fn set_config_options(&mut self, config_options: Value) {
        self.config_options = Some(config_options);
    }

    pub fn load_response(&self) -> Value {
        let mut response = Map::new();
        if let Some(modes) = &self.modes {
            response.insert("modes".into(), modes.clone());
        }
        if let Some(models) = &self.models {
            response.insert("models".into(), models.clone());
        }
        if let Some(config_options) = &self.config_options {
            response.insert("configOptions".into(), config_options.clone());
        }
        Value::Object(response)
    }

    pub fn summary(&self, waiting_for_permission: bool) -> SessionSummary {
        let state = if self.dead {
            SessionState::Dead
        } else if waiting_for_permission {
            SessionState::WaitingForPermission
        } else if self.is_running() {
            SessionState::Running
        } else {
            SessionState::Idle
        };
        SessionSummary {
            session_id: self.session_id.clone(),
            agent: self.agent_name.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            state,
            attached: self.observer.is_some(),
            updated_at: format_timestamp(self.updated_at),
        }
    }

    pub fn list_entry(&self) -> Value {
        let mut entry = json!({
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "updatedAt": format_timestamp(self.updated_at),
        });
        if let (Some(title), Some(entry)) = (&self.title, entry.as_object_mut()) {
            entry.insert("title".into(), Value::String(title.clone()));
        }
        entry
    }
}

pub fn format_timestamp(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn load_response_tracks_mode_and_config_changes() {
        let mut session = MuxSession::new("s".into(), 1, "agent".into(), "/p".into(), None);
        session.set_session_config(&json!({
            "sessionId": "s",
            "modes": {"currentModeId": "ask", "availableModes": [{"id": "ask"}, {"id": "code"}]},
            "configOptions": [{"id": "model", "currentValue": "a"}],
        }));
        session.record_update(
            &json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}),
        );
        session.record_update(&json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{"id": "model", "currentValue": "b"}],
        }));
        session.record_update(&json!({"sessionUpdate": "session_info_update", "title": "Title"}));

        assert_eq!(
            session.load_response(),
            json!({
                "modes": {"currentModeId": "code", "availableModes": [{"id": "ask"}, {"id": "code"}]},
                "configOptions": [{"id": "model", "currentValue": "b"}],
            })
        );
        assert_eq!(session.title.as_deref(), Some("Title"));
    }

    #[test]
    fn records_prompts_as_user_message_chunks() {
        let mut session = MuxSession::new("s".into(), 1, "agent".into(), "/p".into(), None);
        session.record_prompt(&json!({
            "sessionId": "s",
            "prompt": [{"type": "text", "text": "hello "}, {"type": "text", "text": "there"}],
        }));

        assert_eq!(
            session.snapshot.updates().cloned().collect::<Vec<_>>(),
            vec![json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "hello there"},
            })]
        );
    }
}
