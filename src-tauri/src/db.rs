//! SQLite storage for meetings and transcript segments, with FTS5 search.

use crate::models::{
    ExportStatus, Meeting, MeetingListItem, MeetingStatus, Speaker, TranscriptSegment,
    TranscriptionMode,
};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meetings (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    audio_path TEXT,
    transcription_mode TEXT NOT NULL,
    summary TEXT,
    summarized_at TEXT
);

CREATE TABLE IF NOT EXISTS segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    speaker TEXT NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_segments_meeting ON segments(meeting_id, start_ms);

CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(
    text,
    content='segments',
    content_rowid='id',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS segments_ai AFTER INSERT ON segments BEGIN
    INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS segments_ad AFTER DELETE ON segments BEGIN
    INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS segments_au AFTER UPDATE ON segments BEGIN
    INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
END;
"#;

/// Columns added after the first release, as `(name, type)`. Added to existing
/// databases on open (see `migrate`); all nullable.
const MEETING_EXPORT_COLUMNS: &[(&str, &str)] = &[
    ("export_status", "TEXT"),       // 'pending' | 'exported' | 'failed' | NULL = never
    ("export_document_id", "TEXT"),  // SecondBrain document id
    ("exported_at", "TEXT"),         // RFC3339 of the last successful export
    ("export_error", "TEXT"),        // last failure message
    ("export_content_hash", "TEXT"), // sha256 of the last exported payload (VTT + metadata + summary)
    ("export_revision", "INTEGER"),  // revision of the transcript document (for If-Match)
    ("export_job_id", "TEXT"),       // SecondBrain job of the transcript upload
    ("export_summary_document_id", "TEXT"), // SecondBrain id of the summary note
    ("export_summary_revision", "INTEGER"), // revision of the summary note
    ("export_sync_state", "TEXT"), // committed field hashes and outstanding approval proposals
];

const MEETING_COLS: &str = "id, title, started_at, ended_at, duration_ms, status, audio_path, transcription_mode, summary, summarized_at, export_status, export_document_id, exported_at, export_error, export_summary_document_id";
/// Number of columns in `MEETING_COLS`; extra selected columns start here.
const MEETING_COL_COUNT: usize = 15;

/// What SecondBrain knows about a meeting's export: the documents created and
/// their revisions. Kept across failures so the next export updates them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportRecord {
    /// The `transcript` document.
    pub document_id: Option<String>,
    pub revision: Option<i64>,
    /// Job of the last transcript upload (ING-5).
    pub job_id: Option<String>,
    /// The summary `note` document.
    pub summary_document_id: Option<String>,
    pub summary_revision: Option<i64>,
    pub sync: ExportSyncState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSyncState {
    pub destination: Option<String>,
    pub transcript_hash: Option<String>,
    pub summary_hash: Option<String>,
    pub pending: Vec<PendingUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub pending_id: String,
    pub document_id: String,
    pub revision: i64,
    pub body: serde_json::Value,
}

/// Add any missing columns to `meetings`. Idempotent.
fn migrate(conn: &Connection) -> Result<()> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(meetings)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, ty) in MEETING_EXPORT_COLUMNS {
        if !existing.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE meetings ADD COLUMN {name} {ty}"))
                .with_context(|| format!("adding column meetings.{name}"))?;
        }
    }
    Ok(())
}

fn meeting_from_row(row: &Row<'_>) -> rusqlite::Result<Meeting> {
    let status: String = row.get(5)?;
    let mode: String = row.get(7)?;
    Ok(Meeting {
        id: row.get(0)?,
        title: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        duration_ms: row.get(4)?,
        status: MeetingStatus::parse(&status),
        audio_path: row.get(6)?,
        transcription_mode: TranscriptionMode::parse(&mode),
        summary: row.get(8)?,
        summarized_at: row.get(9)?,
        export_status: row
            .get::<_, Option<String>>(10)?
            .as_deref()
            .and_then(ExportStatus::parse),
        export_document_id: row.get(11)?,
        exported_at: row.get(12)?,
        export_error: row.get(13)?,
        export_summary_document_id: row.get(14)?,
    })
}

fn segment_from_row(row: &Row<'_>) -> rusqlite::Result<TranscriptSegment> {
    let speaker: String = row.get(2)?;
    Ok(TranscriptSegment {
        id: row.get(0)?,
        meeting_id: row.get(1)?,
        speaker: Speaker::parse(&speaker),
        start_ms: row.get(3)?,
        end_ms: row.get(4)?,
        text: row.get(5)?,
    })
}

/// Turn free text into a safe FTS5 MATCH expression: each word becomes a quoted
/// token, the last one a prefix query. Returns None for an empty query.
pub fn fts_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split_whitespace()
        .map(|w| w.replace('"', "\"\""))
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let n = words.len();
    let parts: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if i == n - 1 {
                format!("\"{w}\"*")
            } else {
                format!("\"{w}\"")
            }
        })
        .collect();
    Some(parts.join(" "))
}

/// Longest transcript segment text stored, in characters. A single chunk of
/// speech is at most ~60 s, so anything longer is a misbehaving engine.
pub const MAX_SEGMENT_CHARS: usize = 4000;

fn truncate_segment_text(text: &str) -> &str {
    match text.char_indices().nth(MAX_SEGMENT_CHARS) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("initialising schema")?;
        migrate(&conn).context("migrating schema")?;
        // Owner-only access for the database and its WAL/SHM side files.
        crate::fs_perms::harden_file(path);
        for suffix in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            let side = std::path::PathBuf::from(side);
            if side.exists() {
                crate::fs_perms::harden_file(&side);
            }
        }
        // Any meeting left in "recording" state is from a crashed session.
        conn.execute(
            "UPDATE meetings SET status = 'done' WHERE status IN ('recording', 'processing')",
            [],
        )?;
        // Likewise an export that was in flight when the app quit.
        conn.execute(
            "UPDATE meetings SET export_status = 'failed', export_error = 'Export was interrupted (the app closed). Export again.'
             WHERE export_status = 'pending'",
            [],
        )?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_meeting(&self, meeting: &Meeting) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            &format!("INSERT INTO meetings ({MEETING_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"),
            params![
                meeting.id,
                meeting.title,
                meeting.started_at,
                meeting.ended_at,
                meeting.duration_ms,
                meeting.status.as_str(),
                meeting.audio_path,
                meeting.transcription_mode.as_str(),
                meeting.summary,
                meeting.summarized_at,
                meeting.export_status.map(|s| s.as_str()),
                meeting.export_document_id,
                meeting.exported_at,
                meeting.export_error,
                meeting.export_summary_document_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_meeting(&self, id: &str) -> Result<Option<Meeting>> {
        let conn = self.conn.lock();
        let m = conn
            .query_row(
                &format!("SELECT {MEETING_COLS} FROM meetings WHERE id = ?1"),
                params![id],
                meeting_from_row,
            )
            .optional()?;
        Ok(m)
    }

    pub fn require_meeting(&self, id: &str) -> Result<Meeting> {
        self.get_meeting(id)?
            .ok_or_else(|| anyhow::anyhow!("meeting {id} not found"))
    }

    pub fn finish_meeting(
        &self,
        id: &str,
        ended_at: &str,
        duration_ms: i64,
        audio_path: Option<&str>,
        status: MeetingStatus,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE meetings SET ended_at = ?2, duration_ms = ?3, audio_path = ?4, status = ?5 WHERE id = ?1",
            params![id, ended_at, duration_ms, audio_path, status.as_str()],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: MeetingStatus) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE meetings SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    pub fn rename_meeting(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE meetings SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn set_summary(&self, id: &str, summary: &str, summarized_at: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE meetings SET summary = ?2, summarized_at = ?3 WHERE id = ?1",
            params![id, summary, summarized_at],
        )?;
        Ok(())
    }

    /// Mark an export as started; clears the previous error.
    pub fn set_export_pending(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE meetings SET export_status = 'pending', export_error = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// The SecondBrain documents and revisions recorded for a meeting.
    pub fn export_record(&self, id: &str) -> Result<ExportRecord> {
        let conn = self.conn.lock();
        let rec = conn
            .query_row(
                "SELECT export_document_id, export_revision, export_job_id,
                        export_summary_document_id, export_summary_revision, export_sync_state
                 FROM meetings WHERE id = ?1",
                params![id],
                |row| {
                    Ok(ExportRecord {
                        document_id: row.get(0)?,
                        revision: row.get(1)?,
                        job_id: row.get(2)?,
                        summary_document_id: row.get(3)?,
                        summary_revision: row.get(4)?,
                        sync: row.get::<_, Option<String>>(5)?
                            .map(|s| serde_json::from_str(&s).map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                                5, rusqlite::types::Type::Text, Box::new(e))))
                            .transpose()?.unwrap_or_default(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("meeting {id} not found"))?;
        Ok(rec)
    }

    /// Store the documents and revisions in `rec`.
    fn write_export_record(conn: &Connection, id: &str, rec: &ExportRecord) -> Result<()> {
        conn.execute(
            "UPDATE meetings SET export_document_id = ?2, export_revision = ?3, export_job_id = ?4,
                    export_summary_document_id = ?5, export_summary_revision = ?6, export_sync_state = ?7
             WHERE id = ?1",
            params![id, rec.document_id, rec.revision, rec.job_id, rec.summary_document_id, rec.summary_revision,
                serde_json::to_string(&rec.sync)?],
        )?;
        Ok(())
    }

    /// Record a successful export.
    pub fn checkpoint_export(&self, id: &str, rec: &ExportRecord) -> Result<()> {
        Self::write_export_record(&self.conn.lock(), id, rec)
    }

    /// Record a successful export.
    pub fn set_export_success(
        &self,
        id: &str,
        rec: &ExportRecord,
        content_hash: &str,
        exported_at: &str,
    ) -> Result<()> {
        anyhow::ensure!(rec.sync.pending.is_empty(), "cannot mark an unapproved export as successful");
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        Self::write_export_record(&tx, id, rec)?;
        tx.execute(
            "UPDATE meetings SET export_status = 'exported', export_content_hash = ?2,
                    exported_at = ?3, export_error = NULL
             WHERE id = ?1",
            params![id, content_hash, exported_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Save acknowledged proposals without advancing the last committed hash/time.
    pub fn set_export_awaiting_approval(&self, id: &str, rec: &ExportRecord) -> Result<()> {
        let ids = rec.sync.pending.iter().map(|p| p.pending_id.as_str()).collect::<Vec<_>>().join(", ");
        let message = format!("Approve the update in SecondBrain, then check approval here. Pending: {ids}");
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        Self::write_export_record(&tx, id, rec)?;
        tx.execute(
            "UPDATE meetings SET export_status = 'awaiting_approval', export_error = ?2 WHERE id = ?1",
            params![id, message],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a failed export together with whatever it created (`rec`), so
    /// the next attempt updates those documents. The hash of the last success
    /// is kept, so an automatic export will try again.
    pub fn set_export_failed(&self, id: &str, error: &str, rec: &ExportRecord) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        Self::write_export_record(&tx, id, rec)?;
        tx.execute(
            "UPDATE meetings SET export_status = 'failed', export_error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// sha256 of the document last exported successfully, if any.
    pub fn export_content_hash(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let hash = conn
            .query_row(
                "SELECT export_content_hash FROM meetings WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(hash)
    }

    pub fn delete_meeting(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM segments WHERE meeting_id = ?1", params![id])?;
        conn.execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn insert_segment(
        &self,
        meeting_id: &str,
        speaker: Speaker,
        start_ms: i64,
        end_ms: i64,
        text: &str,
    ) -> Result<TranscriptSegment> {
        let text = truncate_segment_text(text);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO segments (meeting_id, speaker, start_ms, end_ms, text) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![meeting_id, speaker.as_str(), start_ms, end_ms, text],
        )?;
        let id = conn.last_insert_rowid();
        Ok(TranscriptSegment {
            id,
            meeting_id: meeting_id.to_string(),
            speaker,
            start_ms,
            end_ms,
            text: text.to_string(),
        })
    }

    pub fn segments_for(&self, meeting_id: &str) -> Result<Vec<TranscriptSegment>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, speaker, start_ms, end_ms, text FROM segments WHERE meeting_id = ?1 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![meeting_id], segment_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_meetings(&self, query: Option<&str>) -> Result<Vec<MeetingListItem>> {
        let conn = self.conn.lock();
        let fts = query.and_then(fts_query);
        match fts {
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {MEETING_COLS}, (SELECT COUNT(*) FROM segments s WHERE s.meeting_id = m.id)
                     FROM meetings m ORDER BY started_at DESC"
                ))?;
                let rows = stmt.query_map([], |row| {
                    Ok(MeetingListItem {
                        meeting: meeting_from_row(row)?,
                        segment_count: row.get(MEETING_COL_COUNT)?,
                        snippet: None,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            Some(fts) => {
                let like = format!("%{}%", query.unwrap_or("").trim());
                let mut stmt = conn.prepare(&format!(
                    "SELECT {MEETING_COLS},
                        (SELECT COUNT(*) FROM segments s WHERE s.meeting_id = m.id),
                        (SELECT snippet(segments_fts, 0, '<b>', '</b>', '…', 14)
                           FROM segments_fts JOIN segments s ON s.id = segments_fts.rowid
                          WHERE s.meeting_id = m.id AND segments_fts MATCH ?1
                          ORDER BY rank LIMIT 1)
                     FROM meetings m
                     WHERE m.title LIKE ?2
                        OR m.id IN (SELECT s.meeting_id FROM segments_fts
                                    JOIN segments s ON s.id = segments_fts.rowid
                                    WHERE segments_fts MATCH ?1)
                     ORDER BY started_at DESC"
                ))?;
                let rows = stmt.query_map(params![fts, like], |row| {
                    Ok(MeetingListItem {
                        meeting: meeting_from_row(row)?,
                        segment_count: row.get(MEETING_COL_COUNT)?,
                        snippet: row.get(MEETING_COL_COUNT + 1)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_and_prefixes() {
        assert_eq!(fts_query("hello world").as_deref(), Some("\"hello\" \"world\"*"));
        assert_eq!(fts_query("  ").as_deref(), None);
        assert_eq!(fts_query("say \"hi\"").as_deref(), Some("\"say\" \"\"\"hi\"\"\"*"));
    }

    #[test]
    fn segment_text_is_capped() {
        let short = "héllo";
        assert_eq!(truncate_segment_text(short), short);
        let long: String = "é".repeat(MAX_SEGMENT_CHARS + 10);
        let t = truncate_segment_text(&long);
        assert_eq!(t.chars().count(), MAX_SEGMENT_CHARS);
        let exact: String = "a".repeat(MAX_SEGMENT_CHARS);
        assert_eq!(truncate_segment_text(&exact).len(), MAX_SEGMENT_CHARS);
    }

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("transcriptor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.sqlite")).unwrap();
        let m = Meeting {
            id: "m1".into(),
            title: "Planning".into(),
            started_at: "2026-10-08T10:00:00Z".into(),
            ended_at: None,
            duration_ms: 0,
            status: MeetingStatus::Recording,
            audio_path: None,
            transcription_mode: TranscriptionMode::Local,
            summary: None,
            summarized_at: None,
            export_status: None,
            export_document_id: None,
            exported_at: None,
            export_error: None,
            export_summary_document_id: None,
        };
        db.create_meeting(&m).unwrap();
        db.insert_segment("m1", Speaker::You, 0, 1000, "We need to ship the roadmap").unwrap();
        db.insert_segment("m1", Speaker::Others, 1000, 2000, "Agreed, next Friday").unwrap();
        let all = db.list_meetings(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].segment_count, 2);
        let hits = db.list_meetings(Some("roadmap")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.as_deref().unwrap().contains("<b>roadmap</b>"));
        let none = db.list_meetings(Some("zebra")).unwrap();
        assert!(none.is_empty());
        db.delete_meeting("m1").unwrap();
        assert!(db.list_meetings(None).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn meeting(id: &str) -> Meeting {
        Meeting {
            id: id.into(),
            title: "Sync".into(),
            started_at: "2026-10-08T10:00:00Z".into(),
            ended_at: None,
            duration_ms: 0,
            status: MeetingStatus::Done,
            audio_path: None,
            transcription_mode: TranscriptionMode::Local,
            summary: None,
            summarized_at: None,
            export_status: None,
            export_document_id: None,
            exported_at: None,
            export_error: None,
            export_summary_document_id: None,
        }
    }

    #[test]
    fn export_state_roundtrip() {
        let dir = std::env::temp_dir().join(format!("transcriptor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.sqlite")).unwrap();
        db.create_meeting(&meeting("m1")).unwrap();
        let m = db.require_meeting("m1").unwrap();
        assert_eq!(m.export_status, None);
        assert_eq!(db.export_content_hash("m1").unwrap(), None);

        db.set_export_pending("m1").unwrap();
        assert_eq!(db.require_meeting("m1").unwrap().export_status, Some(ExportStatus::Pending));

        assert_eq!(db.export_record("m1").unwrap(), ExportRecord::default());
        let rec = ExportRecord {
            document_id: Some("01K70Q3V8X".into()),
            revision: Some(1),
            job_id: Some("01JOB".into()),
            summary_document_id: Some("01NOTE".into()),
            summary_revision: Some(2),
            ..Default::default()
        };
        db.set_export_success("m1", &rec, "abc", "2026-10-08T11:00:00Z").unwrap();
        let m = db.require_meeting("m1").unwrap();
        assert_eq!(m.export_status, Some(ExportStatus::Exported));
        assert_eq!(m.export_document_id.as_deref(), Some("01K70Q3V8X"));
        assert_eq!(m.export_summary_document_id.as_deref(), Some("01NOTE"));
        assert_eq!(m.exported_at.as_deref(), Some("2026-10-08T11:00:00Z"));
        assert_eq!(db.export_content_hash("m1").unwrap().as_deref(), Some("abc"));
        assert_eq!(db.export_record("m1").unwrap(), rec);

        // A failure stores the progress made (new revision) but keeps the hash.
        db.set_export_pending("m1").unwrap();
        let progressed = ExportRecord { revision: Some(3), ..rec.clone() };
        db.set_export_failed("m1", "boom", &progressed).unwrap();
        let m = db.require_meeting("m1").unwrap();
        assert_eq!(m.export_status, Some(ExportStatus::Failed));
        assert_eq!(m.export_error.as_deref(), Some("boom"));
        // The document id survives a failure so the next export updates it.
        assert_eq!(m.export_document_id.as_deref(), Some("01K70Q3V8X"));
        assert_eq!(db.export_record("m1").unwrap(), progressed);
        assert_eq!(db.export_content_hash("m1").unwrap().as_deref(), Some("abc"));
        assert!(db.export_record("nope").is_err());

        // Listing still reads counts from the right column.
        let all = db.list_meetings(None).unwrap();
        assert_eq!(all[0].segment_count, 0);

        // A pending export left over from a quit is marked failed on reopen.
        db.set_export_pending("m1").unwrap();
        drop(db);
        let db = Db::open(&dir.join("t.sqlite")).unwrap();
        let m = db.require_meeting("m1").unwrap();
        assert_eq!(m.export_status, Some(ExportStatus::Failed));
        assert!(m.export_error.unwrap().contains("interrupted"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn migrates_old_schema() {
        let dir = std::env::temp_dir().join(format!("transcriptor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");
        {
            // The original meetings table, without the export columns.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meetings (
                    id TEXT PRIMARY KEY, title TEXT NOT NULL, started_at TEXT NOT NULL,
                    ended_at TEXT, duration_ms INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL,
                    audio_path TEXT, transcription_mode TEXT NOT NULL, summary TEXT, summarized_at TEXT);
                 INSERT INTO meetings (id, title, started_at, status, transcription_mode)
                 VALUES ('old', 'Old meeting', '2026-01-01T00:00:00Z', 'done', 'local');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let m = db.require_meeting("old").unwrap();
        assert_eq!(m.title, "Old meeting");
        assert_eq!(m.export_status, None);
        assert_eq!(m.export_summary_document_id, None);
        assert_eq!(db.export_record("old").unwrap(), ExportRecord::default());
        drop(db);
        // Opening again is a no-op.
        let db = Db::open(&path).unwrap();
        let rec = ExportRecord { document_id: Some("doc".into()), revision: Some(1), ..Default::default() };
        db.set_export_success("old", &rec, "h", "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(db.export_content_hash("old").unwrap().as_deref(), Some("h"));
        assert_eq!(db.export_record("old").unwrap(), rec);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn awaiting_approval_survives_restart_without_advancing_committed_hash() {
        let dir = std::env::temp_dir().join(format!("transcriptor-approval-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sqlite");
        let db = Db::open(&path).unwrap();
        db.create_meeting(&meeting("m1")).unwrap();
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        db.set_export_success("m1", &rec, "committed", "2026-10-24T12:00:00Z").unwrap();
        rec.sync.pending.push(PendingUpdate { pending_id: "01P".into(), document_id: "01TR".into(),
            revision: 3, body: serde_json::json!({"title": "Changed"}) });
        db.set_export_awaiting_approval("m1", &rec).unwrap();
        assert!(db.set_export_success("m1", &rec, "unapproved", "later").is_err());
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.require_meeting("m1").unwrap().export_status, Some(ExportStatus::AwaitingApproval));
        assert_eq!(db.export_record("m1").unwrap(), rec);
        assert_eq!(db.export_content_hash("m1").unwrap().as_deref(), Some("committed"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_v1_export_schema() {
        // A database from the first export release: export columns up to
        // export_content_hash, without revisions or the summary note.
        let dir = std::env::temp_dir().join(format!("transcriptor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meetings (
                    id TEXT PRIMARY KEY, title TEXT NOT NULL, started_at TEXT NOT NULL,
                    ended_at TEXT, duration_ms INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL,
                    audio_path TEXT, transcription_mode TEXT NOT NULL, summary TEXT, summarized_at TEXT,
                    export_status TEXT, export_document_id TEXT, exported_at TEXT, export_error TEXT,
                    export_content_hash TEXT);
                 INSERT INTO meetings (id, title, started_at, status, transcription_mode,
                                       export_status, export_document_id, export_content_hash)
                 VALUES ('v1', 'Exported', '2026-01-01T00:00:00Z', 'done', 'local',
                         'exported', '01OLD', 'oldhash');",
            )
            .unwrap();
        }
        for _ in 0..2 {
            let db = Db::open(&path).unwrap();
            let m = db.require_meeting("v1").unwrap();
            assert_eq!(m.export_status, Some(ExportStatus::Exported));
            assert_eq!(m.export_document_id.as_deref(), Some("01OLD"));
            assert_eq!(
                db.export_record("v1").unwrap(),
                ExportRecord { document_id: Some("01OLD".into()), ..Default::default() }
            );
            assert_eq!(db.export_content_hash("v1").unwrap().as_deref(), Some("oldhash"));
        }
        let conn = Connection::open(&path).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(meetings)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (name, _) in MEETING_EXPORT_COLUMNS {
            assert_eq!(cols.iter().filter(|c| c == name).count(), 1, "{name}");
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(dir);
    }
}
