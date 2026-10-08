# Transcriptor

A cross-platform desktop app (Rust + Tauri 2 + React) that records your meetings and
transcribes them live, with speaker separation, Claude-generated summaries, and a
searchable library.

## Features

- **Records two sources at once**: your microphone ("You") and system audio ("Others",
  i.e. everyone else in Zoom / Meet / Teams / FaceTime). Saved as a 16 kHz stereo WAV
  (left = you, right = others).
- **Live transcript** while recording, chunked at natural pauses.
- **Local or cloud transcription**, switchable in Settings:
  - Local: whisper.cpp via `whisper-rs` (Metal-accelerated on Apple silicon), models
    downloaded in-app from Hugging Face. Private, offline, free.
  - Cloud: any OpenAI-compatible `/audio/transcriptions` endpoint (OpenAI, Groq, …).
- **AI summary & action items** with the Claude API (summary, key points, decisions,
  action items, open questions).
- **Library with full-text search** (SQLite FTS5) over titles and transcripts, with
  audio playback.
- API keys are stored in the OS keychain, never in plain files.

## Requirements

- Rust 1.88+ (`rustup update stable`), Node 20+, and the
  [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/) for your OS.
- macOS 14.6+ for system-audio capture (Core Audio process taps via cpal). The first
  recording prompts for Microphone and System Audio Recording permissions.
- Windows: system audio via WASAPI loopback (cpal). Linux: system audio via the
  PipeWire/PulseAudio monitor source of the default output (found automatically, or
  pick one in Settings).

## Platform support

| Platform | Microphone | System audio ("Others") | Local Whisper GPU | Status |
|---|---|---|---|---|
| macOS 14.6+ | ✅ | ✅ Core Audio process tap | ✅ Metal (always on) | Primary development platform |
| Windows 10/11 | ✅ | ✅ WASAPI loopback | Optional: Vulkan or CUDA (build feature) | Untested until CI runs |
| Linux (PipeWire or PulseAudio) | ✅ | ✅ monitor source via PulseAudio protocol (works with pipewire-pulse) | Optional: Vulkan (build feature) | Untested until CI runs |

Mobile (iOS/Android) is not supported in v1.

**Choosing the "Others" source.** Settings → Audio → *Other participants (system audio)
source* defaults to **Automatic**: macOS and Windows record the default output device
via loopback; Linux uses the monitor source of the default output (e.g. "Monitor of
Built-in Audio Analog Stereo"), falling back to any monitor-like input. You can instead
pick any input device explicitly, such as a Linux monitor source, BlackHole on macOS, or
"Stereo Mix" on Windows; monitor/loopback-looking devices are listed first.

On Windows, WASAPI loopback delivers nothing while nothing is playing. The recorder
aligns both WAV channels (and transcript timestamps) to wall-clock time, inserting
silence for a source that falls more than 250 ms behind, so "You" and "Others" stay in
sync across silent stretches.

### Optional GPU builds (Windows / Linux)

Local transcription runs on the CPU by default on Windows and Linux. Two opt-in cargo
features enable whisper.cpp GPU backends (the matching SDK must be installed at build
time; neither is needed on macOS, which always builds with Metal):

```bash
npm run tauri build -- --features gpu-vulkan   # Vulkan SDK (Windows/Linux)
npm run tauri build -- --features gpu-cuda     # CUDA toolkit (NVIDIA, Windows/Linux)
```

## Develop

```bash
npm install
npm run tauri dev
```

Data lives in the per-user app data directory (macOS:
`~/Library/Application Support/com.justinleahy.transcriptor/`): `transcriptor.sqlite`,
`recordings/*.wav`, `models/ggml-*.bin`. Settings are in the app config directory.

**Local data is not encrypted at rest.** Recordings, transcripts, summaries, and settings
are plain files protected only by file permissions (on macOS/Linux the app sets its
directories to `0700` and its files to `0600`; on Windows they inherit the per-user
profile ACLs). Anyone with access to your user account, or to an unencrypted backup or
disk, can read them; use full-disk encryption (FileVault, BitLocker, LUKS) if that matters.
API keys are the exception: they live in the OS keychain.

Whisper models are downloaded from a pinned Hugging Face commit and verified against a
SHA-256 digest and exact size before use (see `transcribe/models.rs`). The cloud
transcription endpoint must be `https://` (plain `http://` only for localhost), and
changing it to a different origin clears the stored cloud API key.

Useful checks:

```bash
cd src-tauri
cargo test                       # unit tests (chunker, resampler, db, wav)
cargo clippy --all-targets
cargo run --example capture_probe            # opens mic + loopback for 4 s and reports frames
cargo run --example capture_probe -- "Monitor of Built-in Audio Analog Stereo"  # explicit "Others" source
cargo run --example whisper_probe -- <model.bin> <file.wav>
```

### Continuous integration

`.github/workflows/ci.yml` type-checks and builds the frontend, then runs
`cargo clippy --all-targets -- -D warnings` and `cargo test` on macOS, Windows, and
Ubuntu for every push and pull request. `.github/workflows/release.yml` builds unsigned
bundles on all three OSes for `v*` tags and uploads them as workflow artifacts. Both
workflows are new and **have not run yet**; Windows and Linux builds have so far only
been type-checked from macOS for the audio module.

## Build a release bundle

```bash
npm run tauri build
```

Installers are deliberately limited to ones that **do not need administrator rights**:

- Windows: NSIS `.exe` in `currentUser` mode (installs to `%LOCALAPPDATA%`). The MSI target
  is excluded because Tauri's MSI is per-machine. If WebView2 is missing (rare; it ships with
  Windows 11 and nearly all Windows 10 machines), the bundled bootstrapper installs it per-user.
- macOS: `.app` / `.dmg`. Non-admin users can drag it to `~/Applications`.
- Linux: AppImage (no package manager, no root). The `.deb` target is excluded.

At runtime the app only writes to the per-user data/config directories and the user's keychain.

## Architecture

```
src/                     React + TypeScript UI (Record, Library, Meeting, Settings)
docs/IPC.md              The command/event contract between UI and backend
src-tauri/src/
  lib.rs                 Tauri setup, state, exit handling
  commands.rs            IPC commands
  models.rs              Shared serde types (mirror of docs/IPC.md)
  settings.rs            settings.json + OS keychain secrets
  db.rs                  SQLite (rusqlite) + FTS5 search
  audio/mod.rs           cpal capture: mic + system loopback / monitor source, one thread per stream
  audio/resample.rs      downmix + resample to 16 kHz mono
  transcribe/chunker.rs  energy-based VAD chunking
  transcribe/local.rs    whisper.cpp engine
  transcribe/cloud.rs    OpenAI-compatible HTTP engine
  transcribe/models.rs   model catalogue + downloads
  summarize.rs           Claude Messages API (raw HTTP); long transcripts summarized in parts
  net.rs                 bounded HTTP response bodies
  fs_perms.rs            owner-only file/dir permissions (Unix)
  recorder.rs            session orchestration: capture → WAV + chunks → transcriber → DB/events
```

Speaker separation is by source: anything from the microphone is "You", anything from
system audio is "Others". Each source is chunked and transcribed independently and the
segments are merged by timestamp.
