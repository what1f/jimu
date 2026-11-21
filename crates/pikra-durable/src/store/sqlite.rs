use crate::error::DurableError;
use crate::store::DurableStore;
use crate::types::{
    CheckpointMutation, CommitBatch, CommitReceipt, CompletedToolRecord, ConversationRecord,
    ConversationStatus, DocumentRecord, SignalDeliveryResult, SignalRecord, SubmissionRecord,
    TaskCheckpoint,
};
use pikra_session::SessionEntry;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    conversation_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    head_entry_id TEXT,
    status TEXT NOT NULL,
    active_model_json TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS transcript_entries (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    entry_id TEXT NOT NULL,
    parent_id TEXT,
    entry_json TEXT NOT NULL,
    PRIMARY KEY (conversation_id, seq),
    UNIQUE (conversation_id, entry_id)
);

CREATE TABLE IF NOT EXISTS submissions (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    submission_id TEXT NOT NULL,
    user_entry_id TEXT,
    state_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, submission_id)
);

CREATE TABLE IF NOT EXISTS task_checkpoints (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    checkpoint_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS completed_tools (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    tool_call_id TEXT NOT NULL,
    assistant_entry_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    record_json TEXT NOT NULL,
    completed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, tool_call_id)
);

CREATE INDEX IF NOT EXISTS idx_completed_tools_assistant
    ON completed_tools(conversation_id, assistant_entry_id);

CREATE TABLE IF NOT EXISTS signals (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    signal_id TEXT NOT NULL,
    signal_key TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL,
    consumed_at_ms INTEGER,
    seq INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, signal_id)
);

CREATE INDEX IF NOT EXISTS idx_signals_lookup
    ON signals(conversation_id, signal_key, consumed_at_ms, seq);

CREATE TABLE IF NOT EXISTS documents (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    doc_key TEXT NOT NULL,
    revision INTEGER NOT NULL,
    value_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, doc_key)
);
"#;

type RawConversationRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    i64,
    i64,
);
type RawSignalRow = (String, String, String, String, i64, Option<i64>);

/// Single-node SQLite WAL [`DurableStore`] with atomic `IMMEDIATE` transactions.
pub struct SqliteDurableStore {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl SqliteDurableStore {
    /// Open or create a durable SQLite database file at `path`.
    ///
    /// Newly created database files are restricted to Unix mode `0600`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DurableError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let existed = path.exists();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Self::configure_and_init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path),
        })
    }

    /// Open an isolated `:memory:` SQLite database.
    pub fn open_in_memory() -> Result<Self, DurableError> {
        let conn = Connection::open_in_memory()?;
        Self::configure_and_init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn configure_and_init(conn: &Connection) -> Result<(), DurableError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(())
    }

    fn parse_conversation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawConversationRow> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        ))
    }

    fn decode_conversation(tuple: RawConversationRow) -> Result<ConversationRecord, DurableError> {
        let (
            conversation_id,
            session_id,
            head_entry_id,
            status_str,
            active_model_json,
            system_prompt,
            created_at_ms,
            updated_at_ms,
        ) = tuple;
        let status = ConversationStatus::parse(&status_str).ok_or_else(|| {
            DurableError::InvalidState(format!("unknown conversation status: {status_str}"))
        })?;
        let active_model = serde_json::from_str(&active_model_json)?;
        Ok(ConversationRecord {
            conversation_id,
            session_id,
            head_entry_id,
            status,
            active_model,
            system_prompt,
            created_at_ms: created_at_ms.max(0) as u64,
            updated_at_ms: updated_at_ms.max(0) as u64,
        })
    }

    fn decode_signal(tuple: RawSignalRow) -> Result<SignalRecord, DurableError> {
        let (conversation_id, signal_id, signal_key, payload_json, received_at_ms, consumed_at_ms) =
            tuple;
        let payload = serde_json::from_str(&payload_json)?;
        Ok(SignalRecord {
            conversation_id,
            signal_id,
            signal_key,
            payload,
            received_at_ms: received_at_ms.max(0) as u64,
            consumed_at_ms: consumed_at_ms.map(|v| v.max(0) as u64),
        })
    }
}

impl DurableStore for SqliteDurableStore {
    fn create_conversation(
        &self,
        mut record: ConversationRecord,
        initial_entries: Vec<SessionEntry>,
    ) -> Result<CommitReceipt, DurableError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let db_tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let exists: Option<i64> = db_tx
            .query_row(
                "SELECT 1 FROM conversations WHERE conversation_id = ?1",
                params![&record.conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            return Err(DurableError::ConversationAlreadyExists(
                record.conversation_id,
            ));
        }

        if let Some(last) = initial_entries.last() {
            record.head_entry_id = Some(last.id.clone());
        }
        let model_json = serde_json::to_string(&record.active_model)?;
        db_tx.execute(
            "INSERT INTO conversations (
                conversation_id, session_id, head_entry_id, status,
                active_model_json, system_prompt, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &record.conversation_id,
                &record.session_id,
                &record.head_entry_id,
                record.status.as_str(),
                &model_json,
                &record.system_prompt,
                record.created_at_ms as i64,
                record.updated_at_ms as i64,
            ],
        )?;

        let mut seq = 0u64;
        for entry in &initial_entries {
            seq += 1;
            let entry_json = serde_json::to_string(entry)?;
            db_tx.execute(
                "INSERT INTO transcript_entries (
                    conversation_id, seq, entry_id, parent_id, entry_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &record.conversation_id,
                    seq as i64,
                    &entry.id,
                    &entry.parent_id,
                    &entry_json,
                ],
            )?;
        }

        db_tx.commit()?;
        Ok(CommitReceipt {
            conversation_id: record.conversation_id,
            head_entry_id: record.head_entry_id,
            last_seq: seq,
            document_revisions: Vec::new(),
        })
    }

    fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT conversation_id, session_id, head_entry_id, status,
                        active_model_json, system_prompt, created_at_ms, updated_at_ms
                 FROM conversations WHERE conversation_id = ?1",
                params![conversation_id],
                Self::parse_conversation_row,
            )
            .optional()?;
        row.map(Self::decode_conversation).transpose()
    }

    fn list_conversations(&self) -> Result<Vec<ConversationRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT conversation_id, session_id, head_entry_id, status,
                    active_model_json, system_prompt, created_at_ms, updated_at_ms
             FROM conversations ORDER BY created_at_ms ASC, conversation_id ASC",
        )?;
        let rows = stmt.query_map([], Self::parse_conversation_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(Self::decode_conversation(r?)?);
        }
        Ok(out)
    }

    fn load_entries(&self, conversation_id: &str) -> Result<Vec<SessionEntry>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM conversations WHERE conversation_id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(DurableError::ConversationNotFound(
                conversation_id.to_string(),
            ));
        }

        let mut stmt = conn.prepare(
            "SELECT entry_json FROM transcript_entries
             WHERE conversation_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |r| r.get::<_, String>(0))?;
        let mut entries = Vec::new();
        for json_res in rows {
            let entry: SessionEntry = serde_json::from_str(&json_res?)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    fn get_submission(
        &self,
        conversation_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row: Option<(Option<String>, String, i64, i64)> = conn
            .query_row(
                "SELECT user_entry_id, state_json, created_at_ms, updated_at_ms
                 FROM submissions WHERE conversation_id = ?1 AND submission_id = ?2",
                params![conversation_id, submission_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((user_entry_id, state_json, created_at_ms, updated_at_ms)) => {
                Ok(Some(SubmissionRecord {
                    conversation_id: conversation_id.to_string(),
                    submission_id: submission_id.to_string(),
                    user_entry_id,
                    state: serde_json::from_str(&state_json)?,
                    created_at_ms: created_at_ms.max(0) as u64,
                    updated_at_ms: updated_at_ms.max(0) as u64,
                }))
            }
        }
    }

    fn get_checkpoint(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TaskCheckpoint>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let json: Option<String> = conn
            .query_row(
                "SELECT checkpoint_json FROM task_checkpoints WHERE conversation_id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(DurableError::from)
    }

    fn get_completed_tool(
        &self,
        conversation_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<CompletedToolRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let json: Option<String> = conn
            .query_row(
                "SELECT record_json FROM completed_tools
                 WHERE conversation_id = ?1 AND tool_call_id = ?2",
                params![conversation_id, tool_call_id],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(DurableError::from)
    }

    fn list_completed_tools(
        &self,
        conversation_id: &str,
        assistant_entry_id: &str,
    ) -> Result<Vec<CompletedToolRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT record_json FROM completed_tools
             WHERE conversation_id = ?1 AND assistant_entry_id = ?2
             ORDER BY completed_at_ms ASC, tool_call_id ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id, assistant_entry_id], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for json_res in rows {
            out.push(serde_json::from_str(&json_res?)?);
        }
        Ok(out)
    }

    fn deliver_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        signal_id: &str,
        payload: Value,
        now_ms: u64,
    ) -> Result<SignalDeliveryResult, DurableError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let db_tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let exists: Option<i64> = db_tx
            .query_row(
                "SELECT 1 FROM conversations WHERE conversation_id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(DurableError::ConversationNotFound(
                conversation_id.to_string(),
            ));
        }

        let existing = db_tx
            .query_row(
                "SELECT conversation_id, signal_id, signal_key, payload_json, received_at_ms, consumed_at_ms
                 FROM signals WHERE conversation_id = ?1 AND signal_id = ?2",
                params![conversation_id, signal_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;

        if let Some(tuple) = existing {
            let rec = Self::decode_signal(tuple)?;
            db_tx.commit()?;
            return Ok(SignalDeliveryResult::Duplicate(rec));
        }

        let next_seq: i64 = db_tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM signals WHERE conversation_id = ?1",
            params![conversation_id],
            |r| r.get(0),
        )?;
        let payload_json = serde_json::to_string(&payload)?;
        db_tx.execute(
            "INSERT INTO signals (
                conversation_id, signal_id, signal_key, payload_json, received_at_ms, consumed_at_ms, seq
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
            params![
                conversation_id,
                signal_id,
                signal_key,
                &payload_json,
                now_ms as i64,
                next_seq,
            ],
        )?;
        db_tx.commit()?;

        Ok(SignalDeliveryResult::Delivered(SignalRecord {
            conversation_id: conversation_id.to_string(),
            signal_id: signal_id.to_string(),
            signal_key: signal_key.to_string(),
            payload,
            received_at_ms: now_ms,
            consumed_at_ms: None,
        }))
    }

    fn peek_unconsumed_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
    ) -> Result<Option<SignalRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT conversation_id, signal_id, signal_key, payload_json, received_at_ms, consumed_at_ms
                 FROM signals
                 WHERE conversation_id = ?1 AND signal_key = ?2 AND consumed_at_ms IS NULL
                 ORDER BY seq ASC LIMIT 1",
                params![conversation_id, signal_key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(Self::decode_signal).transpose()
    }

    fn list_signals(&self, conversation_id: &str) -> Result<Vec<SignalRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT conversation_id, signal_id, signal_key, payload_json, received_at_ms, consumed_at_ms
             FROM signals WHERE conversation_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(Self::decode_signal(r?)?);
        }
        Ok(out)
    }

    fn get_document(
        &self,
        conversation_id: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT revision, value_json FROM documents
                 WHERE conversation_id = ?1 AND doc_key = ?2",
                params![conversation_id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((rev, json)) => Ok(Some((rev.max(0) as u64, serde_json::from_str(&json)?))),
        }
    }

    fn list_documents(&self, conversation_id: &str) -> Result<Vec<DocumentRecord>, DurableError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT doc_key, revision, value_json, updated_at_ms
             FROM documents WHERE conversation_id = ?1 ORDER BY doc_key ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (key, rev, json, updated_at_ms) = r?;
            out.push(DocumentRecord {
                conversation_id: conversation_id.to_string(),
                key,
                revision: rev.max(0) as u64,
                value: serde_json::from_str(&json)?,
                updated_at_ms: updated_at_ms.max(0) as u64,
            });
        }
        Ok(out)
    }

    fn commit(&self, tx: CommitBatch) -> Result<CommitReceipt, DurableError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let db_tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(conv) = &tx.upsert_conversation {
            let model_json = serde_json::to_string(&conv.active_model)?;
            db_tx.execute(
                "INSERT INTO conversations (
                    conversation_id, session_id, head_entry_id, status,
                    active_model_json, system_prompt, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(conversation_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    head_entry_id = excluded.head_entry_id,
                    status = excluded.status,
                    active_model_json = excluded.active_model_json,
                    system_prompt = excluded.system_prompt,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    &conv.conversation_id,
                    &conv.session_id,
                    &conv.head_entry_id,
                    conv.status.as_str(),
                    &model_json,
                    &conv.system_prompt,
                    conv.created_at_ms as i64,
                    tx.now_ms as i64,
                ],
            )?;
        }

        let current_head: Option<Option<String>> = db_tx
            .query_row(
                "SELECT head_entry_id FROM conversations WHERE conversation_id = ?1",
                params![&tx.conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        let mut head_entry_id = match current_head {
            Some(h) => h,
            None => return Err(DurableError::ConversationNotFound(tx.conversation_id)),
        };

        if let Some(status) = tx.update_status {
            db_tx.execute(
                "UPDATE conversations SET status = ?1, updated_at_ms = ?2 WHERE conversation_id = ?3",
                params![status.as_str(), tx.now_ms as i64, &tx.conversation_id],
            )?;
        }

        if let Some(model) = &tx.update_model {
            let model_json = serde_json::to_string(model)?;
            db_tx.execute(
                "UPDATE conversations SET active_model_json = ?1, updated_at_ms = ?2 WHERE conversation_id = ?3",
                params![&model_json, tx.now_ms as i64, &tx.conversation_id],
            )?;
        }

        let mut last_seq: i64 = db_tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM transcript_entries WHERE conversation_id = ?1",
            params![&tx.conversation_id],
            |r| r.get(0),
        )?;

        if !tx.append_entries.is_empty() {
            for entry in &tx.append_entries {
                last_seq += 1;
                let entry_json = serde_json::to_string(entry)?;
                db_tx.execute(
                    "INSERT INTO transcript_entries (
                        conversation_id, seq, entry_id, parent_id, entry_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        &tx.conversation_id,
                        last_seq,
                        &entry.id,
                        &entry.parent_id,
                        &entry_json,
                    ],
                )?;
                head_entry_id = Some(entry.id.clone());
            }
            db_tx.execute(
                "UPDATE conversations SET head_entry_id = ?1, updated_at_ms = ?2 WHERE conversation_id = ?3",
                params![&head_entry_id, tx.now_ms as i64, &tx.conversation_id],
            )?;
        } else {
            db_tx.execute(
                "UPDATE conversations SET updated_at_ms = ?1 WHERE conversation_id = ?2",
                params![tx.now_ms as i64, &tx.conversation_id],
            )?;
        }

        if let Some(sub) = &tx.upsert_submission {
            let state_json = serde_json::to_string(&sub.state)?;
            db_tx.execute(
                "INSERT INTO submissions (
                    conversation_id, submission_id, user_entry_id, state_json, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(conversation_id, submission_id) DO UPDATE SET
                    user_entry_id = excluded.user_entry_id,
                    state_json = excluded.state_json,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    &sub.conversation_id,
                    &sub.submission_id,
                    &sub.user_entry_id,
                    &state_json,
                    sub.created_at_ms as i64,
                    sub.updated_at_ms as i64,
                ],
            )?;
        }

        match &tx.checkpoint {
            CheckpointMutation::Unchanged => {}
            CheckpointMutation::Set(cp) => {
                let cp_json = serde_json::to_string(cp)?;
                db_tx.execute(
                    "INSERT INTO task_checkpoints (conversation_id, checkpoint_json, updated_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(conversation_id) DO UPDATE SET
                        checkpoint_json = excluded.checkpoint_json,
                        updated_at_ms = excluded.updated_at_ms",
                    params![&tx.conversation_id, &cp_json, tx.now_ms as i64],
                )?;
            }
            CheckpointMutation::Clear => {
                db_tx.execute(
                    "DELETE FROM task_checkpoints WHERE conversation_id = ?1",
                    params![&tx.conversation_id],
                )?;
            }
        }

        for tool_rec in &tx.record_completed_tools {
            let rec_json = serde_json::to_string(tool_rec)?;
            db_tx.execute(
                "INSERT INTO completed_tools (
                    conversation_id, tool_call_id, assistant_entry_id, tool_name, record_json, completed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(conversation_id, tool_call_id) DO NOTHING",
                params![
                    &tool_rec.conversation_id,
                    &tool_rec.tool_call_id,
                    &tool_rec.assistant_entry_id,
                    &tool_rec.tool_name,
                    &rec_json,
                    tool_rec.completed_at_ms as i64,
                ],
            )?;
        }

        for sig_id in &tx.consume_signal_ids {
            db_tx.execute(
                "UPDATE signals SET consumed_at_ms = ?1
                 WHERE conversation_id = ?2 AND signal_id = ?3 AND consumed_at_ms IS NULL",
                params![tx.now_ms as i64, &tx.conversation_id, sig_id],
            )?;
        }

        let mut document_revisions = Vec::with_capacity(tx.document_writes.len());
        for write in &tx.document_writes {
            let actual: Option<u64> = db_tx
                .query_row(
                    "SELECT revision FROM documents WHERE conversation_id = ?1 AND doc_key = ?2",
                    params![&tx.conversation_id, &write.key],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .map(|v| v.max(0) as u64);

            if actual != write.expected_revision {
                return Err(DurableError::Conflict {
                    conversation_id: tx.conversation_id,
                    key: write.key.clone(),
                    expected: write.expected_revision,
                    actual,
                });
            }

            let next_rev = actual.unwrap_or(0) + 1;
            let val_json = serde_json::to_string(&write.value)?;
            db_tx.execute(
                "INSERT INTO documents (conversation_id, doc_key, revision, value_json, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(conversation_id, doc_key) DO UPDATE SET
                    revision = excluded.revision,
                    value_json = excluded.value_json,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    &tx.conversation_id,
                    &write.key,
                    next_rev as i64,
                    &val_json,
                    tx.now_ms as i64,
                ],
            )?;
            document_revisions.push((write.key.clone(), next_rev));
        }

        db_tx.commit()?;

        Ok(CommitReceipt {
            conversation_id: tx.conversation_id,
            head_entry_id,
            last_seq: last_seq.max(0) as u64,
            document_revisions,
        })
    }
}
