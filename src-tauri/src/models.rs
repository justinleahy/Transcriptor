//! Shared data types. These mirror `docs/IPC.md` exactly and are what the
//! frontend receives over Tauri IPC.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    You,
    Others,
}

impl Speaker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Speaker::You => "you",
            Speaker::Others => "others",
        }
    }

    pub fn parse(s: &str) -> Speaker {
        match s {
            "you" => Speaker::You,
            _ => Speaker::Others,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionMode {
    Local,
    Cloud,
}

impl TranscriptionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranscriptionMode::Local => "local",
            TranscriptionMode::Cloud => "cloud",
        }
    }

    pub fn parse(s: &str) -> TranscriptionMode {
        match s {
            "cloud" => TranscriptionMode::Cloud,
            _ => TranscriptionMode::Local,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MeetingStatus {
    Recording,
    Processing,
    Done,
    Failed,
}

impl MeetingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MeetingStatus::Recording => "recording",
            MeetingStatus::Processing => "processing",
            MeetingStatus::Done => "done",
            MeetingStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> MeetingStatus {
        match s {
            "recording" => MeetingStatus::Recording,
            "processing" => MeetingStatus::Processing,
            "failed" => MeetingStatus::Failed,
            _ => MeetingStatus::Done,
        }
    }
}

/// State of a meeting's export to SecondBrain. `None` on a meeting means it
/// has never been exported.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportStatus {
    Pending,
    #[serde(rename = "awaiting_approval")]
    AwaitingApproval,
    Exported,
    Failed,
}

impl ExportStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExportStatus::Pending => "pending",
            ExportStatus::AwaitingApproval => "awaiting_approval",
            ExportStatus::Exported => "exported",
            ExportStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<ExportStatus> {
        match s {
            "pending" => Some(ExportStatus::Pending),
            "awaiting_approval" => Some(ExportStatus::AwaitingApproval),
            "exported" => Some(ExportStatus::Exported),
            "failed" => Some(ExportStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: i64,
    pub status: MeetingStatus,
    pub audio_path: Option<String>,
    pub transcription_mode: TranscriptionMode,
    pub summary: Option<String>,
    pub summarized_at: Option<String>,
    /// SecondBrain export state; None = never exported.
    #[serde(default)]
    pub export_status: Option<ExportStatus>,
    /// SecondBrain document id (ULID) once created.
    #[serde(default)]
    pub export_document_id: Option<String>,
    /// RFC3339 time of the last successful export.
    #[serde(default)]
    pub exported_at: Option<String>,
    /// Error from the last failed export.
    #[serde(default)]
    pub export_error: Option<String>,
    /// SecondBrain id of the summary `note` document, once created.
    #[serde(default)]
    pub export_summary_document_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: i64,
    pub meeting_id: String,
    pub speaker: Speaker,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingDetail {
    pub meeting: Meeting,
    pub segments: Vec<TranscriptSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingListItem {
    pub meeting: Meeting,
    pub segment_count: i64,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub active: bool,
    pub meeting_id: Option<String>,
    pub elapsed_ms: i64,
    pub system_audio: bool,
}

impl RecordingStatus {
    pub fn idle() -> Self {
        RecordingStatus {
            active: false,
            meeting_id: None,
            elapsed_ms: 0,
            system_audio: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Input devices that look like monitor / loopback sources (subset of `inputs`).
    pub monitors: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub transcription_mode: TranscriptionMode,
    pub local_model: String,
    pub cloud_base_url: String,
    pub cloud_model: String,
    pub language: String,
    pub input_device: Option<String>,
    pub capture_system_audio: bool,
    /// Input device recorded as "Others"; None = automatic system loopback.
    pub system_audio_device: Option<String>,
    pub chunk_seconds: u32,
    pub claude_model: String,
    /// Send meetings to SecondBrain (the user's local knowledge base).
    pub secondbrain_enabled: bool,
    /// SecondBrain REST base URL, e.g. "http://127.0.0.1:7171/v1".
    pub secondbrain_url: String,
    /// Export automatically when transcription finishes (and after summaries).
    pub secondbrain_auto_export: bool,
    /// Tags attached to every exported document.
    pub secondbrain_tags: Vec<String>,
    /// Ask SecondBrain to block until the document is indexed (`?wait=30`).
    pub secondbrain_wait_for_index: bool,
    /// The user's email or name: the `<v …>` speaker for "You" in the
    /// exported WebVTT and the transcript's only `participants[]` entry.
    /// Empty = speaker "You" and no participants.
    pub secondbrain_identity: String,
    /// Allow plain `http://` SecondBrain URLs on private-network hosts
    /// (RFC1918, link-local, ULA, `.lan`/`.local`/... names).
    pub secondbrain_allow_private_http: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            transcription_mode: TranscriptionMode::Local,
            local_model: "base.en".to_string(),
            cloud_base_url: "https://api.openai.com/v1".to_string(),
            cloud_model: "whisper-1".to_string(),
            language: "auto".to_string(),
            input_device: None,
            capture_system_audio: true,
            system_audio_device: None,
            chunk_seconds: 8,
            claude_model: "claude-opus-5-5".to_string(),
            secondbrain_enabled: false,
            secondbrain_url: "http://127.0.0.1:7171/v1".to_string(),
            secondbrain_auto_export: true,
            secondbrain_tags: vec!["meeting".to_string(), "transcriptor".to_string()],
            secondbrain_wait_for_index: false,
            secondbrain_identity: String::new(),
            secondbrain_allow_private_http: false,
        }
    }
}

/// Result of the `secondbrain_status` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecondBrainStatus {
    pub enabled: bool,
    pub has_key: bool,
    /// `GET /health` answered. None when no check was attempted (disabled).
    pub reachable: Option<bool>,
    /// `GET /stats` accepted the key (true) or answered 401/403 (false).
    /// None when not checked (unreachable, no key) or inconclusive.
    pub key_ok: Option<bool>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size_mb: u64,
    pub downloaded: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Levels {
    pub mic: f32,
    pub system: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProgress {
    pub name: String,
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub message: String,
}

/// Event names, kept in one place so the backend and `docs/IPC.md` can't drift.
pub mod events {
    pub const RECORDING_STATUS: &str = "recording:status";
    pub const TRANSCRIPT_SEGMENT: &str = "transcript:segment";
    pub const AUDIO_LEVELS: &str = "audio:levels";
    pub const RECORDING_WARNING: &str = "recording:warning";
    pub const MODEL_PROGRESS: &str = "model:progress";
    pub const MEETING_UPDATED: &str = "meeting:updated";
}
