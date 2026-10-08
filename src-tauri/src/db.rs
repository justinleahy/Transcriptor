//! SQLite storage for meetings and transcript segments, with FTS5 search.

use crate::models::{
    Meeting, MeetingListItem, MeetingStatus, Speaker, TranscriptSegment, TranscriptionMode,
};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
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

const MEETING_COLS: &str = "id, title, started_at, ended_at, duration_ms, status, audio_path, transcription_mode, summary, summarized_at";

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
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_meeting(&self, meeting: &Meeting) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            &format!("INSERT INTO meetings ({MEETING_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
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
                        segment_count: row.get(10)?,
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
                        segment_count: row.get(10)?,
                        snippet: row.get(11)?,
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
}
