use serde_json::{Map, Value};
use std::collections::HashMap;

const MESSAGE_CHUNK_KINDS: &[&str] = &[
    "user_message_chunk",
    "agent_message_chunk",
    "agent_thought_chunk",
];

const LATEST_ONLY_KINDS: &[&str] = &[
    "plan",
    "available_commands_update",
    "current_mode_update",
    "config_option_update",
    "session_info_update",
    "usage_update",
];

/// The current state of a session expressed as the smallest sequence of
/// `session/update` payloads that reproduces it when replayed.
#[derive(Default, Debug, Clone)]
pub struct SessionSnapshot {
    entries: Vec<Value>,
    tool_call_indices: HashMap<String, usize>,
    latest: Vec<(String, Value)>,
}

impl SessionSnapshot {
    pub fn apply(&mut self, update: Value) {
        let Some(kind) = update_kind(&update).map(str::to_string) else {
            self.entries.push(update);
            return;
        };

        if MESSAGE_CHUNK_KINDS.contains(&kind.as_str()) {
            if let Some(last) = self.entries.last_mut()
                && merge_text_chunk(last, &update)
            {
                return;
            }
            self.entries.push(update);
            return;
        }

        if kind == "tool_call" || kind == "tool_call_update" {
            let Some(tool_call_id) = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                self.entries.push(update);
                return;
            };
            match self
                .tool_call_indices
                .get(&tool_call_id)
                .and_then(|index| self.entries.get_mut(*index))
            {
                Some(existing) => overlay_tool_call(existing, update),
                None => {
                    self.tool_call_indices
                        .insert(tool_call_id, self.entries.len());
                    self.entries.push(update);
                }
            }
            return;
        }

        if LATEST_ONLY_KINDS.contains(&kind.as_str()) {
            match self
                .latest
                .iter_mut()
                .find(|(existing, _)| *existing == kind)
            {
                // Session info updates are patches, so a later update with only
                // `updatedAt` must not erase an earlier title.
                Some((_, existing)) if kind == "session_info_update" => {
                    overlay(existing, update);
                }
                Some((_, existing)) => *existing = update,
                None => self.latest.push((kind, update)),
            }
            return;
        }

        self.entries.push(update);
    }

    /// Updates in replay order: the conversation first, then the latest value
    /// of each piece of session-wide state.
    pub fn updates(&self) -> impl Iterator<Item = &Value> {
        self.entries
            .iter()
            .chain(self.latest.iter().map(|(_, update)| update))
    }
}

fn update_kind(update: &Value) -> Option<&str> {
    update.get("sessionUpdate").and_then(Value::as_str)
}

fn text_content(update: &Value) -> Option<&str> {
    let content = update.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    content.get("text").and_then(Value::as_str)
}

fn merge_text_chunk(last: &mut Value, update: &Value) -> bool {
    if update_kind(last) != update_kind(update) || last.get("messageId") != update.get("messageId")
    {
        return false;
    }
    let (Some(_), Some(appended)) = (text_content(last), text_content(update)) else {
        return false;
    };
    let Some(Value::String(text)) = last
        .get_mut("content")
        .and_then(|content| content.get_mut("text"))
    else {
        return false;
    };
    text.push_str(appended);
    true
}

fn overlay_tool_call(existing: &mut Value, update: Value) {
    let is_full_tool_call = update_kind(&update) == Some("tool_call");
    let Value::Object(fields) = update else {
        return;
    };
    let Some(target) = existing.as_object_mut() else {
        return;
    };
    for (key, value) in fields {
        if key == "sessionUpdate" {
            if is_full_tool_call {
                target.insert(key, value);
            }
            continue;
        }
        if !value.is_null() {
            target.insert(key, value);
        }
    }
}

fn overlay(existing: &mut Value, update: Value) {
    let Value::Object(fields) = update else {
        *existing = update;
        return;
    };
    if !existing.is_object() {
        *existing = Value::Object(Map::new());
    }
    if let Some(target) = existing.as_object_mut() {
        for (key, value) in fields {
            target.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn replay(snapshot: &SessionSnapshot) -> Vec<Value> {
        snapshot.updates().cloned().collect()
    }

    #[test]
    fn merges_adjacent_text_chunks_of_the_same_kind() {
        let mut snapshot = SessionSnapshot::default();
        for text in ["Hel", "lo", " world"] {
            snapshot.apply(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            }));
        }
        snapshot.apply(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "hmm"},
        }));
        snapshot.apply(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "again"},
        }));

        assert_eq!(
            replay(&snapshot),
            vec![
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello world"}}),
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "again"}}),
            ]
        );
    }

    #[test]
    fn keeps_chunks_with_different_message_ids_separate() {
        let mut snapshot = SessionSnapshot::default();
        snapshot.apply(json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": "a",
            "content": {"type": "text", "text": "one"},
        }));
        snapshot.apply(json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": "b",
            "content": {"type": "text", "text": "two"},
        }));
        snapshot.apply(json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": "b",
            "content": {"type": "text", "text": " three"},
        }));

        assert_eq!(
            replay(&snapshot),
            vec![
                json!({"sessionUpdate": "user_message_chunk", "messageId": "a", "content": {"type": "text", "text": "one"}}),
                json!({"sessionUpdate": "user_message_chunk", "messageId": "b", "content": {"type": "text", "text": "two three"}}),
            ]
        );
    }

    #[test]
    fn keeps_non_text_chunks_separate() {
        let mut snapshot = SessionSnapshot::default();
        let image = json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "image", "data": "AAAA", "mimeType": "image/png"},
        });
        snapshot.apply(json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "look"},
        }));
        snapshot.apply(image.clone());
        snapshot.apply(json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "here"},
        }));

        assert_eq!(replay(&snapshot).len(), 3);
        assert_eq!(replay(&snapshot)[1], image);
    }

    #[test]
    fn merges_tool_call_updates_into_the_tool_call() {
        let mut snapshot = SessionSnapshot::default();
        snapshot.apply(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Read file",
            "kind": "read",
            "status": "pending",
            "content": [{"type": "content", "content": {"type": "text", "text": "old"}}],
            "locations": [{"path": "/a"}],
        }));
        snapshot.apply(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "between"},
        }));
        snapshot.apply(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "new"}}],
            "title": null,
        }));

        assert_eq!(
            replay(&snapshot),
            vec![
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "Read file",
                    "kind": "read",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "new"}}],
                    "locations": [{"path": "/a"}],
                }),
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "between"}}),
            ]
        );
    }

    #[test]
    fn keeps_updates_for_unknown_tool_calls() {
        let mut snapshot = SessionSnapshot::default();
        snapshot.apply(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "status": "in_progress",
        }));
        snapshot.apply(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "status": "completed",
        }));

        assert_eq!(
            replay(&snapshot),
            vec![
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed"})
            ]
        );
    }

    #[test]
    fn keeps_only_the_latest_session_wide_state_after_the_conversation() {
        let mut snapshot = SessionSnapshot::default();
        snapshot.apply(json!({"sessionUpdate": "current_mode_update", "currentModeId": "ask"}));
        snapshot.apply(json!({"sessionUpdate": "plan", "entries": [{"content": "a"}]}));
        snapshot.apply(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"},
        }));
        snapshot.apply(json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}));
        snapshot.apply(json!({"sessionUpdate": "plan", "entries": [{"content": "b"}]}));
        snapshot.apply(json!({"sessionUpdate": "session_info_update", "title": "Fix bug"}));
        snapshot.apply(
            json!({"sessionUpdate": "session_info_update", "updatedAt": "2026-01-01T00:00:00Z"}),
        );

        assert_eq!(
            replay(&snapshot),
            vec![
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}),
                json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}),
                json!({"sessionUpdate": "plan", "entries": [{"content": "b"}]}),
                json!({"sessionUpdate": "session_info_update", "title": "Fix bug", "updatedAt": "2026-01-01T00:00:00Z"}),
            ]
        );
    }

    #[test]
    fn keeps_unknown_updates_in_order() {
        let mut snapshot = SessionSnapshot::default();
        let unknown = json!({"sessionUpdate": "_future_update", "payload": [1, 2]});
        let untagged = json!({"something": "else"});
        snapshot.apply(unknown.clone());
        snapshot.apply(untagged.clone());
        snapshot.apply(unknown.clone());

        assert_eq!(replay(&snapshot), vec![unknown.clone(), untagged, unknown]);
    }
}
