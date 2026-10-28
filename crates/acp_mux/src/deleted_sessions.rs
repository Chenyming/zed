//! Confirmed deletions survive daemon restarts. Absence from the live session
//! map is never evidence that an agent's saved history was deleted.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::OpenOptionsExt as _,
    path::PathBuf,
};

#[derive(Default)]
pub(crate) struct DeletedSessions {
    pub entries: BTreeSet<(String, String)>,
    path: Option<PathBuf>,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredDeletions {
    version: u32,
    sessions: BTreeSet<(String, String)>,
}

impl DeletedSessions {
    pub fn open(path: PathBuf) -> Result<Self> {
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                let stored: StoredDeletions = serde_json::from_slice(&bytes)
                    .context("reading acp-mux confirmed session deletions")?;
                ensure!(
                    stored.version == 1,
                    "unsupported session deletion store version"
                );
                stored.sessions
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(error).context("reading acp-mux session deletion store"),
        };
        Ok(Self {
            entries,
            path: Some(path),
            dirty: false,
        })
    }

    pub fn record(&mut self, agent: String, session_id: String) -> Result<()> {
        // Retain the real outcome in memory even if the disk write fails. The
        // caller reports uncertainty instead of acknowledging durable success.
        self.dirty |= self.entries.insert((agent, session_id));
        self.persist()
    }

    pub fn persist(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().context("deletion store has no parent")?;
        fs::create_dir_all(parent)?;
        let staging = path.with_extension("json.pending");
        let bytes = serde_json::to_vec(&StoredDeletions {
            version: 1,
            sessions: self.entries.clone(),
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&staging, path)?;
        fs::File::open(parent)?.sync_all()?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_deletions_survive_restart_and_keep_agent_scope() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deleted.json");
        let mut store = DeletedSessions::open(path.clone()).unwrap();
        store.record("agent-a".into(), "same-id".into()).unwrap();
        store.record("agent-a".into(), "same-id".into()).unwrap();
        let restored = DeletedSessions::open(path).unwrap();
        assert_eq!(restored.entries.len(), 1);
        assert!(
            !restored
                .entries
                .contains(&("agent-b".into(), "same-id".into()))
        );
    }

    #[test]
    fn corruption_and_write_errors_are_not_reported_as_empty_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deleted.json");
        fs::write(&path, b"broken").unwrap();
        assert!(DeletedSessions::open(path).is_err());
        let path = directory.path().join("blocked");
        let mut store = DeletedSessions::open(path.clone()).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.record("agent".into(), "id".into()).is_err());
        assert!(store.entries.contains(&("agent".into(), "id".into())));
    }
}
