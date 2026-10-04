//! Last known window, persisted in `DATA_DIR/state.json`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Last time this service sent a starter message successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_start: Option<DateTime<Utc>>,
    /// When the last known window resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// Where `resets_at` came from: `local` (estimated after a start) or `oauth-usage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub trait StateStore {
    /// Missing or unreadable state is `None`; it is simply rewritten on the next save.
    fn load(&self) -> Option<State>;
    fn save(&self, state: &State) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct FileStateStore {
    path: PathBuf,
}

impl FileStateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl StateStore for FileStateStore {
    fn load(&self) -> Option<State> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "cannot read state file; ignoring it");
                return None;
            }
        };
        match serde_json::from_str(&text) {
            Ok(state) => Some(state),
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "corrupt state file; ignoring it");
                None
            }
        }
    }

    /// Writes to a temporary file and renames it, so a crash never leaves a half-written file.
    fn save(&self, state: &State) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut file, state)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)
        };
        write().with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;

    use super::*;

    /// In-memory store for scheduler tests.
    #[derive(Default)]
    pub struct MemoryStateStore {
        pub state: RefCell<Option<State>>,
    }

    impl MemoryStateStore {
        pub fn with(state: State) -> Self {
            Self {
                state: RefCell::new(Some(state)),
            }
        }

        pub fn get(&self) -> Option<State> {
            self.state.borrow().clone()
        }
    }

    impl StateStore for MemoryStateStore {
        fn load(&self) -> Option<State> {
            self.get()
        }

        fn save(&self, state: &State) -> Result<()> {
            *self.state.borrow_mut() = Some(state.clone());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(dir.path().join("state.json"));
        assert_eq!(store.load(), None);

        let state = State {
            last_start: Some(utc("2026-10-03T07:00:04Z")),
            resets_at: Some(utc("2026-10-03T12:00:04Z")),
            source: Some("local".into()),
        };
        store.save(&state).unwrap();
        assert_eq!(store.load(), Some(state));
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[test]
    fn reads_the_documented_format() {
        let state: State = serde_json::from_str(
            r#"{ "last_start": "2026-10-03T07:00:04Z", "resets_at": "2026-10-03T12:00:04Z",
                 "source": "local" }"#,
        )
        .unwrap();
        assert_eq!(state.resets_at, Some(utc("2026-10-03T12:00:04Z")));
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{ not json").unwrap();
        let store = FileStateStore::new(path);
        assert_eq!(store.load(), None);
        store.save(&State::default()).unwrap();
        assert_eq!(store.load(), Some(State::default()));
    }
}
