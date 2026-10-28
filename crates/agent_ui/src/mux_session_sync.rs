//! Applies explicit acp-mux lifecycle events to this machine's sidebar records.
use std::{rc::Rc, time::Duration};

use acp_thread::{
    AgentConnection, AgentSessionList, AgentSessionListRequest, AgentSessionListResponse,
    SessionListUpdate,
};
use anyhow::{Result, anyhow, ensure};
use futures::{
    FutureExt as _,
    future::{Either, select},
};
use gpui::{App, Entity, Task};
use project::{AgentId, Project};
use remote::RemoteConnectionOptions;
use util::ResultExt as _;

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths};

pub(crate) fn watch(
    connection: &Rc<dyn AgentConnection>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Option<Task<()>> {
    if !connection
        .clone()
        .downcast::<agent_servers::AcpConnection>()?
        .is_hosted_by_mux()
    {
        return None;
    }
    let list = connection.session_list(cx)?;
    let agent_id = connection.agent_id();
    let remote_connection = project.read(cx).remote_connection_options(cx);
    watch_list(&list, agent_id, remote_connection, cx)
}

fn watch_list(
    list: &Rc<dyn AgentSessionList>,
    agent_id: AgentId,
    remote_connection: Option<RemoteConnectionOptions>,
    cx: &mut App,
) -> Option<Task<()>> {
    let updates = list.watch(cx)?;
    // Only weakly retain the connection: a synchronization task must not keep
    // a replaced transport alive. Its event receiver closes with that transport.
    let list = Rc::downgrade(list);
    Some(cx.spawn(async move |cx| {
        let mut retry = true;
        loop {
            if retry {
                let Some(list) = list.upgrade() else {
                    break;
                };
                let request =
                    cx.update(|cx| list.list_sessions(AgentSessionListRequest::default(), cx));
                let timeout = cx.background_executor().timer(Duration::from_secs(15));
                let response = match select(request, timeout).await {
                    Either::Left((response, _)) => response,
                    Either::Right(_) => Err(anyhow!("acp-mux session synchronization timed out")),
                };
                retry = match response {
                    Ok(response) => match snapshot_updates(response) {
                        Ok(snapshot) => {
                            let mut failed = false;
                            for update in snapshot {
                                let task = cx.update(|cx| {
                                    apply_update(update, &agent_id, remote_connection.as_ref(), cx)
                                });
                                if let Some(task) = task {
                                    failed |= task.await.log_err().is_none();
                                }
                            }
                            failed
                        }
                        Err(error) => {
                            log::warn!("Invalid acp-mux synchronization snapshot: {error:#}");
                            true
                        }
                    },
                    Err(error) => {
                        log::warn!("Could not synchronize acp-mux sessions: {error:#}");
                        true
                    }
                };
            }
            // On errors keep receiving live events, and retry the read in five
            // seconds. A successful snapshot needs no permanent polling.
            let retry_timer = cx.background_executor().timer(Duration::from_secs(5));
            futures::pin_mut!(retry_timer);
            loop {
                let next = if retry {
                    match select(updates.recv().boxed_local(), retry_timer.as_mut()).await {
                        Either::Left((next, _)) => next,
                        Either::Right(_) => break,
                    }
                } else {
                    updates.recv().await
                };
                let Ok(update) = next else {
                    return;
                };
                if matches!(update, SessionListUpdate::Refresh) {
                    retry = true;
                    break;
                }
                let task =
                    cx.update(|cx| apply_update(update, &agent_id, remote_connection.as_ref(), cx));
                if let Some(task) = task {
                    retry |= task.await.log_err().is_none();
                }
            }
        }
    }))
}

/// Explicit tombstones are the only authority to delete a desktop history row.
/// A failed, malformed or partial response must never erase local metadata.
fn snapshot_updates(response: AgentSessionListResponse) -> Result<Vec<SessionListUpdate>> {
    let Some(sync) = response
        .meta
        .as_ref()
        .and_then(|meta| meta.get("zed.dev/acpMux"))
        .and_then(|meta| meta.get("sessionSync"))
    else {
        return Ok(Vec::new()); // Older daemon: retain the original behavior.
    };
    ensure!(
        response.next_cursor.is_none()
            && sync.get("complete").and_then(|v| v.as_bool()) == Some(true),
        "incomplete session snapshot"
    );
    let deleted = sync
        .get("deletedSessions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("missing deletedSessions"))?;
    let mut updates = Vec::new();
    for deletion in deleted {
        let id = deletion
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("invalid deleted session id"))?;
        updates.push(SessionListUpdate::SessionDeleted(id.into()));
    }
    for session in response.sessions {
        if session
            .meta
            .as_ref()
            .and_then(|meta| meta.get("zed.dev/acpMux"))
            .and_then(|meta| meta.get("own"))
            .and_then(|v| v.as_bool())
            != Some(true)
        {
            updates.push(SessionListUpdate::SessionCreated(session));
        }
    }
    Ok(updates)
}

fn apply_update(
    update: SessionListUpdate,
    agent_id: &AgentId,
    remote_connection: Option<&RemoteConnectionOptions>,
    cx: &mut App,
) -> Option<Task<anyhow::Result<()>>> {
    let store = ThreadMetadataStore::try_global(cx)?;
    store.update(cx, |store, cx| {
        match update {
            SessionListUpdate::SessionCreated(session) => {
                // Several projects may observe the same agent. Match the server
                // as well as the ID to avoid cross-host edits.
                if store.entries().any(|entry| {
                    &entry.agent_id == agent_id
                        && entry.session_id.as_ref() == Some(&session.session_id)
                        && entry.matches_remote_connection(remote_connection)
                }) {
                    return None;
                }
                let paths = session.work_dirs?;
                store.save(
                    ThreadMetadata {
                        thread_id: ThreadId::new(),
                        session_id: Some(session.session_id),
                        agent_id: agent_id.clone(),
                        title: session.title,
                        title_override: None,
                        updated_at: session.updated_at.unwrap_or_else(chrono::Utc::now),
                        created_at: session.created_at,
                        interacted_at: None,
                        worktree_paths: WorktreePaths::from_folder_paths(&paths),
                        remote_connection: remote_connection.cloned(),
                        archived: false,
                    },
                    cx,
                );
                None
            }
            SessionListUpdate::SessionDeleted(session_id) => {
                // Persist the exclusion even if this client has not imported
                // the session, so a late save or history import cannot revive it.
                let metadata = ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(session_id),
                    agent_id: agent_id.clone(),
                    title: None,
                    title_override: None,
                    updated_at: chrono::Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::default(),
                    remote_connection: remote_connection.cloned(),
                    archived: false,
                };
                Some(store.exclude_session_from_history(&metadata, cx))
            }
            SessionListUpdate::Refresh | SessionListUpdate::SessionInfo { .. } => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::AgentSessionInfo;
    use gpui::TestAppContext;
    use util::path_list::PathList;

    fn creation() -> SessionListUpdate {
        SessionListUpdate::SessionCreated(AgentSessionInfo {
            session_id: "shared-session".into(),
            work_dirs: Some(PathList::new(&[std::path::Path::new("/project")])),
            title: Some("From Android".into()),
            updated_at: None,
            created_at: None,
            meta: None,
        })
    }

    fn snapshot(
        sessions: Vec<AgentSessionInfo>,
        deleted: serde_json::Value,
    ) -> AgentSessionListResponse {
        let mut response = AgentSessionListResponse::new(sessions);
        response.meta = Some(
            serde_json::json!({"zed.dev/acpMux": {"sessionSync": {
                "complete": true, "deletedSessions": deleted
            }}})
            .as_object()
            .unwrap()
            .clone(),
        );
        response
    }

    #[test]
    fn snapshot_requires_explicit_complete_deletions_and_skips_own_creations() {
        assert!(
            snapshot_updates(AgentSessionListResponse::new(Vec::new()))
                .unwrap()
                .is_empty()
        );
        let mut partial = snapshot(Vec::new(), serde_json::json!([{"sessionId":"old"}]));
        partial.next_cursor = Some("next".into());
        assert!(snapshot_updates(partial).is_err());
        assert!(
            snapshot_updates(snapshot(
                Vec::new(),
                serde_json::json!([{"sessionId":"old"}, {}])
            ))
            .is_err()
        );
        let SessionListUpdate::SessionCreated(mut own) = creation() else {
            unreachable!()
        };
        own.meta = Some(
            serde_json::json!({"zed.dev/acpMux":{"own":true}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let updates = snapshot_updates(snapshot(
            vec![own],
            serde_json::json!([{"sessionId":"deleted"}]),
        ))
        .unwrap();
        assert_eq!(updates.len(), 1);
        assert!(
            matches!(&updates[0], SessionListUpdate::SessionDeleted(id) if id.0.as_ref() == "deleted")
        );
    }

    struct FakeList {
        responses: std::cell::RefCell<std::collections::VecDeque<Result<AgentSessionListResponse>>>,
        events: async_channel::Receiver<SessionListUpdate>,
        calls: std::cell::Cell<usize>,
    }
    impl AgentSessionList for FakeList {
        fn list_sessions(
            &self,
            _: AgentSessionListRequest,
            _: &mut App,
        ) -> Task<Result<AgentSessionListResponse>> {
            self.calls.set(self.calls.get() + 1);
            Task::ready(
                self.responses
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected snapshot read"),
            )
        }
        fn watch(&self, _: &mut App) -> Option<async_channel::Receiver<SessionListUpdate>> {
            Some(self.events.clone())
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
            self
        }
    }

    #[gpui::test]
    async fn reconnect_retries_snapshot_without_events_and_preserves_unlisted_history(
        cx: &mut TestAppContext,
    ) {
        crate::conversation_view::tests::init_test(cx);
        let agent: AgentId = "test-agent".into();
        let store = cx.read(ThreadMetadataStore::global);
        cx.update(|cx| {
            apply_update(creation(), &agent, None, cx);
        });
        let SessionListUpdate::SessionCreated(mut new) = creation() else {
            unreachable!()
        };
        new.session_id = "offline-created".into();
        let (_sender, events) = async_channel::unbounded();
        let list = Rc::new(FakeList {
            responses: std::cell::RefCell::new(
                [
                    Err(anyhow!("temporary failure")),
                    Ok(snapshot(vec![new], serde_json::json!([]))),
                    Ok(snapshot(
                        Vec::new(),
                        serde_json::json!([{"sessionId":"shared-session"}]),
                    )),
                ]
                .into(),
            ),
            events,
            calls: std::cell::Cell::new(0),
        });
        let connection: Rc<dyn AgentSessionList> = list.clone();
        let task = cx.update(|cx| watch_list(&connection, agent.clone(), None, cx).unwrap());
        cx.run_until_parked();
        assert_eq!(list.calls.get(), 1);
        assert_eq!(store.read_with(cx, |s, _| s.entries().count()), 1);
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(list.calls.get(), 2);
        // A complete live list omitting old history must retain that history.
        assert_eq!(store.read_with(cx, |s, _| s.entries().count()), 2);
        drop(task);
        let _reconnected = cx.update(|cx| watch_list(&connection, agent, None, cx).unwrap());
        cx.run_until_parked();
        assert_eq!(list.calls.get(), 3);
        assert_eq!(store.read_with(cx, |s, _| s.entries().count()), 1);
        assert!(
            store.read_with(cx, |s, _| s.entries().all(|e| e
                .session_id
                .as_ref()
                .unwrap()
                .0
                .as_ref()
                == "offline-created"))
        );
    }

    #[gpui::test]
    async fn lifecycle_metadata_is_deduplicated_and_deletion_cannot_be_revived(
        cx: &mut TestAppContext,
    ) {
        crate::conversation_view::tests::init_test(cx);
        let agent: AgentId = "test-agent".into();
        let store = cx.read(ThreadMetadataStore::global);
        cx.update(|cx| {
            apply_update(creation(), &agent, None, cx);
            apply_update(creation(), &agent, None, cx);
        });
        assert_eq!(store.read_with(cx, |store, _| store.entries().count()), 1);
        let saved = store.read_with(cx, |store, _| store.entries().next().unwrap().clone());
        assert_eq!(saved.title.as_deref(), Some("From Android"));
        assert!(!saved.archived);
        // The same session id on another agent is independent.
        cx.update(|cx| {
            apply_update(creation(), &"other-agent".into(), None, cx);
        });
        let other_host = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 7 });
        cx.update(|cx| {
            apply_update(creation(), &agent, Some(&other_host), cx);
        });
        let deletion = cx.update(|cx| {
            apply_update(
                SessionListUpdate::SessionDeleted("shared-session".into()),
                &agent,
                None,
                cx,
            )
            .unwrap()
        });
        deletion.await.unwrap();
        assert_eq!(store.read_with(cx, |store, _| store.entries().count()), 2);
        assert!(store.read_with(cx, |store, _| {
            store
                .entries()
                .any(|entry| entry.matches_remote_connection(Some(&other_host)))
        }));
        cx.update(|cx| {
            apply_update(creation(), &agent, None, cx);
        });
        store.update(cx, |store, cx| store.save(saved, cx));
        assert_eq!(store.read_with(cx, |store, _| store.entries().count()), 2);
    }
}
