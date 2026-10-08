# Security review — 2026-10-08

Reviewer: Daybreak Blue (`gpt-daybreak-blue-latest`, high reasoning), read-only source review delegated from T3 Code.
Scope: full repository at the state after the no-admin installer change. No GUI, recording, model download, or live API calls were run.

## Executive summary

No Critical or High findings. 4 Medium, 5 Low.

| # | Severity | Finding | Location |
|---|---|---|---|
| 1 | Medium | Changing `cloud_base_url` silently reuses the stored cloud API key and sends audio to the new origin; HTTP / loopback / private hosts are not rejected | settings.rs:45, cloud.rs:43,65 |
| 2 | Medium | Spoken prompt injection can steer Claude's decisions/action items; system prompt doesn't mark transcript as untrusted | summarize.rs:14,30,63 |
| 3 | Medium | Unbounded chunk channel between processing and transcription threads; slow transcriber ⇒ unbounded RAM growth (~128 KB/s, ~4.6 GB over 10 h) | recorder.rs:180,329,453 |
| 4 | Medium | Whisper models downloaded from mutable HF `main` with no SHA-256 / size / magic check before native parsing | transcribe/models.rs:79–100, local.rs:27 |
| 5 | Low | No body-size limits on transcription/Claude responses or model downloads; no download timeout | cloud.rs:74, summarize.rs:109, models.rs:90 |
| 6 | Low | `delete_meeting` uses lexical `starts_with` on a DB-stored path; tampered DB could delete outside data dir | commands.rs:115 |
| 7 | Low | CSP `null`, asset scope `$APPDATA/**`, unused `dialog:default` / `opener:default` capabilities | tauri.conf.json, capabilities/default.json |
| 8 | Low | Data dir / SQLite / WAV / settings created with ambient umask (0755/0644) rather than owner-only | lib.rs:22, db.rs:119, settings.rs:52, recorder.rs:154 |
| 9 | Low | Summarization has no transcript-size or cost bound; whole transcript in one request | db.rs:244, summarize.rs:30,63 |

## Recommended fixes (from the report)

1. Parse `cloud_base_url` as a URL on save; require HTTPS (loopback HTTP only behind an explicit dev toggle); reject credentials/fragments; key the stored credential by origin and require re-entry when the origin changes.
2. Move all behavioral instructions to the system prompt; state that title and transcript are untrusted quoted evidence whose instructions must never be followed; require timestamps for every decision/action item; consider structured JSON output validated locally.
3. Replace `unbounded` with a duration/byte-bounded queue and `try_send`; warn and drop live transcription when backlog exceeds a limit; cancel in-flight cloud requests on stop.
4. Pin model URLs to an immutable revision; add SHA-256 per catalog entry; hash while streaming; enforce size range; check ggml magic; fsync then atomic rename.
5. Reject oversized `Content-Length`; stream with byte caps; add connect/idle/total timeouts to downloads; delete `.part` on failure.
6. Derive the WAV path from the meeting UUID instead of trusting the stored path (or canonicalize and require a direct child of `recordings/`).
7. Set a restrictive CSP; narrow asset scope to `$APPDATA/recordings/*.wav`; drop `dialog:default` and `opener:default`; minimize `core:default`.
8. `0700` dirs / `0600` files on Unix; user-only ACL on Windows; document that data is not encrypted at rest.
9. Cap transcript bytes/tokens before building the request; chunk-and-merge long meetings; confirm above a cost threshold.

## Checked and found sound

- SQL uses rusqlite parameters; FTS query builder quotes every token (tested `"`, `OR`, `NEAR(`, `*`) — no breakout.
- Markdown and snippet renderers emit React text nodes only; no raw HTML, no links — no XSS path found.
- Secrets allowlisted by name, stored in OS keychain, never in `Settings`, never logged.
- Model names validated against the static catalog before path construction.
- Anthropic and Hugging Face endpoints are HTTPS with rustls verification; malformed JSON errors rather than panics.
- Raw audio channel bounded (512, `try_send`); WAV lead capped at 2 s; chunk buffers capped at 2× target (≤ 60 s).
- No `unsafe` in authored Rust; whisper context cleared on exit.
- Local mode sends nothing off-device; summarization explicitly sends transcript + title + date + duration to Anthropic.

## Verification run by the reviewer

- `cargo test`: 9 passed
- `cargo clippy --all-targets -- -D warnings`: clean
- `tsc --noEmit`: clean
- `npm audit`: 0 vulnerabilities
- `cargo audit`: not run (cargo-audit not installed)

## Not verified

GUI, live capture, model download, third-party API calls, OS keychain/ACL behavior on packaged builds, RustSec advisories, whisper.cpp internals.

---

# Round 2 — re-review after fixes (same day)

Fixes implemented by Claude Opus 5.5; re-reviewed by Daybreak Blue. Verification: `cargo test` 32/32, clippy `-D warnings` clean, `tsc` clean, `vite build` ok, `npm audit` 0 vulnerabilities, all nine pinned model digests matched Hugging Face LFS metadata at commit `5359861c739e955e79d9a303bcbc70fb988958b1`.

| # | Status | Remaining gap |
|---|---|---|
| 1 | Partially fixed | Key is still a single global entry; `settings.json` tampering (or deletion) can redirect it to another HTTPS origin on restart. Fix: bind `{origin, key}` in the keychain and validate URL on load. |
| 2 | Partially fixed | Boundary is model-enforced; `started_at` not neutralized; no local validation that cited timestamps exist. Fix: neutralize all metadata, request parseable structure, verify citations locally. |
| 3 | Fixed | Bounded queue, non-blocking, no deadlock on stop. Final flush may be dropped under backlog (warned; WAV intact). |
| 4 | Partially fixed | New downloads verified; pre-existing/replaced files trusted. Fix: verify size/sha256/magic before first load; startup migration. |
| 5 | Fixed | Caps and timeouts correct, no off-by-one. |
| 6 | Fixed | Deletion derived from UUID. Stored `audio_path` only used for playback, which the asset scope confines. |
| 7 | Fixed | CSP/asset scope/capabilities correct for Tauri 2.12.1. |
| 8 | Partially fixed | Unix-only; failures only logged. Fix: fail startup when storage can't be secured; Windows DACL. |
| 9 | **Regressed** | Hierarchical summarization has no total/part-count/cost cap: a 100 MB tampered transcript ⇒ ~683 paid Claude calls plus an oversized merge. Fix: hard input/call budgets, per-meeting single-flight, confirmation above an estimate, capped recursive merges. |

New finding (Medium): unbounded billable fan-out in `summarize.rs` (same root cause as #9).

Residual risks, ranked: (1) summary cost amplification, (2) key not bound to origin, (3) prompt injection model-enforced, (4) on-disk models unverified, (5) permissions Unix-only/non-fatal, (6) cloud work not cancelled on stop, (7) RustSec advisories unchecked (cargo-audit not installed).
