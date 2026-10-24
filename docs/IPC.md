# Transcriptor IPC contract (frontend <-> Rust backend)

All commands are invoked with `invoke(name, args)` from `@tauri-apps/api/core`.
All events are received with `listen(name, handler)` from `@tauri-apps/api/event`.
Errors are thrown as plain strings.

## Shared types (TypeScript)

```ts
export type Speaker = "you" | "others";
export type TranscriptionMode = "local" | "cloud";
export type MeetingStatus = "recording" | "processing" | "done" | "failed";
export type ExportStatus = "pending" | "awaiting_approval" | "exported" | "failed";

export interface Meeting {
  id: string;             // uuid
  title: string;
  started_at: string;     // RFC3339
  ended_at: string | null;
  duration_ms: number;
  status: MeetingStatus;
  audio_path: string | null;      // absolute path to 16kHz stereo WAV (L = you, R = others)
  transcription_mode: TranscriptionMode;
  summary: string | null;         // markdown produced by Claude, null until summarized
  summarized_at: string | null;
  export_status: ExportStatus | null;   // SecondBrain export; null = never exported
  export_document_id: string | null;    // SecondBrain `transcript` document id (ULID) once created
  exported_at: string | null;           // RFC3339 of the last successful export
  export_error: string | null;          // failure message, or approval instructions/pending ids
  export_summary_document_id: string | null; // SecondBrain `note` (summary) document id once created
}

export interface TranscriptSegment {
  id: number;
  meeting_id: string;
  speaker: Speaker;
  start_ms: number;       // offset from meeting start
  end_ms: number;
  text: string;
}

export interface MeetingDetail {
  meeting: Meeting;
  segments: TranscriptSegment[];   // ordered by start_ms
}

export interface MeetingListItem {
  meeting: Meeting;
  segment_count: number;
  snippet: string | null;         // FTS match snippet (with <b>..</b> marks) when searching, else null
}

export interface RecordingStatus {
  active: boolean;
  meeting_id: string | null;
  elapsed_ms: number;
  system_audio: boolean;          // false when system-audio capture failed and only mic is recording
}

export interface AudioDevices {
  inputs: string[];
  outputs: string[];
  monitors: string[];             // subset of `inputs` that look like monitor/loopback sources
                                  // (PulseAudio/PipeWire monitors, BlackHole, Stereo Mix, ...)
  default_input: string | null;
  default_output: string | null;
}

export interface Settings {
  transcription_mode: TranscriptionMode;
  local_model: string;            // e.g. "base.en", "small.en", "small", "medium.en", "large-v3-turbo"
  cloud_base_url: string;         // default "https://api.openai.com/v1"
  cloud_model: string;            // default "whisper-1"
  language: string;               // "auto" or ISO code like "en"
  input_device: string | null;    // null = system default
  capture_system_audio: boolean;  // default true
  system_audio_device: string | null; // input device recorded as "others" (e.g. a Linux monitor
                                  // source, BlackHole, Stereo Mix); null = automatic loopback.
                                  // Ignored when capture_system_audio is false.
  chunk_seconds: number;          // live chunk length target, default 8
  claude_model: string;           // default "claude-opus-5-5"
  secondbrain_enabled: boolean;   // default false
  secondbrain_url: string;        // default "http://127.0.0.1:7171/v1"; same rules as cloud_base_url,
                                  // plus http:// to private-network hosts when
                                  // secondbrain_allow_private_http is true
  secondbrain_auto_export: boolean; // default true: export when transcription finishes and
                                  // re-export after a summary
  secondbrain_tags: string[];     // default ["meeting", "transcriptor"]; trimmed, deduplicated
  secondbrain_wait_for_index: boolean; // default false: POST with ?wait=30
  secondbrain_identity: string;   // default "": your email or name. Speaker name for "you" in the
                                  // exported WebVTT ("You" when empty) and the transcript's only
                                  // participants[] entry (none when empty). Whitespace collapsed.
  secondbrain_allow_private_http: boolean; // default false: also accept http:// SecondBrain URLs for
                                  // RFC1918 / link-local IPv4, IPv6 ULA (fc00::/7) / link-local
                                  // (fe80::/10), single-label names, and *.lan, *.local, *.internal,
                                  // *.home, *.home.arpa, *.localdomain
}

export interface SecondBrainStatus {
  enabled: boolean;
  has_key: boolean;               // secondbrain_api_key is in the keychain
  reachable: boolean | null;      // GET /health answered; null when no check was attempted (disabled)
  key_ok: boolean | null;         // GET /stats with the key: true = accepted, false = 401/403,
                                  // null = not checked (unreachable or no key) or inconclusive
  message: string;                // human-readable result
}

export interface ModelInfo {
  name: string;                   // "base.en"
  size_mb: number;
  downloaded: boolean;
  path: string | null;
}

export interface Levels { mic: number; system: number; }  // 0..1 RMS
```

## Commands

| Command | Args | Returns |
|---|---|---|
| `get_settings` | – | `Settings` |
| `save_settings` | `{ settings: Settings }` | `Settings` (normalized). Errors if `cloud_base_url` or `secondbrain_url` is not `https://` (or `http://` on localhost/127.0.0.1/::1, or for `secondbrain_url` also `http://` to a private-network host when `secondbrain_allow_private_http` is true) or has credentials/query/fragment. If `cloud_base_url`'s origin changes, the stored `openai_api_key` is deleted; if `secondbrain_url`'s origin changes, `secondbrain_api_key` is deleted; re-check `has_secret` afterwards. |
| `set_secret` | `{ name: "openai_api_key" \| "anthropic_api_key" \| "secondbrain_api_key", value: string }` | `void` (empty string deletes) |
| `has_secret` | `{ name }` | `boolean` |
| `list_audio_devices` | – | `AudioDevices` |
| `start_recording` | `{ title: string \| null }` | `Meeting` |
| `stop_recording` | – | `Meeting` |
| `recording_status` | – | `RecordingStatus` |
| `list_meetings` | `{ query: string \| null }` | `MeetingListItem[]` newest first; when `query` is non-empty, full-text search over transcript + title |
| `get_meeting` | `{ id: string }` | `MeetingDetail` |
| `rename_meeting` | `{ id, title }` | `Meeting` |
| `delete_meeting` | `{ id }` | `void` (also deletes audio file) |
| `summarize_meeting` | `{ id }` | `Meeting` (with `summary` filled) — calls Claude; may take 10–60 s. Afterwards re-exports to SecondBrain in the background if export is enabled and the meeting was already exported or auto-export is on. |
| `export_meeting` | `{ id }` | `Meeting` — exports or checks approval in SecondBrain (regardless of `secondbrain_auto_export`): the transcript as a WebVTT `transcript` document (`export_document_id`) and, if there is a summary, a `note` (`export_summary_document_id`). Requires `secondbrain_enabled`, a read/write `secondbrain_api_key`, a finished meeting, and a transcript. Sets `export_status` `pending` → `exported` / `awaiting_approval` / `failed` and emits `meeting:updated`. A `202 pending_id` from PATCH preserves committed revisions and hashes; proposal ids/values persist in the database. Re-invoking reads the document and confirms its values and newer revision before marking it exported. On failure the error is stored in `export_error` and thrown. Errors if an export is already running. |
| `secondbrain_status` | – | `SecondBrainStatus` — when enabled, calls SecondBrain `GET /health` without auth (`reachable`), then, if a key is saved, `GET /stats` with it (`key_ok`); 5 s timeout per request |
| `list_local_models` | – | `ModelInfo[]` |
| `download_model` | `{ name }` | `ModelInfo` (resolves when done; progress via event) |
| `delete_model` | `{ name }` | `void` |
| `open_data_dir` | – | `void` opens the app data folder in Finder/Explorer |

Audio playback: use `convertFileSrc(meeting.audio_path)` from `@tauri-apps/api/core` as the `<audio src>`.

## Events

| Event | Payload |
|---|---|
| `recording:status` | `RecordingStatus` — emitted on start, stop, and every second while recording |
| `transcript:segment` | `TranscriptSegment` — a newly transcribed segment for the active meeting |
| `audio:levels` | `Levels` — ~10×/s while recording |
| `recording:warning` | `{ message: string }` — non-fatal problems (e.g. system audio unavailable) |
| `model:progress` | `{ name: string; downloaded: number; total: number }` |
| `meeting:updated` | `Meeting` — status/summary/export changes (e.g. transcription finished after stop, SecondBrain export started or finished) |
