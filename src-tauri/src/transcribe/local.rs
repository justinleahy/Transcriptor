//! On-device transcription with whisper.cpp via whisper-rs.

use super::{is_hallucination, Transcriber, Utterance};
use crate::audio::resample::TARGET_RATE;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

/// Loaded model contexts are expensive; keep the most recent one around.
type CachedContext = Option<(PathBuf, Arc<WhisperContext>)>;
static CONTEXT_CACHE: OnceLock<Mutex<CachedContext>> = OnceLock::new();
static LOGGING: OnceLock<()> = OnceLock::new();

fn load_context(model_path: &Path) -> Result<Arc<WhisperContext>> {
    LOGGING.get_or_init(|| {
        whisper_rs::install_logging_hooks();
    });
    let cache = CONTEXT_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock();
    if let Some((p, ctx)) = guard.as_ref() {
        if p == model_path {
            return Ok(ctx.clone());
        }
    }
    let path_str = model_path
        .to_str()
        .context("model path is not valid UTF-8")?;
    log::info!("loading whisper model {path_str}");
    let mut params = WhisperContextParameters::default();
    params.use_gpu(true);
    let ctx = WhisperContext::new_with_params(path_str, params)
        .with_context(|| format!("loading whisper model {path_str}"))?;
    let ctx = Arc::new(ctx);
    *guard = Some((model_path.to_path_buf(), ctx.clone()));
    Ok(ctx)
}

/// Drop any cached model context (call before process exit so ggml's Metal
/// backend tears down in the right order).
pub fn clear_cache() {
    if let Some(cache) = CONTEXT_CACHE.get() {
        *cache.lock() = None;
    }
}

pub struct LocalTranscriber {
    _ctx: Arc<WhisperContext>,
    state: WhisperState,
    threads: i32,
}

impl LocalTranscriber {
    pub fn new(model_path: &Path) -> Result<Self> {
        let ctx = load_context(model_path)?;
        let state = ctx.create_state().context("creating whisper state")?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4)
            .clamp(1, 8);
        Ok(LocalTranscriber {
            _ctx: ctx,
            state,
            threads,
        })
    }
}

impl Transcriber for LocalTranscriber {
    fn name(&self) -> &'static str {
        "whisper.cpp"
    }

    fn transcribe(&mut self, samples: &[f32], language: &str) -> Result<Vec<Utterance>> {
        // whisper.cpp refuses audio shorter than 1 s; pad a little with silence.
        let min_len = (TARGET_RATE as usize * 11) / 10;
        let padded: Vec<f32>;
        let audio: &[f32] = if samples.len() < min_len {
            let mut v = samples.to_vec();
            v.resize(min_len, 0.0);
            padded = v;
            &padded
        } else {
            samples
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        let lang = language.trim();
        if lang.is_empty() || lang == "auto" {
            params.set_language(None);
        } else {
            params.set_language(Some(lang));
        }
        params.set_n_threads(self.threads);
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_temperature(0.0);

        self.state
            .full(params, audio)
            .context("whisper inference failed")?;

        let mut out = Vec::new();
        for seg in self.state.as_iter() {
            let text = match seg.to_str_lossy() {
                Ok(t) => t.trim().to_string(),
                Err(_) => continue,
            };
            if is_hallucination(&text) {
                continue;
            }
            if seg.no_speech_probability() > 0.85 {
                continue;
            }
            out.push(Utterance {
                start_ms: seg.start_timestamp() * 10,
                end_ms: seg.end_timestamp() * 10,
                text,
            });
        }
        Ok(out)
    }
}
