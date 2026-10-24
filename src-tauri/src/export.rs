//! Export meetings to SecondBrain, the user's local-first knowledge base,
//! through its REST API (spec v0.4: §5.4 content types, §5.8 transcript
//! segments, §7.3 REST API, Appendix E type schemas).
//!
//! One meeting maps to up to two SecondBrain documents:
//!
//! - a `transcript`, uploaded once as a WebVTT file (multipart `file` +
//!   `metadata`) so SecondBrain's VTT extractor records speaker segments with
//!   `start_ms`/`end_ms`. Its content is immutable afterwards; later exports
//!   only `PATCH` its title and tags (with `If-Match`).
//! - a `note` with the Claude summary (JSON `content`), linked to the meeting by
//!   `source_url: transcriptor://meeting/<id>#summary`. Created once a summary
//!   exists and updated in place when the summary is regenerated.
//!
//! Every `POST` carries an `Idempotency-Key` derived from the meeting id and a
//! hash of the payload, so a retried request never creates a duplicate and a
//! changed payload never reuses a key (ING-2).

use crate::commands::AppState;
use crate::db::{Db, ExportRecord, PendingUpdate};
use crate::models::{
    events, ExportStatus, Meeting, MeetingStatus, SecondBrainStatus, Settings, Speaker,
    TranscriptSegment,
};
use crate::settings::{self, SettingsStore, SECRET_SECONDBRAIN};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use reqwest::header::{ACCEPT, ETAG, IF_MATCH};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per request; `?wait=30` alone may block for up to 30 s on the server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest SecondBrain response body we read.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// Backoff between attempts for transient failures (3 attempts in total).
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
/// `?wait=<seconds>` when "wait for indexing" is on; the spec's maximum.
const WAIT_SECONDS: u32 = 30;

const UNTITLED: &str = "Untitled meeting";
/// Voice name for the user's side when no identity is configured.
const YOU_FALLBACK: &str = "You";
/// Voice name for the other side. Never sent as a participant.
const OTHERS_VOICE: &str = "Others";

// ---------------------------------------------------------------------------
// Documents (pure)

/// Typed properties of a `transcript` document, exactly the keys of
/// `brain:type/transcript/1` (Appendix E; `additionalProperties: false`).
/// SecondBrain derives `occurred_at` from `started_at`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TranscriptProps {
    /// Person references: the configured identity, or empty.
    pub participants: Vec<String>,
    /// RFC3339 UTC, second precision.
    pub started_at: String,
    pub duration_ms: i64,
    /// `file://` URI of the local WAV recording (the audio is not uploaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_url: Option<String>,
}

/// The multipart `metadata` part of the transcript upload.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TranscriptMetadata {
    #[serde(rename = "type")]
    pub doc_type: &'static str,
    pub title: String,
    pub tags: Vec<String>,
    pub source_url: String,
    pub props: TranscriptProps,
}

/// The JSON body that creates the summary `note`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SummaryNote {
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub doc_type: &'static str,
    pub tags: Vec<String>,
    pub source_url: String,
}

/// Everything one export sends.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportPlan {
    pub vtt: String,
    pub metadata: TranscriptMetadata,
    /// `metadata` serialized exactly as it is sent.
    pub metadata_json: String,
    /// None when the meeting has no summary.
    pub summary: Option<SummaryNote>,
}

impl ExportPlan {
    /// Lowercase hex sha256 over the VTT, the metadata, and the summary note;
    /// an automatic export is skipped when this matches the last success.
    pub fn content_hash(&self) -> String {
        let summary = self.summary.as_ref().map(|n| serde_json::to_string(n).unwrap_or_default());
        let bytes = serde_json::to_vec(&json!([self.vtt, self.metadata_json, summary])).unwrap_or_default();
        format!("{:x}", Sha256::digest(&bytes))
    }

    /// `Idempotency-Key` for the transcript upload.
    pub fn transcript_key(&self, meeting_id: &str) -> String {
        idempotency_key(meeting_id, &[&self.vtt, &self.metadata_json])
    }

    /// `Idempotency-Key` for creating the summary note.
    pub fn summary_key(&self, meeting_id: &str) -> Option<String> {
        let note = self.summary.as_ref()?;
        Some(idempotency_key(meeting_id, &[&serde_json::to_string(note).unwrap_or_default()]))
    }
}

/// `transcriptor-<meeting id>-<first 16 hex of sha256(payload parts)>`: stable
/// for an identical payload, different whenever the payload changes, so a key
/// is never reused for another body (`422 idempotency-key-reuse`).
pub fn idempotency_key(meeting_id: &str, payload: &[&str]) -> String {
    let mut h = Sha256::new();
    for part in payload {
        h.update(part.as_bytes());
    }
    let hex = format!("{:x}", h.finalize());
    format!("transcriptor-{meeting_id}-{}", &hex[..16])
}

/// Collapse all whitespace (including newlines) to single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn display_title(meeting: &Meeting) -> String {
    match one_line(&meeting.title) {
        t if t.is_empty() => UNTITLED.to_string(),
        t => t,
    }
}

/// The identity setting on one line (may be empty).
fn identity(settings: &Settings) -> String {
    one_line(&settings.secondbrain_identity)
}

/// `participants[]` for the transcript: the identity alone (emails
/// lower-cased, as SecondBrain normalizes them), or nothing.
fn participants(identity: &str) -> Vec<String> {
    match identity.trim() {
        "" => Vec::new(),
        id if id.contains('@') => vec![id.to_lowercase()],
        id => vec![id.to_string()],
    }
}

/// `started_at` as RFC3339 UTC with second precision; unparseable input is
/// passed through unchanged.
fn rfc3339_utc(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|_| raw.to_string())
}

/// WebVTT cue timestamp `HH:MM:SS.mmm` (hours grow past two digits if needed).
pub fn vtt_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Escape cue text (and voice names) for WebVTT.
fn escape_vtt(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render the transcript as WebVTT with one `<v Speaker>` cue per segment.
/// The user's side is voiced as `identity` ("You" when empty), the other side
/// as "Others". Segments with no text are skipped; a cue whose end is not
/// after its start is stretched to 1 ms. Pure: no I/O. The meeting is not
/// used today (cue times are relative to the recording start).
pub fn build_vtt(_meeting: &Meeting, segments: &[TranscriptSegment], identity: &str) -> String {
    let identity = one_line(identity);
    let you = if identity.is_empty() { YOU_FALLBACK.to_string() } else { identity };
    let mut out = String::from("WEBVTT\n");
    for seg in segments {
        let text = one_line(&seg.text);
        if text.is_empty() {
            continue;
        }
        let start = seg.start_ms.max(0);
        let end = if seg.end_ms > start { seg.end_ms } else { start + 1 };
        let voice = match seg.speaker {
            Speaker::You => you.as_str(),
            Speaker::Others => OTHERS_VOICE,
        };
        let _ = write!(
            out,
            "\n{} --> {}\n<v {}>{}\n",
            vtt_timestamp(start),
            vtt_timestamp(end),
            escape_vtt(voice),
            escape_vtt(&text)
        );
    }
    out
}

/// The transcript upload's `metadata`. Pure.
pub fn build_metadata(meeting: &Meeting, settings: &Settings) -> TranscriptMetadata {
    let recording_url = meeting
        .audio_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .and_then(|p| url::Url::from_file_path(p).ok())
        .map(|u| u.to_string());
    TranscriptMetadata {
        doc_type: "transcript",
        title: display_title(meeting),
        tags: settings.secondbrain_tags.clone(),
        source_url: format!("transcriptor://meeting/{}", meeting.id),
        props: TranscriptProps {
            participants: participants(&identity(settings)),
            started_at: rfc3339_utc(&meeting.started_at),
            duration_ms: meeting.duration_ms.max(0),
            recording_url,
        },
    }
}

/// The summary `note`, or None when the meeting has no summary. Pure.
pub fn build_summary_note(meeting: &Meeting, settings: &Settings) -> Option<SummaryNote> {
    let content = meeting.summary.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let mut tags = settings.secondbrain_tags.clone();
    if !tags.iter().any(|t| t.eq_ignore_ascii_case("summary")) {
        tags.push("summary".to_string());
    }
    Some(SummaryNote {
        title: format!("{} — notes", display_title(meeting)),
        content: content.to_string(),
        doc_type: "note",
        tags,
        source_url: format!("transcriptor://meeting/{}#summary", meeting.id),
    })
}

/// Build everything an export sends. Pure.
pub fn build_plan(meeting: &Meeting, segments: &[TranscriptSegment], settings: &Settings) -> ExportPlan {
    let metadata = build_metadata(meeting, settings);
    let metadata_json = serde_json::to_string(&metadata).unwrap_or_default();
    ExportPlan {
        vtt: build_vtt(meeting, segments, &identity(settings)),
        metadata,
        metadata_json,
        summary: build_summary_note(meeting, settings),
    }
}

// ---------------------------------------------------------------------------
// HTTP transport

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Patch,
}

#[derive(Debug, Clone, PartialEq)]
enum Body {
    Empty,
    Json(Value),
    /// Multipart: `file` (the VTT) + `metadata` (JSON text).
    Vtt { file_name: String, vtt: String, metadata: String },
}

/// One SecondBrain request, independent of the HTTP client (so tests can
/// answer it without a network).
#[derive(Debug, Clone, PartialEq)]
struct Request {
    method: Method,
    /// Relative to the base URL, e.g. `/documents?wait=30`.
    path: String,
    /// Send `Authorization: Bearer <key>`.
    auth: bool,
    /// Sent as `If-Match: "<revision>"`.
    if_match: Option<i64>,
    idempotency_key: Option<String>,
    body: Body,
}

impl Request {
    fn new(method: Method, path: impl Into<String>) -> Self {
        Request { method, path: path.into(), auth: true, if_match: None, idempotency_key: None, body: Body::Empty }
    }
}

#[derive(Debug, Clone)]
struct Response {
    status: StatusCode,
    etag: Option<String>,
    body: String,
}

trait Transport {
    /// Send one request. Errors are transport failures only (no response);
    /// any HTTP status is returned as a `Response`.
    async fn send(&self, req: &Request) -> Result<Response>;
}

struct HttpTransport {
    client: reqwest::Client,
    base: String,
    api_key: String,
}

impl HttpTransport {
    fn new(base: &str, api_key: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .user_agent(concat!("Transcriptor/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client")?;
        Ok(HttpTransport { client, base: base.to_string(), api_key: api_key.to_string() })
    }
}

impl Transport for HttpTransport {
    async fn send(&self, req: &Request) -> Result<Response> {
        let url = endpoint(&self.base, &req.path);
        let mut rb = match req.method {
            Method::Get => self.client.get(&url),
            Method::Post => self.client.post(&url),
            Method::Patch => self.client.patch(&url),
        }
        .header(ACCEPT, "application/json, application/problem+json");
        if req.auth && !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        if let Some(rev) = req.if_match {
            rb = rb.header(IF_MATCH, format!("\"{rev}\""));
        }
        if let Some(key) = &req.idempotency_key {
            rb = rb.header("Idempotency-Key", key);
        }
        rb = match &req.body {
            Body::Empty => rb,
            Body::Json(v) => rb.json(v),
            Body::Vtt { file_name, vtt, metadata } => {
                let file = reqwest::multipart::Part::text(vtt.clone())
                    .file_name(file_name.clone())
                    .mime_str("text/vtt")?;
                let form = reqwest::multipart::Form::new()
                    .part("file", file)
                    .text("metadata", metadata.clone());
                rb.multipart(form)
            }
        };
        let resp = rb.send().await.map_err(|e| anyhow!("could not reach SecondBrain: {e}"))?;
        let status = resp.status();
        let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = crate::net::read_text_capped(resp, MAX_RESPONSE_BYTES, "SecondBrain").await?;
        Ok(Response { status, etag, body })
    }
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

fn clip(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Responses and errors

/// The parts of an RFC 9457 problem+json body we use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Problem {
    /// Last path segment of `type`, e.g. `revision-mismatch`; None for
    /// `about:blank` or a missing `type`.
    pub kind: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    /// From `412 revision-mismatch`.
    pub current_revision: Option<i64>,
}

/// An integer from a JSON number or numeric string.
fn json_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Parse a problem+json body; anything else gives an empty `Problem`.
pub fn parse_problem(body: &str) -> Problem {
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Problem::default() };
    let text = |k: &str| v[k].as_str().map(|s| clip(s, 300)).filter(|s| !s.is_empty());
    let kind = v["type"]
        .as_str()
        .map(|t| t.trim().trim_end_matches('/'))
        .filter(|t| !t.is_empty() && *t != "about:blank")
        .and_then(|t| t.rsplit('/').next())
        .map(|t| clip(t, 80));
    Problem { kind, title: text("title"), detail: text("detail"), current_revision: json_i64(&v["current_revision"]) }
}

/// User-facing message for a non-2xx response.
pub fn problem_message(status: StatusCode, body: &str) -> String {
    let p = parse_problem(body);
    let code = status.as_u16();
    let detail = p.detail.clone().map(|d| format!(": {d}")).unwrap_or_default();
    match p.kind.as_deref() {
        Some("idempotency-key-reuse") => {
            return format!(
                "SecondBrain returned {code} idempotency-key-reuse: the Idempotency-Key was already used for a different payload{detail}"
            )
        }
        Some("revision-mismatch") => {
            let current = p.current_revision.map(|r| format!(" (current revision {r})")).unwrap_or_default();
            return format!(
                "SecondBrain returned {code} revision-mismatch: the document was changed in SecondBrain during the export{current}. Export again."
            );
        }
        Some("validation-error") => {
            return format!("SecondBrain rejected the document as invalid ({code} validation-error){detail}")
        }
        _ => {}
    }
    let reason = match (p.title, p.detail) {
        (Some(t), Some(d)) if t != d => format!("{t}: {d}"),
        (Some(t), _) => t,
        (None, Some(d)) => d,
        (None, None) if serde_json::from_str::<Value>(body).is_err() && !body.trim().is_empty() => clip(body, 200),
        (None, None) => status.canonical_reason().unwrap_or("request failed").to_string(),
    };
    let kind = p.kind.map(|k| format!(" [{k}]")).unwrap_or_default();
    let hint = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            " (check the SecondBrain API key in Settings; it needs the write scope)"
        }
        _ => "",
    };
    format!("SecondBrain returned {code}: {reason}{kind}{hint}")
}

/// Revision from an `ETag` value: `"4"`, `W/"4"`, or `4`.
fn parse_etag(etag: &str) -> Option<i64> {
    let t = etag.trim();
    let t = t.strip_prefix("W/").unwrap_or(t);
    t.trim_matches('"').trim().parse().ok()
}

/// What a document response tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DocInfo {
    id: Option<String>,
    revision: Option<i64>,
    job_id: Option<String>,
}

/// `document.id`/`document.revision`/`job.id` from a `{document, job}`
/// response (or a bare document); the `ETag` header supplies the revision
/// when the body has none.
fn parse_doc_info(resp: &Response) -> DocInfo {
    let v: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
    let doc = if v["document"].is_object() { &v["document"] } else { &v };
    let id = doc["id"].as_str().map(str::to_string).filter(|id| valid_document_id(id));
    let revision = json_i64(&doc["revision"])
        .or_else(|| json_i64(&v["revision"]))
        .or_else(|| resp.etag.as_deref().and_then(parse_etag));
    let job_id = v["job"]["id"].as_str().map(str::to_string).filter(|j| !j.is_empty() && j.len() <= 128);
    DocInfo { id, revision, job_id }
}

/// SecondBrain ids are ULIDs; refuse anything that could change the URL path.
fn valid_document_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why one request failed.
#[derive(Debug)]
enum Failure {
    /// The server answered with a non-2xx status.
    Status { status: StatusCode, problem: Problem, message: String },
    /// Connection, timeout, or body-read failure.
    Transport(anyhow::Error),
}

impl Failure {
    fn transient(&self) -> bool {
        match self {
            Failure::Transport(_) => true,
            Failure::Status { status, .. } => status.is_server_error(),
        }
    }

    fn status(&self) -> Option<StatusCode> {
        match self {
            Failure::Status { status, .. } => Some(*status),
            Failure::Transport(_) => None,
        }
    }

    fn kind(&self) -> Option<&str> {
        match self {
            Failure::Status { problem, .. } => problem.kind.as_deref(),
            Failure::Transport(_) => None,
        }
    }

    fn current_revision(&self) -> Option<i64> {
        match self {
            Failure::Status { problem, .. } => problem.current_revision,
            Failure::Transport(_) => None,
        }
    }

    fn message(&self) -> String {
        match self {
            Failure::Status { message, .. } => message.clone(),
            Failure::Transport(e) => format!("{e:#}"),
        }
    }
}

/// Send once; non-2xx becomes `Failure::Status`.
async fn send_checked<T: Transport>(t: &T, req: &Request) -> Result<Response, Failure> {
    let resp = t.send(req).await.map_err(Failure::Transport)?;
    if resp.status.is_success() {
        Ok(resp)
    } else {
        Err(Failure::Status {
            status: resp.status,
            problem: parse_problem(&resp.body),
            message: problem_message(resp.status, &resp.body),
        })
    }
}

/// Run `op`, retrying transient failures (connection errors, 5xx) after each
/// delay in `delays`. 4xx responses are returned immediately.
async fn with_retries<T, F, Fut>(delays: &[Duration], mut op: F) -> Result<T, Failure>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Failure>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Err(f) if f.transient() && attempt < delays.len() => {
                log::warn!(
                    "SecondBrain request failed (attempt {} of {}): {}; retrying",
                    attempt + 1,
                    delays.len() + 1,
                    f.message()
                );
                tokio::time::sleep(delays[attempt]).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

// ---------------------------------------------------------------------------
// Export flow

/// Outcome of `GET /documents/{id}` when looking up a revision.
enum Fetched {
    Revision(i64),
    Gone,
}

enum Updated {
    Committed(DocInfo),
    AwaitingApproval(PendingUpdate),
    Gone,
}

fn fields_match(document: &Value, expected: &Value) -> bool {
    expected.as_object().is_some_and(|fields| fields.iter().all(|(key, value)| {
        let actual = document.get(key).or_else(|| (key == "content").then(|| document.get("text")).flatten());
        if key == "tags" {
            let strings = |v: &Value| -> Option<std::collections::BTreeSet<String>> {
                v.as_array()?.iter().map(|v| v.as_str().map(str::to_owned)).collect()
            };
            actual.and_then(strings).zip(strings(value)).is_some_and(|(a, b)| a == b)
        } else {
            actual == Some(value)
        }
    }))
}

fn fields_hash(body: &Value) -> String {
    format!("{:x}", Sha256::digest(body.to_string().as_bytes()))
}

struct Exporter<'a, T: Transport> {
    t: &'a T,
    delays: &'a [Duration],
    /// Append `?wait=30` to `POST /documents`.
    wait: bool,
}

impl<T: Transport> Exporter<'_, T> {
    async fn send(&self, req: &Request) -> Result<Response, Failure> {
        with_retries(self.delays, || send_checked(self.t, req)).await
    }

    fn create_path(&self) -> String {
        if self.wait {
            format!("/documents?wait={WAIT_SECONDS}")
        } else {
            "/documents".to_string()
        }
    }

    /// `POST /documents`. On `422 idempotency-key-reuse` (which the hashed key
    /// should make impossible) retries once with a uuid-suffixed key.
    async fn create(&self, body: Body, key: String) -> Result<DocInfo, Failure> {
        let mut req = Request::new(Method::Post, self.create_path());
        req.idempotency_key = Some(key);
        req.body = body;
        let resp = match self.send(&req).await {
            Err(f) if f.kind() == Some("idempotency-key-reuse") => {
                log::warn!("SecondBrain reported idempotency-key-reuse; retrying with a fresh key");
                req.idempotency_key = req.idempotency_key.map(|k| format!("{k}-{}", uuid::Uuid::new_v4()));
                self.send(&req).await?
            }
            other => other?,
        };
        let info = parse_doc_info(&resp);
        if info.id.is_none() {
            return Err(Failure::Transport(anyhow!(
                "SecondBrain's response did not include a valid document id"
            )));
        }
        Ok(info)
    }

    /// The current revision of a document (`document.revision` or `ETag`).
    async fn fetch_revision(&self, id: &str) -> Result<Fetched, Failure> {
        match self.send(&Request::new(Method::Get, format!("/documents/{id}"))).await {
            Ok(resp) => parse_doc_info(&resp).revision.map(Fetched::Revision).ok_or_else(|| {
                Failure::Transport(anyhow!("SecondBrain did not report the revision of document {id}"))
            }),
            Err(f) if f.status() == Some(StatusCode::NOT_FOUND) => Ok(Fetched::Gone),
            Err(f) => Err(f),
        }
    }

    /// `PATCH /documents/{id}` with `If-Match`. Looks the revision up first
    /// when it is unknown; on `412` takes `current_revision` from the problem
    /// (or re-reads the document) and retries once. Ok(None) means the
    /// document no longer exists (404).
    async fn update(&self, id: &str, revision: Option<i64>, body: Value, pending: Option<&PendingUpdate>) -> Result<Updated, Failure> {
        let mut revision = revision;
        if let Some(pending) = pending {
            let response = match self.send(&Request::new(Method::Get, format!("/documents/{id}?include=text"))).await {
                Ok(response) => response,
                Err(f) if f.status() == Some(StatusCode::NOT_FOUND) => return Ok(Updated::Gone),
                Err(f) => return Err(f),
            };
            let info = parse_doc_info(&response);
            let value: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            let document = if value["document"].is_object() { &value["document"] } else { &value };
            if info.id.as_deref() != Some(id)
                || !info.revision.is_some_and(|r| r > pending.revision)
                || !fields_match(document, &pending.body)
            {
                return Ok(Updated::AwaitingApproval(pending.clone()));
            }
            // A read confirms the proposed values were committed. If the local
            // meeting changed meanwhile, propose its latest values separately.
            if fields_match(document, &body) {
                return Ok(Updated::Committed(info));
            }
            revision = info.revision;
        }
        let mut rev = match revision {
            Some(r) => r,
            None => match self.fetch_revision(id).await? {
                Fetched::Revision(r) => r,
                Fetched::Gone => return Ok(Updated::Gone),
            },
        };
        let mut retried = false;
        loop {
            let mut req = Request::new(Method::Patch, format!("/documents/{id}"));
            req.if_match = Some(rev);
            req.body = Body::Json(body.clone());
            // PATCH is not idempotent: retrying a lost response can create a
            // duplicate approval proposal. Only retry an explicit 412 below.
            match send_checked(self.t, &req).await {
                Ok(resp) => {
                    if resp.status == StatusCode::ACCEPTED {
                        let value: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
                        let pending_id = value["pending_id"].as_str().filter(|id| valid_document_id(id))
                            .ok_or_else(|| Failure::Transport(anyhow!("SecondBrain accepted an update without a valid pending_id; its outcome is unknown")))?;
                        return Ok(Updated::AwaitingApproval(PendingUpdate {
                            pending_id: pending_id.to_string(), document_id: id.to_string(), revision: rev, body,
                        }));
                    }
                    let mut info = parse_doc_info(&resp);
                    info.id = Some(info.id.unwrap_or_else(|| id.to_string()));
                    if info.id.as_deref() != Some(id) || info.revision.is_none() {
                        return Err(Failure::Transport(anyhow!("SecondBrain update did not confirm the document revision")));
                    }
                    return Ok(Updated::Committed(info));
                }
                Err(f) if f.status() == Some(StatusCode::NOT_FOUND) => return Ok(Updated::Gone),
                Err(f) if f.status() == Some(StatusCode::PRECONDITION_FAILED) && !retried => {
                    retried = true;
                    rev = match next_revision(&f) {
                        Some(r) => r,
                        None => match self.fetch_revision(id).await? {
                            Fetched::Revision(r) => r,
                            Fetched::Gone => return Ok(Updated::Gone),
                        },
                    };
                    log::info!("SecondBrain document {id} moved to revision {rev}; retrying the update");
                }
                Err(f) => return Err(f),
            }
        }
    }
}

/// True means the document exists (possibly awaiting approval), false means
/// it must be created. Hashes only advance after a committed response/read.
async fn sync_document<T: Transport>(
    ex: &Exporter<'_, T>, id: &str, revision: &mut Option<i64>, hash: &mut Option<String>,
    pending: &mut Vec<PendingUpdate>, body: Value,
) -> Result<bool, Failure> {
    let wanted_hash = fields_hash(&body);
    let previous = pending.iter().find(|p| p.document_id == id);
    if previous.is_none() && hash.as_deref() == Some(&wanted_hash) {
        // A manual re-export still checks for remote deletion or edits. Avoid
        // a title PATCH (which requires approval) when values already match.
        match ex.send(&Request::new(Method::Get, format!("/documents/{id}?include=text"))).await {
            Ok(response) => {
                let info = parse_doc_info(&response);
                let value: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
                let doc = if value["document"].is_object() { &value["document"] } else { &value };
                if info.id.as_deref() == Some(id) && info.revision.is_some() && fields_match(doc, &body) {
                    *revision = info.revision;
                    return Ok(true);
                }
            }
            Err(f) if f.status() == Some(StatusCode::NOT_FOUND) => {
                *revision = None;
                *hash = None;
                return Ok(false);
            }
            Err(f) => return Err(f),
        }
    }
    let outcome = ex.update(id, *revision, body, previous).await?;
    pending.retain(|p| p.document_id != id);
    match outcome {
        Updated::Committed(info) => { *revision = info.revision; *hash = Some(wanted_hash); Ok(true) }
        Updated::AwaitingApproval(proposal) => { pending.push(proposal); Ok(true) }
        Updated::Gone => { *revision = None; *hash = None; Ok(false) }
    }
}

/// The revision to retry a `412` with, when the problem body carries it;
/// None means "re-read the document".
fn next_revision(f: &Failure) -> Option<i64> {
    f.current_revision()
}

/// Send `plan` to SecondBrain, updating `record` as each step succeeds (so a
/// partial failure still remembers what was created). Returns true when a
/// new transcript document was created.
async fn run_export<T: Transport>(
    ex: &Exporter<'_, T>,
    plan: &ExportPlan,
    meeting_id: &str,
    record: &mut ExportRecord,
    checkpoint: impl Fn(&ExportRecord) -> Result<()>,
) -> Result<bool> {
    // Transcript: upload once, afterwards only title and tags change.
    let mut fresh_key = false;
    let mut created = false;
    let transcript_body = json!({ "title": plan.metadata.title, "tags": plan.metadata.tags });
    let existing = record.document_id.clone().filter(|id| valid_document_id(id));
    let updated = match existing {
        Some(id) => {
            match sync_document(ex, &id, &mut record.revision, &mut record.sync.transcript_hash,
                &mut record.sync.pending, transcript_body.clone()).await {
                Ok(true) => true,
                Ok(false) => {
                    log::info!("SecondBrain transcript {id} no longer exists; uploading a new one");
                    // The hashed key would replay the response for the deleted
                    // document for 24 h, so use a fresh one.
                    fresh_key = true;
                    false
                }
                Err(f) => return Err(anyhow!("Transcript: {}", f.message())),
            }
        }
        None => false,
    };
    if !updated {
        let mut key = plan.transcript_key(meeting_id);
        if fresh_key {
            key = format!("{key}-{}", uuid::Uuid::new_v4());
        }
        let body = Body::Vtt {
            file_name: format!("{meeting_id}.vtt"),
            vtt: plan.vtt.clone(),
            metadata: plan.metadata_json.clone(),
        };
        let info = ex.create(body, key).await.map_err(|f| anyhow!("Transcript: {}", f.message()))?;
        record.document_id = info.id;
        record.revision = info.revision;
        record.job_id = info.job_id;
        record.sync.transcript_hash = Some(fields_hash(&transcript_body));
        created = true;
    }
    // Acknowledged ids/proposals must survive a crash or a failure in the
    // following request, not just a completely successful export.
    checkpoint(record)?;

    // Summary note.
    if let Some(note) = &plan.summary {
        let mut fresh_key = false;
        let existing = record.summary_document_id.clone().filter(|id| valid_document_id(id));
        let mut done = false;
        let note_body = json!({ "title": note.title, "content": note.content, "tags": note.tags });
        if let Some(id) = existing {
            match sync_document(ex, &id, &mut record.summary_revision, &mut record.sync.summary_hash,
                &mut record.sync.pending, note_body.clone()).await {
                Ok(true) => { done = true; }
                Ok(false) => {
                    log::info!("SecondBrain notes document {id} no longer exists; creating a new one");
                    fresh_key = true;
                }
                Err(f) => return Err(anyhow!("Notes: {}", f.message())),
            }
        }
        if !done {
            let mut key = plan.summary_key(meeting_id).unwrap_or_default();
            if fresh_key {
                key = format!("{key}-{}", uuid::Uuid::new_v4());
            }
            let body = Body::Json(serde_json::to_value(note).unwrap_or(Value::Null));
            let info = ex.create(body, key).await.map_err(|f| anyhow!("Notes: {}", f.message()))?;
            record.summary_document_id = info.id;
            record.summary_revision = info.revision;
            record.sync.summary_hash = Some(fields_hash(&note_body));
        }
        checkpoint(record)?;
    }
    Ok(created)
}

/// Export a meeting over HTTP, updating `record` as it goes.
pub async fn export_meeting(
    settings: &Settings,
    api_key: &str,
    meeting: &Meeting,
    segments: &[TranscriptSegment],
    record: &mut ExportRecord,
    checkpoint: impl Fn(&ExportRecord) -> Result<()>,
) -> Result<bool> {
    if record.sync.destination.as_ref().is_some_and(|d| d != &settings.secondbrain_url) {
        *record = ExportRecord::default();
    }
    record.sync.destination = Some(settings.secondbrain_url.clone());
    let plan = build_plan(meeting, segments, settings);
    let transport = HttpTransport::new(&settings.secondbrain_url, api_key, REQUEST_TIMEOUT)?;
    let ex = Exporter { t: &transport, delays: &RETRY_DELAYS, wait: settings.secondbrain_wait_for_index };
    run_export(&ex, &plan, &meeting.id, record, checkpoint).await
}

// ---------------------------------------------------------------------------
// Test connection

/// Result of a connection check.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionCheck {
    reachable: Option<bool>,
    key_ok: Option<bool>,
    message: String,
}

/// `GET /health` (no auth), then `GET /stats` with the key when there is one.
async fn check_connection<T: Transport>(t: &T, base: &str, has_key: bool) -> ConnectionCheck {
    let mut health = Request::new(Method::Get, "/health");
    health.auth = false;
    match send_checked(t, &health).await {
        Ok(_) => {}
        Err(Failure::Transport(e)) => {
            return ConnectionCheck { reachable: Some(false), key_ok: None, message: format!("{e:#}") }
        }
        Err(Failure::Status { status, message, .. }) => {
            let hint = if status == StatusCode::NOT_FOUND {
                " Check the API URL (it usually ends in /v1)."
            } else {
                ""
            };
            return ConnectionCheck {
                reachable: Some(false),
                key_ok: None,
                message: format!("{base}/health did not answer as SecondBrain does. {message}.{hint}"),
            };
        }
    }
    if !has_key {
        return ConnectionCheck {
            reachable: Some(true),
            key_ok: None,
            message: format!("SecondBrain at {base} is reachable, but no API key is saved."),
        };
    }
    match send_checked(t, &Request::new(Method::Get, "/stats")).await {
        Ok(_) => ConnectionCheck {
            reachable: Some(true),
            key_ok: Some(true),
            message: format!("Connected to SecondBrain at {base}."),
        },
        Err(Failure::Status { status: StatusCode::UNAUTHORIZED, .. }) => ConnectionCheck {
            reachable: Some(true),
            key_ok: Some(false),
            message: "The server is reachable but the API key was rejected (401).".to_string(),
        },
        Err(Failure::Status { status: StatusCode::FORBIDDEN, .. }) => ConnectionCheck {
            reachable: Some(true),
            key_ok: Some(false),
            message: "The server is reachable but the API key was rejected (403). This test reads /stats, \
                      which needs the read scope; exporting needs write."
                .to_string(),
        },
        Err(f) => ConnectionCheck {
            reachable: Some(true),
            key_ok: None,
            message: format!("SecondBrain is reachable, but checking the API key failed: {}", f.message()),
        },
    }
}

/// The `secondbrain_status` command.
pub async fn status(settings: &Settings) -> SecondBrainStatus {
    let enabled = settings.secondbrain_enabled;
    let key = settings::get_secret(SECRET_SECONDBRAIN);
    let has_key = matches!(key, Ok(Some(_)));
    if !enabled {
        return SecondBrainStatus {
            enabled,
            has_key,
            reachable: None,
            key_ok: None,
            message: "SecondBrain export is turned off. Enable it and save to test the connection.".to_string(),
        };
    }
    let api_key = match key {
        Ok(k) => k.unwrap_or_default(),
        Err(e) => {
            return SecondBrainStatus {
                enabled,
                has_key,
                reachable: None,
                key_ok: None,
                message: format!("Couldn't read the SecondBrain API key: {e:#}"),
            }
        }
    };
    let base = settings.secondbrain_url.trim_end_matches('/').to_string();
    let check = match HttpTransport::new(&base, &api_key, HEALTH_TIMEOUT) {
        Ok(t) => {
            let limit = HEALTH_TIMEOUT * 2 + Duration::from_secs(1);
            match tokio::time::timeout(limit, check_connection(&t, &base, has_key)).await {
                Ok(c) => c,
                Err(_) => ConnectionCheck {
                    reachable: Some(false),
                    key_ok: None,
                    message: "SecondBrain did not answer in time.".to_string(),
                },
            }
        }
        Err(e) => ConnectionCheck { reachable: None, key_ok: None, message: format!("{e:#}") },
    };
    SecondBrainStatus { enabled, has_key, reachable: check.reachable, key_ok: check.key_ok, message: check.message }
}

// ---------------------------------------------------------------------------
// Orchestration: status bookkeeping, triggers, one export per meeting at a time

/// What asked for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The user pressed "Export"; always runs (if enabled with a key).
    Manual,
    /// Transcription finished.
    Transcribed,
    /// A summary was (re)generated.
    Summarized,
}

/// Whether `trigger` should export `meeting` under `settings`.
pub fn should_export(settings: &Settings, has_key: bool, meeting: &Meeting, trigger: Trigger) -> bool {
    if !settings.secondbrain_enabled || !has_key {
        return false;
    }
    match trigger {
        Trigger::Manual => true,
        Trigger::Transcribed => settings.secondbrain_auto_export,
        Trigger::Summarized => settings.secondbrain_auto_export || meeting.export_document_id.is_some(),
    }
}

/// Meetings with an export running, mapped to "run again when it finishes"
/// (set when another automatic trigger arrives mid-export).
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Default::default);

/// Exclusive right to export one meeting; released on drop.
struct Claim {
    id: String,
    released: bool,
}

/// Claim `id`. If it is already being exported, returns None, and when
/// `queue_rerun` is set asks the running export to go again afterwards.
fn claim(id: &str, queue_rerun: bool) -> Option<Claim> {
    let mut map = IN_FLIGHT.lock();
    if let Some(rerun) = map.get_mut(id) {
        *rerun |= queue_rerun;
        return None;
    }
    map.insert(id.to_string(), false);
    Some(Claim { id: id.to_string(), released: false })
}

impl Claim {
    /// True if a rerun was queued (and clears it); otherwise releases the
    /// claim atomically so no queued request can be lost in between.
    fn finish_or_rerun(&mut self) -> bool {
        let mut map = IN_FLIGHT.lock();
        match map.get_mut(&self.id) {
            Some(rerun) if *rerun => {
                *rerun = false;
                true
            }
            _ => {
                map.remove(&self.id);
                self.released = true;
                false
            }
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.released {
            IN_FLIGHT.lock().remove(&self.id);
        }
    }
}

fn emit_meeting(app: &AppHandle, db: &Db, id: &str) -> Option<Meeting> {
    let m = db.require_meeting(id).ok()?;
    let _ = app.emit(events::MEETING_UPDATED, m.clone());
    Some(m)
}

/// Export one meeting and record the outcome in the database. With
/// `skip_unchanged`, does nothing when the transcript, its metadata, and the
/// summary are identical to the last successful export.
async fn export_once(
    app: &AppHandle,
    db: &Db,
    settings: &Settings,
    id: &str,
    skip_unchanged: bool,
) -> Result<Meeting> {
    let api_key = settings::require_secret(SECRET_SECONDBRAIN, "SecondBrain API key")?;
    let meeting = db.require_meeting(id)?;
    match meeting.status {
        MeetingStatus::Recording => bail!("This meeting is still recording."),
        MeetingStatus::Processing => bail!("This meeting is still being transcribed."),
        MeetingStatus::Done | MeetingStatus::Failed => {}
    }
    let segments = db.segments_for(id)?;
    if segments.iter().all(|s| s.text.trim().is_empty()) {
        bail!("This meeting has no transcript to export.");
    }
    let content_hash = build_plan(&meeting, &segments, settings).content_hash();
    let mut record = db.export_record(id)?;
    if skip_unchanged
        && meeting.export_status == Some(ExportStatus::Exported)
        && record.sync.pending.is_empty()
        && record.sync.destination.as_deref() == Some(&settings.secondbrain_url)
        && db.export_content_hash(id)?.as_deref() == Some(content_hash.as_str())
    {
        log::debug!("meeting {id} unchanged since the last SecondBrain export; skipping");
        return Ok(meeting);
    }

    db.set_export_pending(id)?;
    emit_meeting(app, db, id);
    let result = export_meeting(settings, &api_key, &meeting, &segments, &mut record,
        |record| db.checkpoint_export(id, record)).await;
    match result {
        Ok(created) => {
            if !record.sync.pending.is_empty() {
                db.set_export_awaiting_approval(id, &record)?;
                return emit_meeting(app, db, id).ok_or_else(|| anyhow!("meeting {id} not found"));
            }
            log::info!(
                "{} SecondBrain transcript {} for meeting {id} (notes: {})",
                if created { "created" } else { "updated" },
                record.document_id.as_deref().unwrap_or("?"),
                record.summary_document_id.as_deref().unwrap_or("none"),
            );
            db.set_export_success(id, &record, &content_hash, &chrono::Utc::now().to_rfc3339())?;
            emit_meeting(app, db, id).ok_or_else(|| anyhow!("meeting {id} not found"))
        }
        Err(e) => {
            db.set_export_failed(id, &format!("{e:#}"), &record)?;
            emit_meeting(app, db, id);
            Err(e)
        }
    }
}

/// Export with `claim` held, then keep going while reruns are queued.
async fn auto_loop(app: AppHandle, db: Arc<Db>, store: Arc<SettingsStore>, id: String, mut claim: Claim) {
    loop {
        let settings = store.get();
        if settings.secondbrain_enabled {
            if let Err(e) = export_once(&app, &db, &settings, &id, true).await {
                log::warn!("automatic SecondBrain export of meeting {id} failed: {e:#}");
            }
        }
        if !claim.finish_or_rerun() {
            break;
        }
    }
}

/// The `export_meeting` command: export now, whatever the auto-export setting.
pub async fn export_now(app: &AppHandle, db: Arc<Db>, store: Arc<SettingsStore>, id: &str) -> Result<Meeting> {
    let settings = store.get();
    if !settings.secondbrain_enabled {
        bail!("SecondBrain export is turned off. Enable it in Settings.");
    }
    let mut claim = claim(id, false).ok_or_else(|| anyhow!("This meeting is already being exported."))?;
    let result = export_once(app, &db, &settings, id, false).await;
    if claim.finish_or_rerun() {
        // An automatic trigger (e.g. a new summary) arrived meanwhile.
        let (app, id) = (app.clone(), id.to_string());
        tauri::async_runtime::spawn(auto_loop(app, db, store, id, claim));
    }
    result
}

/// Start a background export for an automatic trigger if the settings call
/// for one. Never blocks on the network; failures land in `export_error`.
pub fn spawn_auto_export(app: &AppHandle, meeting_id: &str, trigger: Trigger) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let settings = state.settings.get();
    if !settings.secondbrain_enabled {
        return;
    }
    let Ok(meeting) = state.db.require_meeting(meeting_id) else { return };
    if !should_export(&settings, settings::has_secret(SECRET_SECONDBRAIN), &meeting, trigger) {
        return;
    }
    let Some(claim) = claim(meeting_id, true) else {
        log::debug!("SecondBrain export of {meeting_id} already running; queued another pass");
        return;
    };
    let (app, db, store, id) = (app.clone(), state.db.clone(), state.settings.clone(), meeting_id.to_string());
    tauri::async_runtime::spawn(auto_loop(app, db, store, id, claim));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TranscriptionMode;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MID: &str = "6f1c2a9e-0000-4000-8000-000000000001";

    fn meeting() -> Meeting {
        Meeting {
            id: MID.into(),
            title: "Roadmap sync".into(),
            started_at: "2026-10-08T12:00:00.123456+02:00".into(),
            ended_at: Some("2026-10-08T12:31:00+02:00".into()),
            duration_ms: 1_859_600,
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

    fn seg(speaker: Speaker, start_ms: i64, end_ms: i64, text: &str) -> TranscriptSegment {
        TranscriptSegment { id: start_ms, meeting_id: "m".into(), speaker, start_ms, end_ms, text: text.into() }
    }

    fn settings_with_identity(identity: &str) -> Settings {
        Settings { secondbrain_identity: identity.into(), ..Default::default() }
    }

    // --- VTT -------------------------------------------------------------

    #[test]
    fn builds_vtt_with_voices_and_escaping() {
        let segs = [
            seg(Speaker::You, 5_000, 9_250, " Let's start with the roadmap. "),
            seg(Speaker::Others, 65_000, 70_500, "Sounds good,\nI have <notes> & R&D > Q4."),
            seg(Speaker::Others, 71_000, 72_000, "   "),
        ];
        let vtt = build_vtt(&meeting(), &segs, "Justin Leahy");
        assert_eq!(
            vtt,
            "WEBVTT\n\
             \n00:00:05.000 --> 00:00:09.250\n<v Justin Leahy>Let's start with the roadmap.\n\
             \n00:01:05.000 --> 00:01:10.500\n<v Others>Sounds good, I have &lt;notes&gt; &amp; R&amp;D &gt; Q4.\n"
        );
    }

    #[test]
    fn vtt_identity_falls_back_to_you_and_is_escaped() {
        let segs = [seg(Speaker::You, 0, 1000, "hi")];
        assert!(build_vtt(&meeting(), &segs, "").contains("<v You>hi\n"));
        assert!(build_vtt(&meeting(), &segs, "  \n ").contains("<v You>hi\n"));
        assert!(build_vtt(&meeting(), &segs, "justin@example.com").contains("<v justin@example.com>hi\n"));
        assert!(build_vtt(&meeting(), &segs, "A <b> & C\nD").contains("<v A &lt;b&gt; &amp; C D>hi\n"));
        assert_eq!(build_vtt(&meeting(), &[], "x"), "WEBVTT\n");
    }

    #[test]
    fn vtt_timestamps_past_an_hour_and_end_bump() {
        assert_eq!(vtt_timestamp(0), "00:00:00.000");
        assert_eq!(vtt_timestamp(3_599_999), "00:59:59.999");
        assert_eq!(vtt_timestamp(3_600_000), "01:00:00.000");
        assert_eq!(vtt_timestamp(3_725_042), "01:02:05.042");
        assert_eq!(vtt_timestamp(100 * 3_600_000 + 1), "100:00:00.001");
        assert_eq!(vtt_timestamp(-5), "00:00:00.000");

        let segs = [
            seg(Speaker::You, 3_725_000, 3_725_000, "equal"),
            seg(Speaker::Others, 4_000_000, 3_999_000, "backwards"),
            seg(Speaker::You, -20, -10, "negative"),
        ];
        let vtt = build_vtt(&meeting(), &segs, "");
        assert!(vtt.contains("\n01:02:05.000 --> 01:02:05.001\n<v You>equal\n"));
        assert!(vtt.contains("\n01:06:40.000 --> 01:06:40.001\n<v Others>backwards\n"));
        assert!(vtt.contains("\n00:00:00.000 --> 00:00:00.001\n<v You>negative\n"));
    }

    // --- metadata and notes ---------------------------------------------

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn metadata_shape_matches_transcript_schema() {
        let m = meeting();
        let plan = build_plan(&m, &[seg(Speaker::You, 0, 1000, "hi")], &settings_with_identity("Justin@Example.com"));
        let v: Value = serde_json::from_str(&plan.metadata_json).unwrap();
        assert_eq!(keys(&v), vec!["props", "source_url", "tags", "title", "type"]);
        assert_eq!(v["type"], "transcript");
        assert_eq!(v["title"], "Roadmap sync");
        assert_eq!(v["tags"], json!(["meeting", "transcriptor"]));
        assert_eq!(v["source_url"], format!("transcriptor://meeting/{MID}"));
        // No recording: exactly the three required props, all in the schema.
        assert_eq!(keys(&v["props"]), vec!["duration_ms", "participants", "started_at"]);
        assert_eq!(v["props"]["participants"], json!(["justin@example.com"]));
        assert_eq!(v["props"]["started_at"], "2026-10-08T10:00:00Z");
        assert_eq!(v["props"]["duration_ms"], json!(1_859_600));
        let schema = ["participants", "started_at", "duration_ms", "event_id", "recording_url"];
        assert!(keys(&v["props"]).iter().all(|k| schema.contains(&k.as_str())));

        #[cfg(unix)]
        {
            let mut m = meeting();
            m.audio_path = Some("/Users/me/Library/Application Support/rec/a.wav".into());
            let v = serde_json::to_value(build_metadata(&m, &Settings::default())).unwrap();
            assert_eq!(keys(&v["props"]), vec!["duration_ms", "participants", "recording_url", "started_at"]);
            assert_eq!(v["props"]["recording_url"], "file:///Users/me/Library/Application%20Support/rec/a.wav");
            m.audio_path = Some("relative/a.wav".into());
            assert_eq!(build_metadata(&m, &Settings::default()).props.recording_url, None);
        }
    }

    #[test]
    fn participants_never_include_others() {
        let m = meeting();
        let segs = [seg(Speaker::Others, 0, 1000, "hi"), seg(Speaker::You, 1000, 2000, "hey")];
        assert!(build_metadata(&m, &Settings::default()).props.participants.is_empty());
        assert!(build_metadata(&m, &settings_with_identity("   ")).props.participants.is_empty());
        assert_eq!(build_metadata(&m, &settings_with_identity(" Justin  Leahy ")).props.participants, vec!["Justin Leahy"]);
        let plan = build_plan(&m, &segs, &settings_with_identity("Justin"));
        assert!(!plan.metadata_json.contains("Others"));
    }

    #[test]
    fn title_and_time_normalization() {
        let mut m = meeting();
        m.title = "  Weekly\nsync  ".into();
        assert_eq!(build_metadata(&m, &Settings::default()).title, "Weekly sync");
        m.title = " ".into();
        assert_eq!(build_metadata(&m, &Settings::default()).title, "Untitled meeting");
        m.started_at = "yesterday".into();
        m.duration_ms = -4;
        let md = build_metadata(&m, &Settings::default());
        assert_eq!(md.props.started_at, "yesterday");
        assert_eq!(md.props.duration_ms, 0);
    }

    #[test]
    fn summary_note_body() {
        let mut m = meeting();
        assert_eq!(build_summary_note(&m, &Settings::default()), None);
        m.summary = Some("  \n".into());
        assert_eq!(build_summary_note(&m, &Settings::default()), None);
        m.summary = Some("\n## Summary\nWe agreed on Q4.\n".into());
        let note = build_summary_note(&m, &Settings::default()).unwrap();
        let v = serde_json::to_value(&note).unwrap();
        assert_eq!(keys(&v), vec!["content", "source_url", "tags", "title", "type"]);
        assert_eq!(v["title"], "Roadmap sync — notes");
        assert_eq!(v["content"], "## Summary\nWe agreed on Q4.");
        assert_eq!(v["type"], "note");
        assert_eq!(v["tags"], json!(["meeting", "transcriptor", "summary"]));
        assert_eq!(v["source_url"], format!("transcriptor://meeting/{MID}#summary"));
        // "summary" isn't duplicated when it is already a configured tag.
        let s = Settings { secondbrain_tags: vec!["Summary".into()], ..Default::default() };
        assert_eq!(build_summary_note(&m, &s).unwrap().tags, vec!["Summary"]);
    }

    #[test]
    fn idempotency_keys_are_stable_and_track_payload() {
        let m = meeting();
        let segs = [seg(Speaker::You, 0, 1000, "hello")];
        let s = Settings::default();
        let a = build_plan(&m, &segs, &s);
        let key = a.transcript_key(MID);
        assert!(key.starts_with(&format!("transcriptor-{MID}-")));
        let suffix = &key[format!("transcriptor-{MID}-").len()..];
        assert_eq!(suffix.len(), 16);
        assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key, build_plan(&m, &segs, &s).transcript_key(MID));
        assert_eq!(a.summary_key(MID), None);

        // A new title changes the metadata and so the key.
        let mut m2 = m.clone();
        m2.title = "Other".into();
        assert_ne!(key, build_plan(&m2, &segs, &s).transcript_key(MID));
        // A new summary does not touch the transcript key but adds a note key.
        let mut m3 = m.clone();
        m3.summary = Some("Notes".into());
        let p3 = build_plan(&m3, &segs, &s);
        assert_eq!(key, p3.transcript_key(MID));
        let nk = p3.summary_key(MID).unwrap();
        assert_ne!(nk, key);
        m3.summary = Some("Other notes".into());
        assert_ne!(nk, build_plan(&m3, &segs, &s).summary_key(MID).unwrap());
        assert_eq!(idempotency_key("x", &["ab", "c"]), idempotency_key("x", &["abc"]));
    }

    #[test]
    fn content_hash_covers_vtt_metadata_and_summary() {
        let m = meeting();
        let segs = [seg(Speaker::You, 0, 1000, "hello")];
        let s = Settings::default();
        let a = build_plan(&m, &segs, &s).content_hash();
        assert_eq!(a.len(), 64);
        assert_eq!(a, build_plan(&m, &segs, &s).content_hash());
        let mut m2 = m.clone();
        m2.summary = Some("New summary".into());
        assert_ne!(a, build_plan(&m2, &segs, &s).content_hash());
        assert_ne!(a, build_plan(&m, &[seg(Speaker::You, 0, 1000, "bye")], &s).content_hash());
        assert_ne!(a, build_plan(&m, &segs, &settings_with_identity("Justin")).content_hash());
        // Export bookkeeping is not part of what is sent.
        let mut m3 = m.clone();
        m3.export_status = Some(ExportStatus::Exported);
        m3.export_document_id = Some("01K70Q3V8X".into());
        m3.export_summary_document_id = Some("01K70Q3V8Y".into());
        assert_eq!(a, build_plan(&m3, &segs, &s).content_hash());
    }

    // --- problems and responses -------------------------------------------

    #[test]
    fn parses_problem_types() {
        let p = parse_problem(
            r#"{"type":"https://secondbrain.dev/problems/revision-mismatch","title":"Document revision has changed","status":412,"detail":"If-Match was 3; the current revision is 4.","current_revision":4}"#,
        );
        assert_eq!(p.kind.as_deref(), Some("revision-mismatch"));
        assert_eq!(p.current_revision, Some(4));
        assert_eq!(p.detail.as_deref(), Some("If-Match was 3; the current revision is 4."));
        assert_eq!(parse_problem(r#"{"type":"validation-error"}"#).kind.as_deref(), Some("validation-error"));
        assert_eq!(parse_problem(r#"{"type":"https://x/problems/idempotency-key-reuse/"}"#).kind.as_deref(), Some("idempotency-key-reuse"));
        assert_eq!(parse_problem(r#"{"type":"about:blank","title":"Not Found"}"#).kind, None);
        assert_eq!(parse_problem(r#"{"current_revision":"7"}"#).current_revision, Some(7));
        assert_eq!(parse_problem("not json"), Problem::default());
    }

    #[test]
    fn problem_messages() {
        let body = r#"{"type":"https://secondbrain.dev/problems/not-found","title":"Document not found","status":404,"detail":"No document with id 01K70X"}"#;
        assert_eq!(
            problem_message(StatusCode::NOT_FOUND, body),
            "SecondBrain returned 404: Document not found: No document with id 01K70X [not-found]"
        );
        let m = problem_message(
            StatusCode::PRECONDITION_FAILED,
            r#"{"type":"https://secondbrain.dev/problems/revision-mismatch","current_revision":4}"#,
        );
        assert!(m.contains("revision-mismatch") && m.contains("current revision 4") && m.contains("Export again"), "{m}");
        let m = problem_message(
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"type":"https://secondbrain.dev/problems/idempotency-key-reuse","detail":"payload hash differs"}"#,
        );
        assert!(m.contains("Idempotency-Key was already used") && m.ends_with(": payload hash differs"), "{m}");
        let m = problem_message(
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"type":"https://secondbrain.dev/problems/validation-error","detail":"props.duration: unknown property"}"#,
        );
        assert_eq!(m, "SecondBrain rejected the document as invalid (422 validation-error): props.duration: unknown property");
        assert_eq!(
            problem_message(StatusCode::UNAUTHORIZED, r#"{"title":"Unauthorized","detail":"Unauthorized"}"#),
            "SecondBrain returned 401: Unauthorized (check the SecondBrain API key in Settings; it needs the write scope)"
        );
        assert_eq!(problem_message(StatusCode::BAD_GATEWAY, "upstream down"), "SecondBrain returned 502: upstream down");
        assert_eq!(problem_message(StatusCode::SERVICE_UNAVAILABLE, ""), "SecondBrain returned 503: Service Unavailable");
        assert_eq!(problem_message(StatusCode::NOT_FOUND, "{}"), "SecondBrain returned 404: Not Found");
        let long = format!(r#"{{"title":"{}"}}"#, "x".repeat(1000));
        assert!(problem_message(StatusCode::BAD_REQUEST, &long).len() < 400);
    }

    fn resp(status: u16, etag: Option<&str>, body: &str) -> Response {
        Response { status: StatusCode::from_u16(status).unwrap(), etag: etag.map(str::to_string), body: body.into() }
    }

    #[test]
    fn parses_document_info_and_etags() {
        let info = parse_doc_info(&resp(
            202,
            None,
            r#"{"document":{"id":"01K70Q3V8X2M4N6P8R0T2V4X6Y","revision":1,"status":"accepted"},"job":{"id":"01JOB","status":"queued"}}"#,
        ));
        assert_eq!(info.id.as_deref(), Some("01K70Q3V8X2M4N6P8R0T2V4X6Y"));
        assert_eq!(info.revision, Some(1));
        assert_eq!(info.job_id.as_deref(), Some("01JOB"));
        // `job: null`, revision from ETag.
        let info = parse_doc_info(&resp(201, Some("\"3\""), r#"{"document":{"id":"01A"},"job":null}"#));
        assert_eq!((info.id.as_deref(), info.revision, info.job_id), (Some("01A"), Some(3), None));
        // Bare document, PATCH-style `{revision}`.
        assert_eq!(parse_doc_info(&resp(200, None, r#"{"id":"01B","revision":"5"}"#)).revision, Some(5));
        assert_eq!(parse_doc_info(&resp(200, None, r#"{"revision":6}"#)).revision, Some(6));
        assert_eq!(parse_doc_info(&resp(200, None, "not json")), DocInfo::default());
        // Unsafe ids are dropped.
        assert_eq!(parse_doc_info(&resp(201, None, r#"{"document":{"id":"../health"}}"#)).id, None);

        assert_eq!(parse_etag("\"4\""), Some(4));
        assert_eq!(parse_etag("W/\"4\""), Some(4));
        assert_eq!(parse_etag("12"), Some(12));
        assert_eq!(parse_etag("\"abc\""), None);
        assert!(valid_document_id("01K70Q3V8X2M4N6P8R0T2V4X6Y"));
        for bad in ["", "../health", "a/b", "id?x=1", "a b"] {
            assert!(!valid_document_id(bad), "{bad}");
        }
    }

    #[test]
    fn endpoint_joins_paths() {
        assert_eq!(endpoint("http://127.0.0.1:7171/v1", "/documents"), "http://127.0.0.1:7171/v1/documents");
        assert_eq!(endpoint("http://127.0.0.1:7171/v1/", "/health"), "http://127.0.0.1:7171/v1/health");
    }

    // --- fake transport flows ---------------------------------------------

    /// Answers requests from a queue and records them. No network.
    struct Fake {
        replies: RefCell<VecDeque<Result<Response>>>,
        log: RefCell<Vec<Request>>,
    }

    impl Fake {
        fn new(replies: Vec<Result<Response>>) -> Self {
            Fake { replies: RefCell::new(replies.into()), log: RefCell::new(Vec::new()) }
        }
        fn requests(&self) -> Vec<Request> {
            self.log.borrow().clone()
        }
    }

    impl Transport for Fake {
        async fn send(&self, req: &Request) -> Result<Response> {
            self.log.borrow_mut().push(req.clone());
            self.replies.borrow_mut().pop_front().unwrap_or_else(|| panic!("unexpected request {req:?}"))
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    }

    fn run(fake: &Fake, plan: &ExportPlan, record: &mut ExportRecord, wait: bool) -> Result<bool> {
        let ex = Exporter { t: fake, delays: &[], wait };
        rt().block_on(run_export(&ex, plan, MID, record, |_| Ok(())))
    }

    fn plan(summary: Option<&str>) -> ExportPlan {
        let mut m = meeting();
        m.summary = summary.map(str::to_string);
        build_plan(&m, &[seg(Speaker::You, 0, 1000, "hello")], &settings_with_identity("Justin"))
    }

    const REV_MISMATCH: &str = r#"{"type":"https://secondbrain.dev/problems/revision-mismatch","status":412,"current_revision":4}"#;

    #[test]
    fn approval_proposal_preserves_committed_revision_and_is_not_resubmitted() {
        let fake = Fake::new(vec![Ok(resp(202, None, r#"{"pending_id":"01PROPOSAL","status":"pending"}"#))]);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        run(&fake, &plan(None), &mut rec, false).unwrap();
        assert_eq!(rec.revision, Some(3));
        assert_eq!(rec.sync.pending[0].pending_id, "01PROPOSAL");
        assert_eq!(rec.sync.transcript_hash, None);

        // Still unapproved: checking again only reads, never creates a second proposal.
        let check = Fake::new(vec![Ok(resp(200, None,
            r#"{"document":{"id":"01TR","revision":3,"title":"Old","tags":[]}}"#))]);
        run(&check, &plan(None), &mut rec, false).unwrap();
        assert!(check.requests().iter().all(|r| r.method == Method::Get));
        assert_eq!(rec.sync.pending.len(), 1);
    }

    #[test]
    fn approval_is_confirmed_by_matching_values_not_just_a_new_revision() {
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        let p = plan(None);
        run(&Fake::new(vec![Ok(resp(202, None, r#"{"pending_id":"01P"}"#))]), &p, &mut rec, false).unwrap();
        let unrelated = Fake::new(vec![Ok(resp(200, None,
            r#"{"document":{"id":"01TR","revision":4,"title":"Unrelated edit","tags":[]}}"#))]);
        run(&unrelated, &p, &mut rec, false).unwrap();
        assert_eq!(rec.revision, Some(3));
        assert_eq!(rec.sync.pending.len(), 1);

        let approved = Fake::new(vec![Ok(resp(200, None,
            r#"{"document":{"id":"01TR","revision":5,"title":"Roadmap sync","tags":["transcriptor","meeting"]}}"#))]);
        run(&approved, &p, &mut rec, false).unwrap();
        assert_eq!(rec.revision, Some(5));
        assert!(rec.sync.pending.is_empty());
        assert!(rec.sync.transcript_hash.is_some());
        // Once confirmed, an unchanged export sends no PATCH at all.
        let unchanged = Fake::new(vec![Ok(resp(200, None,
            r#"{"document":{"id":"01TR","revision":5,"title":"Roadmap sync","tags":["meeting","transcriptor"]}}"#))]);
        run(&unchanged, &p, &mut rec, false).unwrap();
        assert!(unchanged.requests().iter().all(|r| r.method == Method::Get));
    }

    #[test]
    fn unchanged_manual_export_recreates_a_deleted_document() {
        let p = plan(None);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        rec.sync.transcript_hash = Some(fields_hash(&json!({"title": p.metadata.title, "tags": p.metadata.tags})));
        let fake = Fake::new(vec![Ok(resp(404, None, "{}")),
            Ok(resp(201, None, r#"{"document":{"id":"01NEW","revision":1}}"#))]);
        assert!(run(&fake, &p, &mut rec, false).unwrap());
        assert_eq!(rec.document_id.as_deref(), Some("01NEW"));
    }

    #[test]
    fn patch_transport_failures_are_not_retried_as_duplicate_proposals() {
        let fake = Fake::new(vec![Err(anyhow!("response lost"))]);
        let ex = Exporter { t: &fake, delays: &[Duration::ZERO], wait: false };
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        assert!(rt().block_on(run_export(&ex, &plan(None), MID, &mut rec, |_| Ok(()))).is_err());
        assert_eq!(fake.requests().len(), 1);
        assert_eq!(rec.revision, Some(3));
    }

    #[test]
    fn first_export_uploads_vtt_and_creates_note() {
        let p = plan(Some("We agreed."));
        let fake = Fake::new(vec![
            Ok(resp(202, None, r#"{"document":{"id":"01TR","revision":1},"job":{"id":"01JOB"}}"#)),
            Ok(resp(200, Some("\"1\""), r#"{"document":{"id":"01NOTE"},"job":null}"#)),
        ]);
        let mut rec = ExportRecord::default();
        assert!(run(&fake, &p, &mut rec, true).unwrap());
        assert_eq!(
            rec,
            ExportRecord {
                document_id: Some("01TR".into()),
                revision: Some(1),
                job_id: Some("01JOB".into()),
                summary_document_id: Some("01NOTE".into()),
                summary_revision: Some(1),
                sync: rec.sync.clone(),
            }
        );
        let reqs = fake.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!((reqs[0].method, reqs[0].path.as_str()), (Method::Post, "/documents?wait=30"));
        assert_eq!(reqs[0].idempotency_key.as_deref(), Some(p.transcript_key(MID).as_str()));
        assert_eq!(reqs[0].if_match, None);
        assert!(reqs[0].auth);
        match &reqs[0].body {
            Body::Vtt { file_name, vtt, metadata } => {
                assert_eq!(file_name, &format!("{MID}.vtt"));
                assert!(vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n<v Justin>hello\n"));
                assert_eq!(metadata, &p.metadata_json);
            }
            other => panic!("expected multipart, got {other:?}"),
        }
        assert_eq!(reqs[1].method, Method::Post);
        assert_eq!(reqs[1].idempotency_key, p.summary_key(MID));
        assert_eq!(reqs[1].body, Body::Json(serde_json::to_value(p.summary.as_ref().unwrap()).unwrap()));
    }

    #[test]
    fn no_summary_means_no_note_and_no_wait() {
        let fake = Fake::new(vec![Ok(resp(201, None, r#"{"document":{"id":"01TR","revision":1},"job":null}"#))]);
        let mut rec = ExportRecord::default();
        assert!(run(&fake, &plan(None), &mut rec, false).unwrap());
        assert_eq!(fake.requests()[0].path, "/documents");
        assert_eq!(rec.summary_document_id, None);
        assert_eq!(rec.job_id, None);
    }

    #[test]
    fn update_patches_title_and_tags_and_retries_412_with_current_revision() {
        let fake = Fake::new(vec![
            Ok(resp(412, None, REV_MISMATCH)),
            Ok(resp(200, None, r#"{"document":{"id":"01TR","revision":5}}"#)),
        ]);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        assert!(!run(&fake, &plan(None), &mut rec, true).unwrap());
        assert_eq!(rec.revision, Some(5));
        let reqs = fake.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!((reqs[0].method, reqs[0].path.as_str(), reqs[0].if_match), (Method::Patch, "/documents/01TR", Some(3)));
        assert_eq!(reqs[0].body, Body::Json(json!({"title": "Roadmap sync", "tags": ["meeting", "transcriptor"]})));
        assert_eq!(reqs[0].idempotency_key, None);
        assert_eq!(reqs[1].if_match, Some(4));
    }

    #[test]
    fn a_412_without_current_revision_rereads_the_document() {
        let fake = Fake::new(vec![
            Ok(resp(412, None, r#"{"type":"https://secondbrain.dev/problems/revision-mismatch"}"#)),
            Ok(resp(200, Some("\"7\""), r#"{"document":{"id":"01TR"}}"#)),
            Ok(resp(200, Some("\"8\""), "{}")),
        ]);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        run(&fake, &plan(None), &mut rec, false).unwrap();
        let reqs = fake.requests();
        assert_eq!((reqs[1].method, reqs[1].path.as_str()), (Method::Get, "/documents/01TR"));
        assert_eq!(reqs[2].if_match, Some(7));
        assert_eq!(rec.revision, Some(8));
    }

    #[test]
    fn unknown_revision_is_fetched_before_patching() {
        let fake = Fake::new(vec![
            Ok(resp(200, None, r#"{"document":{"id":"01TR","revision":2}}"#)),
            Ok(resp(200, None, r#"{"revision":3}"#)),
        ]);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: None, ..Default::default() };
        run(&fake, &plan(None), &mut rec, false).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].method, Method::Get);
        assert_eq!((reqs[1].method, reqs[1].if_match), (Method::Patch, Some(2)));
        assert_eq!(rec.revision, Some(3));
    }

    #[test]
    fn second_412_is_reported() {
        let fake = Fake::new(vec![Ok(resp(412, None, REV_MISMATCH)), Ok(resp(412, None, REV_MISMATCH))]);
        let mut rec = ExportRecord { document_id: Some("01TR".into()), revision: Some(3), ..Default::default() };
        let err = run(&fake, &plan(None), &mut rec, false).unwrap_err().to_string();
        assert!(err.starts_with("Transcript: ") && err.contains("revision-mismatch"), "{err}");
        assert_eq!(fake.requests().len(), 2);
        assert_eq!(rec.revision, Some(3), "unchanged on failure");
    }

    #[test]
    fn deleted_transcript_is_uploaded_again_with_a_fresh_key() {
        let p = plan(None);
        let fake = Fake::new(vec![
            Ok(resp(404, None, r#"{"type":"https://secondbrain.dev/problems/not-found"}"#)),
            Ok(resp(201, None, r#"{"document":{"id":"01NEW","revision":1},"job":{"id":"01J2"}}"#)),
        ]);
        let mut rec = ExportRecord { document_id: Some("01OLD".into()), revision: Some(2), ..Default::default() };
        assert!(run(&fake, &p, &mut rec, false).unwrap());
        assert_eq!(rec.document_id.as_deref(), Some("01NEW"));
        assert_eq!(rec.job_id.as_deref(), Some("01J2"));
        let key = fake.requests()[1].idempotency_key.clone().unwrap();
        let base = p.transcript_key(MID);
        assert!(key.starts_with(&format!("{base}-")) && key.len() > base.len() + 30, "{key}");
    }

    #[test]
    fn idempotency_key_reuse_retries_once_with_a_new_key() {
        let p = plan(None);
        let fake = Fake::new(vec![
            Ok(resp(422, None, r#"{"type":"https://secondbrain.dev/problems/idempotency-key-reuse"}"#)),
            Ok(resp(201, None, r#"{"document":{"id":"01TR","revision":1}}"#)),
        ]);
        let mut rec = ExportRecord::default();
        run(&fake, &p, &mut rec, false).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].idempotency_key, Some(p.transcript_key(MID)));
        assert_ne!(reqs[1].idempotency_key, reqs[0].idempotency_key);
        assert!(reqs[1].idempotency_key.as_deref().unwrap().starts_with(&p.transcript_key(MID)));

        // Other 422s are final.
        let fake = Fake::new(vec![Ok(resp(
            422,
            None,
            r#"{"type":"https://secondbrain.dev/problems/validation-error","detail":"bad props"}"#,
        ))]);
        let err = run(&fake, &p, &mut ExportRecord::default(), false).unwrap_err().to_string();
        assert!(err.contains("validation-error") && err.contains("bad props"), "{err}");
    }

    #[test]
    fn summary_update_uses_if_match_and_keeps_transcript_progress_on_failure() {
        let p = plan(Some("New notes"));
        // Transcript created, then the note PATCH fails with a 400.
        let fake = Fake::new(vec![
            Ok(resp(201, None, r#"{"document":{"id":"01TR","revision":1}}"#)),
            Ok(resp(400, None, r#"{"title":"Bad request","detail":"content too long"}"#)),
        ]);
        let mut rec = ExportRecord { summary_document_id: Some("01NOTE".into()), summary_revision: Some(2), ..Default::default() };
        let err = run(&fake, &p, &mut rec, false).unwrap_err().to_string();
        assert!(err.starts_with("Notes: ") && err.contains("content too long"), "{err}");
        assert_eq!(rec.document_id.as_deref(), Some("01TR"), "transcript progress kept");
        let reqs = fake.requests();
        assert_eq!((reqs[1].method, reqs[1].path.as_str(), reqs[1].if_match), (Method::Patch, "/documents/01NOTE", Some(2)));
        assert_eq!(
            reqs[1].body,
            Body::Json(json!({"title": "Roadmap sync — notes", "content": "New notes", "tags": ["meeting", "transcriptor", "summary"]}))
        );
    }

    #[test]
    fn transient_failures_are_retried() {
        let fake = Fake::new(vec![
            Err(anyhow!("connection refused")),
            Ok(resp(503, None, "")),
            Ok(resp(201, None, r#"{"document":{"id":"01TR","revision":1}}"#)),
        ]);
        let ex = Exporter { t: &fake, delays: &[Duration::ZERO, Duration::ZERO], wait: false };
        let mut rec = ExportRecord::default();
        rt().block_on(run_export(&ex, &plan(None), MID, &mut rec, |_| Ok(()))).unwrap();
        assert_eq!(fake.requests().len(), 3);
        // Same idempotency key on every attempt.
        let keys: Vec<_> = fake.requests().into_iter().map(|r| r.idempotency_key).collect();
        assert!(keys.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn next_revision_prefers_problem_body() {
        let f = Failure::Status {
            status: StatusCode::PRECONDITION_FAILED,
            problem: parse_problem(REV_MISMATCH),
            message: String::new(),
        };
        assert_eq!(next_revision(&f), Some(4));
        let f = Failure::Status { status: StatusCode::PRECONDITION_FAILED, problem: Problem::default(), message: String::new() };
        assert_eq!(next_revision(&f), None);
        assert_eq!(next_revision(&Failure::Transport(anyhow!("x"))), None);
    }

    #[test]
    fn connection_check_reports_reachability_and_key() {
        let base = "http://brain.lan:7171/v1";
        let check = |replies, has_key| {
            let fake = Fake::new(replies);
            let c = rt().block_on(check_connection(&fake, base, has_key));
            (c, fake.requests())
        };
        let (c, reqs) = check(vec![Ok(resp(200, None, r#"{"status":"ok"}"#)), Ok(resp(200, None, "{}"))], true);
        assert_eq!((c.reachable, c.key_ok), (Some(true), Some(true)));
        assert!(c.message.contains("Connected"));
        assert_eq!((reqs[0].path.as_str(), reqs[0].auth), ("/health", false));
        assert_eq!((reqs[1].path.as_str(), reqs[1].auth), ("/stats", true));

        let (c, _) = check(vec![Ok(resp(200, None, "{}")), Ok(resp(401, None, ""))], true);
        assert_eq!((c.reachable, c.key_ok), (Some(true), Some(false)));
        assert!(c.message.contains("server is reachable but the API key was rejected"));
        let (c, _) = check(vec![Ok(resp(200, None, "{}")), Ok(resp(403, None, ""))], true);
        assert_eq!(c.key_ok, Some(false));
        assert!(c.message.contains("read scope"));
        let (c, _) = check(vec![Ok(resp(200, None, "{}")), Ok(resp(500, None, "boom"))], true);
        assert_eq!((c.reachable, c.key_ok), (Some(true), None));

        let (c, reqs) = check(vec![Ok(resp(200, None, "{}"))], false);
        assert_eq!((c.reachable, c.key_ok), (Some(true), None));
        assert!(c.message.contains("no API key"));
        assert_eq!(reqs.len(), 1);

        let (c, _) = check(vec![Err(anyhow!("could not reach SecondBrain: refused"))], true);
        assert_eq!((c.reachable, c.key_ok), (Some(false), None));
        let (c, _) = check(vec![Ok(resp(404, None, ""))], true);
        assert_eq!(c.reachable, Some(false));
        assert!(c.message.contains("/v1"));
    }

    // --- orchestration ------------------------------------------------------

    #[test]
    fn auto_export_rules() {
        let mut s = Settings::default();
        let mut m = meeting();
        assert!(!should_export(&s, true, &m, Trigger::Manual), "disabled");
        s.secondbrain_enabled = true;
        assert!(!should_export(&s, false, &m, Trigger::Manual), "no key");
        assert!(should_export(&s, true, &m, Trigger::Manual));
        assert!(should_export(&s, true, &m, Trigger::Transcribed));
        assert!(should_export(&s, true, &m, Trigger::Summarized));
        s.secondbrain_auto_export = false;
        assert!(should_export(&s, true, &m, Trigger::Manual));
        assert!(!should_export(&s, true, &m, Trigger::Transcribed));
        assert!(!should_export(&s, true, &m, Trigger::Summarized));
        // Already exported: a new summary is pushed even with auto-export off.
        m.export_document_id = Some("01K70Q".into());
        assert!(should_export(&s, true, &m, Trigger::Summarized));
        assert!(!should_export(&s, true, &m, Trigger::Transcribed));
    }

    #[test]
    fn claims_are_exclusive_and_queue_reruns() {
        let id = format!("claim-test-{}", uuid::Uuid::new_v4());
        let mut c = claim(&id, false).unwrap();
        assert!(claim(&id, false).is_none());
        assert!(!c.finish_or_rerun(), "nothing queued");
        assert!(!IN_FLIGHT.lock().contains_key(&id));

        let mut c2 = claim(&id, true).unwrap();
        assert!(claim(&id, true).is_none(), "busy; queues a rerun");
        assert!(c2.finish_or_rerun(), "rerun queued");
        assert!(!c2.finish_or_rerun(), "rerun consumed");
        assert!(claim(&id, false).is_some(), "released (and the temp claim drops)");
        drop(c);
        drop(c2);

        // Dropping without finishing (e.g. a panic) releases the claim.
        let c3 = claim(&id, false).unwrap();
        drop(c3);
        assert!(claim(&id, false).is_some());
    }

    #[test]
    fn retries_transient_failures_only() {
        let delays = [Duration::ZERO, Duration::ZERO];
        let status = |code: StatusCode, msg: &str| Failure::Status { status: code, problem: Problem::default(), message: msg.into() };

        // Connection errors and 5xx retried; third attempt succeeds.
        let calls = AtomicUsize::new(0);
        let out = rt().block_on(with_retries(&delays, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                match n {
                    0 => Err(Failure::Transport(anyhow!("refused"))),
                    1 => Err(status(StatusCode::BAD_GATEWAY, "bad gateway")),
                    _ => Ok("ok"),
                }
            }
        }));
        assert_eq!(out.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Gives up after three attempts.
        let calls = AtomicUsize::new(0);
        let out: Result<(), Failure> = rt().block_on(with_retries(&delays, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(status(StatusCode::INTERNAL_SERVER_ERROR, "boom")) }
        }));
        assert_eq!(out.unwrap_err().message(), "boom");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // 4xx is final.
        let calls = AtomicUsize::new(0);
        let out: Result<(), Failure> = rt().block_on(with_retries(&delays, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(status(StatusCode::NOT_FOUND, "gone")) }
        }));
        assert_eq!(out.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
