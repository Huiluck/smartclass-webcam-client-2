//! The capture parameters the pipeline was last switched to.
//!
//! A deliberate parameter switch outlives both the connection that carried it and the
//! process: the last set the source accepted is written next to the client's state, and
//! the next session starts from it instead of from the values the registration announces.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use scwc_protocol::Codec;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::events::{CoreEvent, Emit};

/// Suffix of the temporary file a set is written to before it replaces the state file.
const TEMP_SUFFIX: &str = ".tmp";

/// One capture-parameter set: everything a switch can move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedParams {
    /// Protocol resolution, for example `1920x1080`.
    pub(crate) resolution: String,
    /// Frame rate in frames per second.
    pub(crate) fps: u32,
    /// Codec the frames are encoded with.
    pub(crate) codec: Codec,
    /// Encoder bitrate in kilobits per second.
    pub(crate) bitrate_kbps: u32,
}

/// Where the applied parameter set lives.
///
/// On disk when the caller configured a state path — which is what makes a deliberate
/// switch survive a restart — and always in memory, so a session start can tell a real
/// reset from a request that only repeats what the pipeline already runs.
pub(crate) struct ParamStore {
    path: Option<PathBuf>,
    events: Option<UnboundedSender<CoreEvent>>,
    applied: Mutex<Option<PersistedParams>>,
}

impl ParamStore {
    /// Builds a store for `path`; `None` keeps it stateless — nothing is read or written.
    pub(crate) fn new(path: Option<PathBuf>, events: Option<UnboundedSender<CoreEvent>>) -> Self {
        Self {
            path,
            events,
            applied: Mutex::new(None),
        }
    }

    /// The set to start a session from: the persisted one, or `None` when no state path is
    /// configured, no file has been written yet, or the file cannot be used.
    ///
    /// A state file that is unreadable or does not decode is reported and ignored: the
    /// session falls back to the announced parameters rather than failing to start.
    pub(crate) fn persisted(&self) -> Option<PersistedParams> {
        let path = self.path.as_ref()?;
        match read(path) {
            Ok(params) => params,
            Err(error) => {
                let message = format!(
                    "ignoring the unusable capture-parameter state {}: {error}",
                    path.display()
                );
                warn!(path = %path.display(), %error, "ignoring an unusable capture-parameter state");
                self.events.emit(CoreEvent::Warn { message });
                None
            }
        }
    }

    /// The set the source accepted last in this process, if any.
    pub(crate) fn applied(&self) -> Option<PersistedParams> {
        self.applied
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Remembers a set the source accepted without writing it to disk.
    ///
    /// Used when a session start re-applies parameters nobody switched to: they are what
    /// the pipeline runs now, but writing the announced defaults back would freeze them.
    pub(crate) fn note_applied(&self, params: &PersistedParams) {
        *self
            .applied
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(params.clone());
    }

    /// Remembers a deliberately switched set and persists it, so the next session starts
    /// from it.
    ///
    /// A store without a state path only remembers it in memory; a write that fails is
    /// reported and otherwise ignored, because losing the memory of a switch must not fail
    /// the running client.
    pub(crate) fn record_applied(&self, params: &PersistedParams) {
        self.note_applied(params);

        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Err(error) = write(path, params) {
            let message = format!(
                "could not persist the capture parameters to {}: {error}",
                path.display()
            );
            warn!(path = %path.display(), %error, "could not persist the capture parameters");
            self.events.emit(CoreEvent::Warn { message });
        }
    }
}

/// Reads the persisted set: `Ok(None)` when there is no file yet.
fn read(path: &Path) -> Result<Option<PersistedParams>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };

    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Writes `params` atomically: a temporary file in the same directory, then a rename over
/// the state file, so a reader never sees a half-written set.
fn write(path: &Path, params: &PersistedParams) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }

    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(TEMP_SUFFIX);
    let temp = PathBuf::from(temp_name);

    let json = serde_json::to_vec(params).map_err(|error| error.to_string())?;
    fs::write(&temp, json).map_err(|error| error.to_string())?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{ParamStore, PersistedParams};
    use crate::media::test_support::TempDir;
    use scwc_protocol::Codec;

    fn params() -> PersistedParams {
        PersistedParams {
            resolution: "1280x720".to_owned(),
            fps: 15,
            codec: Codec::H265,
            bitrate_kbps: 30_000,
        }
    }

    #[test]
    fn round_trips_a_persisted_set() {
        let dir = TempDir::new("params-round-trip");
        let path = dir.path().join("capture-params.json");

        let store = ParamStore::new(Some(path.clone()), None);
        store.record_applied(&params());

        assert_eq!(store.applied(), Some(params()));
        let reloaded = ParamStore::new(Some(path.clone()), None);
        assert_eq!(reloaded.persisted(), Some(params()));
        assert_eq!(reloaded.applied(), None, "a fresh store knows nothing yet");
        assert!(
            !path.with_file_name("capture-params.json.tmp").exists(),
            "the temporary file must not survive the rename"
        );
    }

    #[test]
    fn ignores_a_missing_state_file() {
        let dir = TempDir::new("params-missing");
        let path = dir.path().join("capture-params.json");

        assert_eq!(ParamStore::new(Some(path), None).persisted(), None);
        assert_eq!(ParamStore::new(None, None).persisted(), None);
    }

    #[test]
    fn ignores_a_corrupt_state_file() {
        let dir = TempDir::new("params-corrupt");
        let path = dir.path().join("capture-params.json");
        std::fs::write(&path, "{\"resolution\": \"1920x1080\"").unwrap();

        assert_eq!(ParamStore::new(Some(path.clone()), None).persisted(), None);
        assert!(path.exists(), "the unusable file is left alone");
    }
}
