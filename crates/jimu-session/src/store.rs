use crate::error::SessionError;
use crate::types::{SessionEntry, SessionHeader};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Single line record serialized to JSONL storage.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum SessionRecord {
    Header(SessionHeader),
    Entry(SessionEntry),
}

/// Append-only persistence backend for [`crate::SessionManager`].
pub trait SessionStore: Send + Sync {
    /// Load existing header (if initialized) and all appended entries in write order.
    fn load(&self) -> Result<(Option<SessionHeader>, Vec<SessionEntry>), SessionError>;

    /// Persist the session header record.
    fn write_header(&self, header: &SessionHeader) -> Result<(), SessionError>;

    /// Append a single session entry record.
    fn append_entry(&self, entry: &SessionEntry) -> Result<(), SessionError>;
}

/// Thread-safe in-memory [`SessionStore`] for ephemeral sessions and tests.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    inner: Mutex<(Option<SessionHeader>, Vec<SessionEntry>)>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemorySessionStore {
    fn load(&self) -> Result<(Option<SessionHeader>, Vec<SessionEntry>), SessionError> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok((guard.0.clone(), guard.1.clone()))
    }

    fn write_header(&self, header: &SessionHeader) -> Result<(), SessionError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.0 = Some(header.clone());
        Ok(())
    }

    fn append_entry(&self, entry: &SessionEntry) -> Result<(), SessionError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.1.push(entry.clone());
        Ok(())
    }
}

/// Strict LF-delimited (`\n`) append-only JSONL file [`SessionStore`].
#[derive(Debug)]
pub struct JsonlSessionStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlSessionStore {
    /// Create or open a JSONL session store at `path`. Parent directories are created on write.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append_record(&self, record: &SessionRecord) -> Result<(), SessionError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }
}

impl SessionStore for JsonlSessionStore {
    fn load(&self) -> Result<(Option<SessionHeader>, Vec<SessionEntry>), SessionError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.path.exists() {
            return Ok((None, Vec::new()));
        }
        let file = fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut header: Option<SessionHeader> = None;
        let mut entries = Vec::new();

        for (line_idx, line_res) in reader.lines().enumerate() {
            let line = line_res?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let record: SessionRecord = serde_json::from_str(trimmed).map_err(|err| {
                SessionError::InvalidSessionFile(format!(
                    "line {}: failed to parse JSONL record: {err}",
                    line_idx + 1
                ))
            })?;
            match record {
                SessionRecord::Header(h) => {
                    if header.is_none() {
                        header = Some(h);
                    }
                }
                SessionRecord::Entry(e) => {
                    entries.push(e);
                }
            }
        }

        Ok((header, entries))
    }

    fn write_header(&self, header: &SessionHeader) -> Result<(), SessionError> {
        self.append_record(&SessionRecord::Header(header.clone()))
    }

    fn append_entry(&self, entry: &SessionEntry) -> Result<(), SessionError> {
        self.append_record(&SessionRecord::Entry(entry.clone()))
    }
}
