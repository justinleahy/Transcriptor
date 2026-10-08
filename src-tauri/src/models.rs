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
        }
    }
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
