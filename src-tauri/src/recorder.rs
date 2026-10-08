//! Orchestrates one recording session: capture -> resample -> WAV file +
//! chunker -> transcriber -> database + UI events.

use crate::audio::resample::{Resampler, TARGET_RATE};
use crate::audio::{self, CaptureConfig, CaptureHandle, RawChunk, Source};
use crate::db::Db;
use crate::models::{
    events, Levels, Meeting, MeetingStatus, RecordingStatus, Settings, Speaker, TranscriptionMode,
    Warning,
};
use crate::settings;
use crate::transcribe::chunker::{Chunk, Chunker, ChunkerConfig};
use crate::transcribe::{cloud::CloudTranscriber, local::LocalTranscriber, models as whisper_models, Transcriber};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

struct SourceChunk {
    speaker: Speaker,
    chunk: Chunk,
}

/// Where a meeting's audio lives. Always derived from the meeting id, never
/// from a stored path.
pub fn recording_path(data_dir: &Path, id: &uuid::Uuid) -> PathBuf {
    data_dir.join("recordings").join(format!("{}.wav", id.hyphenated()))
}

/// Seconds of live-transcription backlog we tolerate before dropping chunks.
const BACKLOG_SECONDS: usize = 180;
/// How often to repeat the "falling behind" warning while drops continue.
const BACKLOG_WARN_INTERVAL: Duration = Duration::from_secs(60);
const BACKLOG_WARNING: &str =
    "Transcription is falling behind; some audio was not transcribed (saved audio is unaffected)";

/// Queue capacity (in chunks) for ~3 minutes of audio across both sources.
fn chunk_queue_capacity(chunk_seconds: u32) -> usize {
    (BACKLOG_SECONDS / chunk_seconds.max(1) as usize * 2).max(8)
}

/// Accounts for chunks dropped because the transcriber can't keep up, and
/// rate-limits the user-facing warning.
#[derive(Default)]
struct DropTracker {
    dropped_ms: i64,
    dropped_chunks: u64,
    last_warned: Option<Instant>,
}

impl DropTracker {
    /// Record a dropped chunk; returns true when a warning should be shown now.
    fn record_drop(&mut self, ms: i64, now: Instant) -> bool {
        self.dropped_ms += ms.max(0);
        self.dropped_chunks += 1;
        let due = self
            .last_warned
            .is_none_or(|t| now.duration_since(t) >= BACKLOG_WARN_INTERVAL);
        if due {
            self.last_warned = Some(now);
        }
        due
    }
}

/// Hand an item to the transcription queue without ever blocking. When the
/// queue is full the item is dropped and accounted; returns true if the
/// caller should emit the backlog warning.
fn offer<T>(tx: &Sender<T>, item: T, ms: i64, tracker: &mut DropTracker, now: Instant) -> bool {
    match tx.try_send(item) {
        Ok(()) => false,
        Err(TrySendError::Full(_)) => tracker.record_drop(ms, now),
        // Transcription worker is gone; nothing to do.
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// A source more than this far behind wall-clock time gets silence inserted.
const ALIGN_TOLERANCE: Duration = Duration::from_millis(250);
/// If every source is this far behind at once, the pipeline itself was paused
/// (e.g. the machine slept); re-base the timeline instead of writing it all
/// out as silence.
const MAX_ALIGN_GAP: Duration = Duration::from_secs(10);

fn samples_in(d: Duration, rate: u32) -> u64 {
    (d.as_nanos() * rate as u128 / 1_000_000_000) as u64
}

/// Samples of silence to append to a source that has produced `have` samples
/// (real + padding) so it catches up with `elapsed` wall-clock time at `rate`.
/// Returns 0 while it is within `tolerance` of where it should be (or ahead).
fn alignment_padding(have: u64, elapsed: Duration, rate: u32, tolerance: Duration) -> u64 {
    let expected = samples_in(elapsed, rate);
    if expected > have.saturating_add(samples_in(tolerance, rate)) {
        expected - have
    } else {
        0
    }
}

/// Per-source timeline position.
#[derive(Default, Clone, Copy)]
struct SourceClock {
    /// Wall-clock time of the first sample received from this source.
    first: Option<Instant>,
    /// 16 kHz samples received from the device.
    received: u64,
    /// 16 kHz samples of silence inserted to keep up with wall-clock time.
    padded: u64,
}

impl SourceClock {
    fn position(&self) -> u64 {
        self.received + self.padded
    }
}

/// Keeps mic and system audio on a shared wall-clock timeline. Sources do not
/// deliver at a steady rate everywhere (WASAPI loopback sends nothing while
/// nothing is playing), so pairing samples by count drifts; instead each
/// source is padded with silence whenever it falls behind the time elapsed
/// since the first sample of any source.
#[derive(Default)]
struct Aligner {
    mic: SourceClock,
    sys: SourceClock,
    /// Pipeline pauses skipped over (see `MAX_ALIGN_GAP`).
    skipped: Duration,
}

impl Aligner {
    fn clock(&mut self, source: Source) -> &mut SourceClock {
        match source {
            Source::Mic => &mut self.mic,
            Source::System => &mut self.sys,
        }
    }

    fn received(&mut self, source: Source, first_sample_at: Instant, samples: u64) {
        let c = self.clock(source);
        c.first.get_or_insert(first_sample_at);
        c.received += samples;
    }

    fn padded(&mut self, source: Source, samples: u64) {
        self.clock(source).padded += samples;
    }

    /// Silence (in samples) each source needs right now: `(mic, sys)`. Only
    /// meaningful once all delivered audio has been counted.
    fn padding(&mut self, now: Instant, system_active: bool) -> (u64, u64) {
        let Some(origin) = [self.mic.first, self.sys.first].into_iter().flatten().min() else {
            return (0, 0);
        };
        let compute = |a: &Aligner| {
            let elapsed = now.saturating_duration_since(origin).saturating_sub(a.skipped);
            let pad = |c: &SourceClock| alignment_padding(c.position(), elapsed, TARGET_RATE, ALIGN_TOLERANCE);
            (pad(&a.mic), if system_active { pad(&a.sys) } else { 0 })
        };
        let (mic, sys) = compute(self);
        let least = if system_active { mic.min(sys) } else { mic };
        if least > samples_in(MAX_ALIGN_GAP, TARGET_RATE) {
            let gap = Duration::from_nanos(least * 1_000_000_000 / TARGET_RATE as u64);
            log::warn!("audio pipeline paused for {:.1} s; skipping the gap", gap.as_secs_f64());
            self.skipped += gap;
            return compute(self);
        }
        (mic, sys)
    }
}

/// Set when the app is exiting: transcription workers stop between chunks so
/// the whisper context can be released before the process tears down.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
/// Number of transcription workers still alive.
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Stop any active recording and wait (bounded) for transcription workers to
/// finish, then release the cached whisper model. Called on app exit.
pub fn shutdown(recorder: &Recorder, app: &AppHandle, db: &Arc<Db>) {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    if recorder.status().active {
        if let Err(e) = recorder.stop(app.clone(), db.clone()) {
            log::warn!("stopping recording on exit: {e:#}");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while ACTIVE_WORKERS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    crate::transcribe::local::clear_cache();
}

struct ActiveSession {
    meeting_id: String,
    started: Instant,
    system_active: bool,
    capture: Option<CaptureHandle>,
    processing: Option<JoinHandle<Result<i64>>>,
    stop_flag: Arc<AtomicBool>,
}

pub struct Recorder {
    session: Mutex<Option<ActiveSession>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            session: Mutex::new(None),
        }
    }
}

fn emit_warning(app: &AppHandle, message: impl Into<String>) {
    let message = message.into();
    log::warn!("{message}");
    let _ = app.emit(events::RECORDING_WARNING, Warning { message });
}

fn make_transcriber(settings: &Settings, data_dir: &Path) -> Result<Box<dyn Transcriber>> {
    match settings.transcription_mode {
        TranscriptionMode::Local => {
            let path = whisper_models::model_path(data_dir, &settings.local_model);
            if !path.is_file() {
                return Err(anyhow!(
                    "Whisper model '{}' is not downloaded. Download it in Settings or switch to cloud transcription.",
                    settings.local_model
                ));
            }
            Ok(Box::new(LocalTranscriber::new(&path)?))
        }
        TranscriptionMode::Cloud => {
            let key = settings::require_secret(settings::SECRET_OPENAI, "Cloud transcription API key")?;
            Ok(Box::new(CloudTranscriber::new(
                &settings.cloud_base_url,
                &settings.cloud_model,
                &key,
            )?))
        }
    }
}

impl Recorder {
    pub fn status(&self) -> RecordingStatus {
        match self.session.lock().as_ref() {
            Some(s) => RecordingStatus {
                active: true,
                meeting_id: Some(s.meeting_id.clone()),
                elapsed_ms: s.started.elapsed().as_millis() as i64,
                system_audio: s.system_active,
            },
            None => RecordingStatus::idle(),
        }
    }

    pub fn start(
        &self,
        app: AppHandle,
        db: Arc<Db>,
        settings: Settings,
        data_dir: PathBuf,
        title: Option<String>,
    ) -> Result<Meeting> {
        let mut guard = self.session.lock();
        if guard.is_some() {
            return Err(anyhow!("a recording is already in progress"));
        }

        // Fail fast on configuration problems before touching audio devices.
        let transcriber_probe = match settings.transcription_mode {
            TranscriptionMode::Local => {
                let path = whisper_models::model_path(&data_dir, &settings.local_model);
                if !path.is_file() {
                    return Err(anyhow!(
                        "Whisper model '{}' is not downloaded. Download it in Settings or switch to cloud transcription.",
                        settings.local_model
                    ));
                }
                Ok(())
            }
            TranscriptionMode::Cloud => {
                settings::require_secret(settings::SECRET_OPENAI, "Cloud transcription API key").map(|_| ())
            }
        };
        transcriber_probe?;

        let now = Utc::now();
        let uuid = uuid::Uuid::new_v4();
        let id = uuid.to_string();
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Meeting {}", now.format("%Y-%m-%d %H:%M")));

        let audio_path = recording_path(&data_dir, &uuid);
        let recordings_dir = audio_path.parent().context("recordings dir")?;
        std::fs::create_dir_all(recordings_dir).context("creating recordings dir")?;
        crate::fs_perms::harden_dir(recordings_dir);

        let (capture, raw_rx, started) = audio::start(CaptureConfig {
            input_device: settings.input_device.clone(),
            capture_system: settings.capture_system_audio,
            system_device: settings.system_audio_device.clone(),
        })?;
        for w in &started.warnings {
            emit_warning(&app, w.clone());
        }

        let meeting = Meeting {
            id: id.clone(),
            title,
            started_at: now.to_rfc3339(),
            ended_at: None,
            duration_ms: 0,
            status: MeetingStatus::Recording,
            audio_path: Some(audio_path.to_string_lossy().to_string()),
            transcription_mode: settings.transcription_mode,
            summary: None,
            summarized_at: None,
        };
        db.create_meeting(&meeting)?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        // Bounded so a slow transcriber can't grow memory without limit; the
        // processing thread drops chunks (never blocks) when it's full.
        let (chunk_tx, chunk_rx) = bounded::<SourceChunk>(chunk_queue_capacity(settings.chunk_seconds));

        // Transcription worker: owns the engine, writes segments, emits events.
        spawn_transcribe_thread(
            app.clone(),
            db.clone(),
            settings.clone(),
            data_dir.clone(),
            id.clone(),
            chunk_rx,
        );

        // Processing worker: resample, write WAV, chunk, levels, status ticks.
        let processing = spawn_processing_thread(
            app.clone(),
            id.clone(),
            raw_rx,
            chunk_tx,
            audio_path.clone(),
            started.system_active,
            settings.chunk_seconds,
            stop_flag.clone(),
        )?;

        *guard = Some(ActiveSession {
            meeting_id: id.clone(),
            started: Instant::now(),
            system_active: started.system_active,
            capture: Some(capture),
            processing: Some(processing),
            stop_flag,
        });

        let _ = app.emit(
            events::RECORDING_STATUS,
            RecordingStatus {
                active: true,
                meeting_id: Some(id),
                elapsed_ms: 0,
                system_audio: started.system_active,
            },
        );
        Ok(meeting)
    }

    pub fn stop(&self, app: AppHandle, db: Arc<Db>) -> Result<Meeting> {
        let mut session = self
            .session
            .lock()
            .take()
            .ok_or_else(|| anyhow!("no recording in progress"))?;

        // Stop capture first so the raw channel closes, then let processing drain.
        if let Some(c) = session.capture.take() {
            c.stop();
        }
        session.stop_flag.store(true, Ordering::SeqCst);
        let duration_ms = match session.processing.take() {
            Some(h) => match h.join() {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    emit_warning(&app, format!("Recording finished with an error: {e}"));
                    session.started.elapsed().as_millis() as i64
                }
                Err(_) => session.started.elapsed().as_millis() as i64,
            },
            None => session.started.elapsed().as_millis() as i64,
        };

        let meeting = db.require_meeting(&session.meeting_id)?;
        db.finish_meeting(
            &meeting.id,
            &Utc::now().to_rfc3339(),
            duration_ms,
            meeting.audio_path.as_deref(),
            MeetingStatus::Processing,
        )?;
        let meeting = db.require_meeting(&meeting.id)?;
        let _ = app.emit(events::RECORDING_STATUS, RecordingStatus::idle());
        let _ = app.emit(events::MEETING_UPDATED, meeting.clone());
        Ok(meeting)
    }
}

/// Most raw blocks handled per loop iteration before levels/status ticks run.
const MAX_BATCH: usize = 256;

fn speaker_for(source: Source) -> Speaker {
    match source {
        Source::Mic => Speaker::You,
        Source::System => Speaker::Others,
    }
}

/// Append `n` samples of silence to a source's WAV buffer and its chunker
/// (keeping the chunker's timeline in step). Returns any chunks completed.
fn pad_with_silence(pending: &mut Vec<f32>, chunker: &mut Chunker, n: usize) -> Vec<Chunk> {
    let zeros = vec![0.0f32; n];
    pending.extend_from_slice(&zeros);
    chunker.push(&zeros)
}

#[allow(clippy::too_many_arguments)]
fn spawn_processing_thread(
    app: AppHandle,
    meeting_id: String,
    raw_rx: Receiver<RawChunk>,
    chunk_tx: Sender<SourceChunk>,
    audio_path: PathBuf,
    system_active: bool,
    chunk_seconds: u32,
    stop_flag: Arc<AtomicBool>,
) -> Result<JoinHandle<Result<i64>>> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&audio_path, spec)
        .with_context(|| format!("creating {}", audio_path.display()))?;
    crate::fs_perms::harden_file(&audio_path);

    let handle = std::thread::Builder::new()
        .name("audio-processing".into())
        .spawn(move || -> Result<i64> {
            let started = Instant::now();
            let mut resamplers: HashMap<Source, Resampler> = HashMap::new();
            let new_chunker = move || Chunker::new(ChunkerConfig::for_target_seconds(chunk_seconds));
            let mut chunkers: HashMap<Source, Chunker> = HashMap::new();
            // Pending 16 kHz samples not yet written to the WAV, per source.
            let mut pending_mic: Vec<f32> = Vec::new();
            let mut pending_sys: Vec<f32> = Vec::new();
            let mut aligner = Aligner::default();
            let mut frames_written: u64 = 0;
            let mut level_acc: HashMap<Source, (f32, usize)> = HashMap::new();
            let mut last_levels = Instant::now();
            let mut last_status = Instant::now();
            let mut drops = DropTracker::default();
            let send_chunk = |speaker: Speaker, chunk: Chunk, drops: &mut DropTracker| {
                let ms = chunk.end_ms - chunk.start_ms;
                if offer(&chunk_tx, SourceChunk { speaker, chunk }, ms, drops, Instant::now()) {
                    log::warn!(
                        "transcription backlog full: dropped {} chunks ({} ms of audio) so far",
                        drops.dropped_chunks,
                        drops.dropped_ms
                    );
                    emit_warning(&app, BACKLOG_WARNING);
                }
            };

            let mut write_frames = |mic: &mut Vec<f32>, sys: &mut Vec<f32>, n: usize| -> Result<()> {
                for i in 0..n {
                    let l = mic.get(i).copied().unwrap_or(0.0);
                    let r = sys.get(i).copied().unwrap_or(0.0);
                    writer.write_sample((l.clamp(-1.0, 1.0) * 32767.0) as i16)?;
                    writer.write_sample((r.clamp(-1.0, 1.0) * 32767.0) as i16)?;
                }
                mic.drain(..n.min(mic.len()));
                sys.drain(..n.min(sys.len()));
                frames_written += n as u64;
                Ok(())
            };

            loop {
                let mut batch = Vec::new();
                match raw_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(raw) => {
                        batch.push(raw);
                        // Take whatever else is queued so alignment sees all delivered audio.
                        batch.extend(raw_rx.try_iter().take(MAX_BATCH));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                for raw in batch {
                    let rs = resamplers
                        .entry(raw.source)
                        .or_insert_with(|| Resampler::new(raw.sample_rate));
                    if rs.src_rate() != raw.sample_rate {
                        *rs = Resampler::new(raw.sample_rate);
                    }
                    let mono = rs.process(&raw.data, raw.channels);

                    // Levels
                    let energy: f32 = mono.iter().map(|s| s * s).sum();
                    let e = level_acc.entry(raw.source).or_insert((0.0, 0));
                    e.0 += energy;
                    e.1 += mono.len();

                    // Timeline: `captured_at` is when the block ended; back up to its first sample.
                    let frames = raw.data.len() / raw.channels.max(1) as usize;
                    let block = Duration::from_secs_f64(frames as f64 / raw.sample_rate.max(1) as f64);
                    let first_at = raw.captured_at.checked_sub(block).unwrap_or(raw.captured_at);
                    aligner.received(raw.source, first_at, mono.len() as u64);

                    // Chunker
                    let ch = chunkers.entry(raw.source).or_insert_with(new_chunker);
                    for chunk in ch.push(&mono) {
                        send_chunk(speaker_for(raw.source), chunk, &mut drops);
                    }

                    match raw.source {
                        Source::Mic => pending_mic.extend_from_slice(&mono),
                        Source::System => pending_sys.extend_from_slice(&mono),
                    }
                }

                // Time-based alignment: a source that fell behind wall-clock time
                // (e.g. WASAPI loopback during silence) gets silence inserted, in
                // the WAV and in its chunker so transcript timestamps agree.
                // Only once the queue is drained, or queued audio would count as lag.
                if raw_rx.is_empty() {
                    let (mic_pad, sys_pad) = aligner.padding(Instant::now(), system_active);
                    for (source, n) in [(Source::Mic, mic_pad), (Source::System, sys_pad)] {
                        if n == 0 {
                            continue;
                        }
                        let pending = match source {
                            Source::Mic => &mut pending_mic,
                            Source::System => &mut pending_sys,
                        };
                        let ch = chunkers.entry(source).or_insert_with(new_chunker);
                        for chunk in pad_with_silence(pending, ch, n as usize) {
                            send_chunk(speaker_for(source), chunk, &mut drops);
                        }
                        aligner.padded(source, n);
                    }
                }

                // Write WAV frames while both channels have data (or only mic exists).
                let n = if system_active {
                    pending_mic.len().min(pending_sys.len())
                } else {
                    pending_mic.len()
                };
                if n > 0 {
                    write_frames(&mut pending_mic, &mut pending_sys, n)?;
                }
                // Avoid unbounded growth if one source runs ahead: cap the lead at
                // 2 s, filling the other source with (accounted) silence.
                let cap = TARGET_RATE as usize * 2;
                if pending_mic.len() > cap || pending_sys.len() > cap {
                    let n = pending_mic.len().max(pending_sys.len());
                    for source in [Source::Mic, Source::System] {
                        if source == Source::System && !system_active {
                            continue;
                        }
                        let pending = match source {
                            Source::Mic => &mut pending_mic,
                            Source::System => &mut pending_sys,
                        };
                        let short = n - pending.len();
                        if short > 0 {
                            let ch = chunkers.entry(source).or_insert_with(new_chunker);
                            for chunk in pad_with_silence(pending, ch, short) {
                                send_chunk(speaker_for(source), chunk, &mut drops);
                            }
                            aligner.padded(source, short as u64);
                        }
                    }
                    write_frames(&mut pending_mic, &mut pending_sys, n)?;
                }

                if last_levels.elapsed() >= Duration::from_millis(100) {
                    last_levels = Instant::now();
                    let rms = |src: Source| -> f32 {
                        level_acc
                            .get(&src)
                            .filter(|(_, n)| *n > 0)
                            .map(|(e, n)| (e / *n as f32).sqrt().min(1.0))
                            .unwrap_or(0.0)
                    };
                    let _ = app.emit(
                        events::AUDIO_LEVELS,
                        Levels {
                            mic: rms(Source::Mic),
                            system: rms(Source::System),
                        },
                    );
                    level_acc.clear();
                }
                if last_status.elapsed() >= Duration::from_secs(1) {
                    last_status = Instant::now();
                    let _ = app.emit(
                        events::RECORDING_STATUS,
                        RecordingStatus {
                            active: true,
                            meeting_id: Some(meeting_id.clone()),
                            elapsed_ms: started.elapsed().as_millis() as i64,
                            system_audio: system_active,
                        },
                    );
                }

                if stop_flag.load(Ordering::SeqCst) && raw_rx.is_empty() {
                    break;
                }
            }

            // Drain and finalize.
            let n = pending_mic.len().max(pending_sys.len());
            if n > 0 {
                write_frames(&mut pending_mic, &mut pending_sys, n)?;
            }
            for (source, ch) in chunkers.iter_mut() {
                if let Some(chunk) = ch.flush() {
                    send_chunk(speaker_for(*source), chunk, &mut drops);
                }
            }
            drop(chunk_tx);
            if drops.dropped_chunks > 0 {
                log::warn!(
                    "live transcription skipped {} ms of audio in {} chunks",
                    drops.dropped_ms,
                    drops.dropped_chunks
                );
            }
            writer.finalize().context("finalizing WAV")?;
            Ok((frames_written * 1000 / TARGET_RATE as u64) as i64)
        })
        .context("spawning processing thread")?;
    Ok(handle)
}

fn spawn_transcribe_thread(
    app: AppHandle,
    db: Arc<Db>,
    settings: Settings,
    data_dir: PathBuf,
    meeting_id: String,
    chunk_rx: Receiver<SourceChunk>,
) {
    let _ = std::thread::Builder::new()
        .name("transcribe".into())
        .spawn(move || {
            ACTIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
            struct Guard;
            impl Drop for Guard {
                fn drop(&mut self) {
                    ACTIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _guard = Guard;

            let mut transcriber = match make_transcriber(&settings, &data_dir) {
                Ok(t) => Some(t),
                Err(e) => {
                    emit_warning(&app, format!("Transcription disabled: {e}"));
                    None
                }
            };
            let mut failures = 0u32;

            for SourceChunk { speaker, chunk } in chunk_rx.iter() {
                if SHUTTING_DOWN.load(Ordering::SeqCst) {
                    break;
                }
                let Some(t) = transcriber.as_mut() else { continue };
                let t0 = Instant::now();
                match t.transcribe(&chunk.samples, &settings.language) {
                    Ok(utterances) => {
                        failures = 0;
                        log::debug!(
                            "{} transcribed {} ms of {:?} in {} ms -> {} utterances",
                            t.name(),
                            chunk.end_ms - chunk.start_ms,
                            speaker,
                            t0.elapsed().as_millis(),
                            utterances.len()
                        );
                        let chunk_len = chunk.end_ms - chunk.start_ms;
                        for u in utterances {
                            let start = chunk.start_ms + u.start_ms.clamp(0, chunk_len);
                            let end = chunk.start_ms + u.end_ms.clamp(0, chunk_len).max(u.start_ms.clamp(0, chunk_len));
                            match db.insert_segment(&meeting_id, speaker, start, end, &u.text) {
                                Ok(seg) => {
                                    let _ = app.emit(events::TRANSCRIPT_SEGMENT, seg);
                                }
                                Err(e) => log::error!("saving segment: {e}"),
                            }
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        log::error!("transcription failed: {e:#}");
                        if failures == 1 || failures % 10 == 0 {
                            emit_warning(&app, format!("Transcription error: {e}"));
                        }
                        if failures >= 25 {
                            emit_warning(&app, "Too many transcription errors; transcription stopped for this recording.");
                            transcriber = None;
                        }
                    }
                }
            }

            // Channel closed: recording stopped and all chunks handled.
            drop(transcriber);
            if let Err(e) = db.set_status(&meeting_id, MeetingStatus::Done) {
                log::error!("marking meeting done: {e}");
            }
            if let Ok(m) = db.require_meeting(&meeting_id) {
                let _ = app.emit(events::MEETING_UPDATED, m);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_capacity_covers_three_minutes() {
        assert_eq!(chunk_queue_capacity(8), 44); // 180 / 8 = 22 per source
        assert_eq!(chunk_queue_capacity(3), 120);
        assert_eq!(chunk_queue_capacity(30), 12);
        assert_eq!(chunk_queue_capacity(0), 360);
        assert!(chunk_queue_capacity(1000) >= 8);
    }

    #[test]
    fn full_queue_drops_without_blocking_and_rate_limits_warning() {
        // A "transcriber" that never keeps up: nobody drains the receiver.
        let (tx, rx) = bounded::<u32>(2);
        let mut t = DropTracker::default();
        let t0 = Instant::now();
        assert!(!offer(&tx, 1, 8000, &mut t, t0));
        assert!(!offer(&tx, 2, 8000, &mut t, t0));
        // Queue full: first drop warns, later drops within 60 s don't.
        assert!(offer(&tx, 3, 8000, &mut t, t0));
        assert!(!offer(&tx, 4, 5000, &mut t, t0 + Duration::from_secs(30)));
        assert_eq!(t.dropped_chunks, 2);
        assert_eq!(t.dropped_ms, 13_000);
        // Still falling behind a minute later: warn again.
        assert!(offer(&tx, 5, 8000, &mut t, t0 + Duration::from_secs(61)));
        assert_eq!(t.dropped_ms, 21_000);
        // The queued items are the first two; drops never displaced them.
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![1, 2]);
        // Room again: accepted, no warning, no new drops.
        assert!(!offer(&tx, 6, 8000, &mut t, t0 + Duration::from_secs(62)));
        assert_eq!(t.dropped_chunks, 3);
    }

    #[test]
    fn disconnected_queue_is_not_a_drop() {
        let (tx, rx) = bounded::<u32>(1);
        drop(rx);
        let mut t = DropTracker::default();
        assert!(!offer(&tx, 1, 8000, &mut t, Instant::now()));
        assert_eq!(t.dropped_chunks, 0);
    }

    #[test]
    fn alignment_padding_only_past_tolerance() {
        let tol = ALIGN_TOLERANCE;
        let rate = TARGET_RATE;
        // On time, slightly behind (within 250 ms), or ahead: nothing to do.
        assert_eq!(alignment_padding(16_000, Duration::from_secs(1), rate, tol), 0);
        assert_eq!(alignment_padding(12_001, Duration::from_secs(1), rate, tol), 0);
        assert_eq!(alignment_padding(40_000, Duration::from_secs(1), rate, tol), 0);
        // More than 250 ms behind: pad all the way up to wall-clock.
        assert_eq!(alignment_padding(11_999, Duration::from_secs(1), rate, tol), 4_001);
        assert_eq!(alignment_padding(0, Duration::from_millis(300), rate, tol), 4_800);
        assert_eq!(alignment_padding(0, Duration::from_secs(60), rate, tol), 960_000);
        assert_eq!(alignment_padding(0, Duration::ZERO, rate, tol), 0);
    }

    #[test]
    fn aligner_pads_silent_loopback_and_keeps_sources_in_step() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut a = Aligner::default();
        // Nothing received yet: no timeline, no padding.
        assert_eq!(a.padding(at(5_000), true), (0, 0));

        // Mic delivers steadily; WASAPI loopback stays silent (no packets).
        a.received(Source::Mic, t0, 1_600);
        assert_eq!(a.padding(at(100), true), (0, 0));
        a.received(Source::Mic, at(100), 1_600);
        assert_eq!(a.padding(at(200), true), (0, 0));
        a.received(Source::Mic, at(200), 3_200);
        // 400 ms in: system audio is 400 ms behind -> padded to wall-clock.
        assert_eq!(a.padding(at(400), true), (0, 6_400));
        a.padded(Source::System, 6_400);
        assert_eq!(a.padding(at(400), true), (0, 0));

        // Mic keeps going to 10 s; playback starts at 10 s.
        a.received(Source::Mic, at(400), 153_600);
        let (mic, sys) = a.padding(at(10_000), true);
        assert_eq!(mic, 0);
        assert_eq!(sys, 153_600);
        a.padded(Source::System, sys);
        a.received(Source::System, at(10_000), 1_600);
        a.received(Source::Mic, at(10_000), 1_600);
        assert_eq!(a.padding(at(10_100), true), (0, 0));
        // Both sources sit at the same timeline position: WAV channels line up.
        assert_eq!(a.mic.position(), a.sys.position());
        assert_eq!(a.mic.position(), 161_600);

        // Mic-only recordings never pad the (inactive) system source.
        let mut m = Aligner::default();
        m.received(Source::Mic, t0, 1_600);
        assert_eq!(m.padding(at(1_000), false), (14_400, 0));
    }

    #[test]
    fn aligner_skips_pipeline_pauses_instead_of_writing_silence() {
        let t0 = Instant::now();
        let mut a = Aligner::default();
        a.received(Source::Mic, t0, 16_000);
        a.received(Source::System, t0, 8_000); // system went quiet after 0.5 s
        // Nothing processed for a minute (e.g. machine asleep): both far behind.
        let (mic, sys) = a.padding(t0 + Duration::from_secs(61), true);
        // The common gap is skipped; system audio still catches up to the mic.
        assert_eq!(mic, 0);
        assert_eq!(sys, 8_000);
    }

    #[test]
    fn recording_path_is_derived_from_id() {
        let id = uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let p = recording_path(Path::new("/data"), &id);
        assert_eq!(p, Path::new("/data/recordings/67e55044-10b1-426f-9247-bb680e5fe0c8.wav"));
    }
}
