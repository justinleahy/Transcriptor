// Shared types mirrored from docs/IPC.md. Keep in sync with the Rust backend.

export type Speaker = "you" | "others";
export type TranscriptionMode = "local" | "cloud";
export type MeetingStatus = "recording" | "processing" | "done" | "failed";

export interface Meeting {
  id: string; // uuid
  title: string;
  started_at: string; // RFC3339
  ended_at: string | null;
  duration_ms: number;
  status: MeetingStatus;
  audio_path: string | null; // absolute path to 16kHz stereo WAV (L = you, R = others)
  transcription_mode: TranscriptionMode;
  summary: string | null; // markdown produced by Claude, null until summarized
  summarized_at: string | null;
}

export interface TranscriptSegment {
  id: number;
  meeting_id: string;
  speaker: Speaker;
  start_ms: number; // offset from meeting start
  end_ms: number;
  text: string;
}

export interface MeetingDetail {
  meeting: Meeting;
  segments: TranscriptSegment[]; // ordered by start_ms
}

export interface MeetingListItem {
  meeting: Meeting;
  segment_count: number;
  snippet: string | null; // FTS match snippet (with <b>..</b> marks) when searching, else null
}

export interface RecordingStatus {
  active: boolean;
  meeting_id: string | null;
  elapsed_ms: number;
  system_audio: boolean; // false when system-audio capture failed and only mic is recording
}

export interface AudioDevices {
  inputs: string[];
  outputs: string[];
  monitors: string[]; // subset of inputs that look like monitor/loopback sources (system audio)
  default_input: string | null;
  default_output: string | null;
}

export interface Settings {
  transcription_mode: TranscriptionMode;
  local_model: string; // e.g. "base.en", "small.en", "small", "medium.en", "large-v3-turbo"
  cloud_base_url: string; // default "https://api.openai.com/v1"
  cloud_model: string; // default "whisper-1"
  language: string; // "auto" or ISO code like "en"
  input_device: string | null; // null = system default
  capture_system_audio: boolean; // default true
  system_audio_device: string | null; // input device recorded as "others"; null = automatic loopback
  chunk_seconds: number; // live chunk length target, default 8
  claude_model: string; // default "claude-opus-5-5"
}

export interface ModelInfo {
  name: string; // "base.en"
  size_mb: number;
  downloaded: boolean;
  path: string | null;
}

export interface Levels {
  mic: number; // 0..1 RMS
  system: number; // 0..1 RMS
}

export type SecretName = "openai_api_key" | "anthropic_api_key";

// Event payloads
export interface RecordingWarning {
  message: string;
}

export interface ModelProgress {
  name: string;
  downloaded: number;
  total: number;
}

export const EVENTS = {
  recordingStatus: "recording:status",
  transcriptSegment: "transcript:segment",
  audioLevels: "audio:levels",
  recordingWarning: "recording:warning",
  modelProgress: "model:progress",
  meetingUpdated: "meeting:updated",
} as const;

export interface EventPayloads {
  "recording:status": RecordingStatus;
  "transcript:segment": TranscriptSegment;
  "audio:levels": Levels;
  "recording:warning": RecordingWarning;
  "model:progress": ModelProgress;
  "meeting:updated": Meeting;
}

export type AppEventName = keyof EventPayloads;
