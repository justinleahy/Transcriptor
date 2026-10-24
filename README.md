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
- **SecondBrain export** (optional): finished meetings become searchable `transcript`
  documents (WebVTT, with speaker segments) in your local SecondBrain knowledge base, and
  summaries become linked notes.
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

## SecondBrain export

Transcriptor can send each meeting to SecondBrain, your local-first knowledge base,
through its REST API (`POST /documents`, with v0.6 approval handling for updates). Turn it on in **Settings →
SecondBrain**, enter the API URL (default `http://127.0.0.1:7171/v1`), an API key, and
optionally **Your identity** (your email or name), then use **Test connection**.

- **Transcript document:** each meeting is uploaded once as a WebVTT file (multipart
  `file=<meeting id>.vtt`, `text/vtt`) plus a `metadata` part
  `{"type":"transcript","title":…,"tags":[…],"source_url":"transcriptor://meeting/<id>","props":{…}}`.
  Every transcript segment becomes one cue with a voice tag (`<v Justin>…` / `<v Others>…`)
  and millisecond timestamps, so SecondBrain's VTT extractor records speaker segments
  (`speaker`, `start_ms`, `end_ms`) and citations can name who said what and when.
  `props` follow the `brain:type/transcript/1` schema exactly: `participants`,
  `started_at` (UTC; SecondBrain derives `occurred_at` from it), `duration_ms`, and
  `recording_url` (`file://` path of the local WAV; the audio itself is not uploaded).
- **Your identity:** used as the speaker name for your side of the conversation and as the
  transcript's only participant (emails are lower-cased). The other side is always
  labelled "Others" and is never sent as a participant, so no fake people are created.
  When the field is empty your side is labelled "You" and `participants` is empty.
- **Notes document:** once a meeting has a Claude summary, it is saved as a separate `note`
  titled "<meeting> — notes" with the summary Markdown as `content`, your tags plus
  `summary`, and `source_url: transcriptor://meeting/<id>#summary`. Regenerating the summary
  proposes an update to that note; approve it in SecondBrain to apply the change.
- **When:** with *Export automatically* on (the default), when transcription finishes, and
  again after a summary is generated. A meeting that was already exported is re-exported
  after a new summary even with auto-export off. You can always export or re-export from
  the meeting's page, which shows the export status, the document ids, and the last error.
  Automatic exports are skipped when the transcript, its metadata, and the summary are all
  unchanged since the last success. *Wait for indexing* adds `?wait=30`; `200`, `201`, and
  `202` (accepted, still indexing) all count as success and the job id is recorded.
- **Updates and revisions:** a transcript's content is immutable once uploaded, so later
  exports only `PATCH /documents/<id>` its `title` and `tags`; the note is patched with
  `title`, `content`, and `tags`. Every `PATCH` sends `If-Match: "<revision>"` with the
  revision stored from the last response (`document.revision` or `ETag`). On
  `412 revision-mismatch` Transcriptor takes `current_revision` from the error (or re-reads
  the document) and retries once. Under v0.6, a key's title/content updates return an
  approval proposal (`202 pending_id`). The meeting shows **Awaiting approval** and
  preserves the last committed revision and export time. Approve the proposal in
  SecondBrain, then click **Check approval**: Transcriptor reads the document and only
  marks the update exported when the proposed values and a newer revision are present.
  Outstanding proposal ids and their intended values survive app restarts, and checks
  do not resubmit the proposal. Unchanged fields do not generate new proposals. If a
  document was deleted in SecondBrain (404), a new one is created.
- **Idempotency:** each `POST` carries `Idempotency-Key:
  transcriptor-<meeting id>-<16 hex chars of the payload's sha256>`, so a retried request
  never creates a duplicate and a changed payload never reuses a key. POST/GET connection
  errors and 5xx responses are retried up to 3 times with the same key; PATCH is not
  automatically retried after an ambiguous failure because it can create a second proposal.
  4xx errors are not retried (apart from revision recovery), and
  SecondBrain's problem `type` (`validation-error`, `revision-mismatch`, …) is shown in the
  error message.
- **API key:** create one with `brain keys create`, name it `transcriptor`, and give it the
  `read` and `write` scopes (reads verify approved updates and **Test connection**). SecondBrain records
  provenance from the key name, so exported documents show `source: api:transcriptor`. The key is kept in the OS keychain, and changing the
  SecondBrain URL to a different host or port deletes it.
- **Test connection:** calls `GET /health` (no auth) to see whether the server is reachable,
  then `GET /stats` with your key: "Connected" when the key is accepted, "Key rejected" on
  401/403 (a key without `read` gets 403 here even though it can still export).
- **Plain HTTP:** the URL must be `https://` unless it points at localhost. SecondBrain v1
  runs without TLS inside your VPN, so **Allow plain HTTP on my private network** also
  accepts `http://` for private addresses (192.168.x.x, 10.x.x.x, 172.16–31.x.x,
  169.254.x.x, IPv6 `fc00::/7` and `fe80::/10`), single-label host names, and `*.lan`,
  `*.local`, `*.internal`, `*.home`, `*.home.arpa`, `*.localdomain` names. Traffic, including
  the API key, is then unencrypted; plain `http://` to any other host is always rejected.

SecondBrain itself is still at the spec stage. Creation follows the transcript schemas;
updates handle v0.6 approval proposals. This integration has not been run against a real
server. Approval checks require the document endpoint to expose title/tags and, for notes,
the Markdown body as `content` or `text` when requested with `?include=text`; missing fields
leave the update awaiting approval. Cloudflare Access service tokens and custom/private
certificate authorities are not configured by this client.

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
Ubuntu for every push and pull request. `.github/workflows/release.yml` repeats these
checks before building installers for `v*` tags. Tag builds require macOS signing and
notarization and Windows signing; missing credentials fail the job instead of quietly
producing unsigned release installers. Artifacts are uploaded only after verification.
The workflow can also be run manually with `signed: false` for clearly labelled unsigned
test artifacts. These workflows still need a successful run in the hosting repository;
local macOS checks do not verify Windows or Linux capture or installation.

Configure these repository secrets before tagging a release:

- macOS: `APPLE_CERTIFICATE` (base64 Developer ID Application `.p12`),
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`,
  `APPLE_PASSWORD` (app-specific password), and `APPLE_TEAM_ID`.
- Windows: `WINDOWS_CERTIFICATE` (base64 signing `.pfx`) and
  `WINDOWS_CERTIFICATE_PASSWORD`; set the repository variable `WINDOWS_TIMESTAMP_URL`
  to your certificate provider's timestamp service. The certificate must support
  signing on the runner; hardware-backed providers require their own signing setup.

See Tauri's [macOS signing](https://tauri.app/distribute/sign/macos/) and
[Windows signing](https://tauri.app/distribute/sign/windows/) instructions.

## Build a release bundle

```bash
npm run tauri build
```

For a local unsigned test build: `npm run tauri -- build --ci --no-sign`.
On macOS, the build discovers the active Xcode Clang runtime needed by Whisper's
Metal availability checks; no Xcode-version-specific path is required.

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
  export.rs              SecondBrain export: WebVTT/metadata builders, REST client, auto-export triggers
  net.rs                 bounded HTTP response bodies
  fs_perms.rs            owner-only file/dir permissions (Unix)
  recorder.rs            session orchestration: capture → WAV + chunks → transcriber → DB/events
```

Speaker separation is by source: anything from the microphone is "You", anything from
system audio is "Others". Each source is chunked and transcribed independently and the
segments are merged by timestamp.
