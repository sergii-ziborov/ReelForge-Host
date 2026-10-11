//! Reuse Weavatrix Memory's checksummed, locked, versioned event journal.

use super::model::State;
use crate::{HostError, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use weavatrix_memory::{
    AgentId, Codec, EventId, EventStore, ExpectedVersion, FileEventStore, FileStoreOptions,
    MemoryError, NewEvent, SessionId, StoredEvent, StreamId, Timestamp,
};

static JOURNAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy)]
struct Json;
impl Codec<StoredEvent<State>> for Json {
    fn encode(&self, value: &StoredEvent<State>) -> weavatrix_memory::Result<Vec<u8>> {
        serde_json::to_vec(value).map_err(|e| MemoryError::Codec {
            message: e.to_string(),
        })
    }
    fn decode(&self, bytes: &[u8]) -> weavatrix_memory::Result<StoredEvent<State>> {
        serde_json::from_slice(bytes).map_err(|e| MemoryError::Codec {
            message: e.to_string(),
        })
    }
}

fn open(dir: &Path) -> Result<FileEventStore<State, Json>> {
    FileEventStore::open(
        dir.join("production.events"),
        Json,
        FileStoreOptions::default(),
    )
    .map_err(|e| HostError::message(e.to_string()))
}

pub(super) fn load(dir: &Path) -> Result<State> {
    let _guard = JOURNAL_LOCK
        .lock()
        .map_err(|e| HostError::message(e.to_string()))?;
    open(dir)?
        .load_all(None, usize::MAX)
        .last()
        .map(|e| e.payload.clone())
        .ok_or_else(|| HostError::message("movie has no committed state"))
}

pub(super) fn commit(dir: &Path, state: &mut State, kind: &str) -> Result<()> {
    let _guard = JOURNAL_LOCK
        .lock()
        .map_err(|e| HostError::message(e.to_string()))?;
    let mut store = open(dir)?;
    let stream = StreamId::new("production").map_err(|e| HostError::message(e.to_string()))?;
    let expected = if state.revision == 0 {
        ExpectedVersion::NoStream
    } else {
        ExpectedVersion::Exact(state.revision - 1)
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| HostError::message(e.to_string()))?
        .as_micros();
    let now = Timestamp::from_unix_micros(
        i64::try_from(stamp).map_err(|e| HostError::message(e.to_string()))?,
    );
    let event = NewEvent::new(
        EventId::new(format!("revision-{}", state.revision))
            .map_err(|e| HostError::message(e.to_string()))?,
        kind,
        now,
        now,
        AgentId::new("reelforge-host").map_err(|e| HostError::message(e.to_string()))?,
        SessionId::new(&state.request.id).map_err(|e| HostError::message(e.to_string()))?,
        {
            let mut next = state.clone();
            next.revision += 1;
            next
        },
    )
    .map_err(|e| HostError::message(e.to_string()))?;
    store
        .append_owned(&stream, expected, vec![event])
        .map_err(|e| HostError::message(e.to_string()))?;
    state.revision += 1;
    Ok(())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn file_digest(path: &Path) -> Result<String> {
    Ok(digest(&std::fs::read(path)?))
}

pub(super) fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
