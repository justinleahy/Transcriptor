//! Energy-based voice-activity chunker. Accumulates 16 kHz mono audio and cuts
//! it into chunks at pauses (or at a maximum length), skipping chunks that
//! contain no speech so we never ask Whisper to transcribe silence.

use crate::audio::resample::TARGET_RATE;

const FRAME_MS: usize = 20;
const FRAME_SAMPLES: usize = TARGET_RATE as usize * FRAME_MS / 1000;

pub struct Chunk {
    pub start_ms: i64,
    pub end_ms: i64,
    pub samples: Vec<f32>,
}

pub struct ChunkerConfig {
    /// Don't cut before this much audio (ms).
    pub min_chunk_ms: usize,
    /// Prefer to cut around here when a pause appears (ms).
    pub target_chunk_ms: usize,
    /// Force a cut at this length even mid-sentence (ms).
    pub max_chunk_ms: usize,
    /// A pause this long ends a chunk (ms).
    pub silence_ms: usize,
    /// Chunks with less speech than this are dropped (ms).
    pub min_speech_ms: usize,
}

impl ChunkerConfig {
    pub fn for_target_seconds(target: u32) -> Self {
        let target_ms = (target.max(3) as usize) * 1000;
        ChunkerConfig {
            min_chunk_ms: (target_ms / 4).max(1500),
            target_chunk_ms: target_ms,
            max_chunk_ms: target_ms * 2,
            silence_ms: 600,
            min_speech_ms: 350,
        }
    }
}

pub struct Chunker {
    cfg: ChunkerConfig,
    buf: Vec<f32>,
    /// Timeline position (ms) of buf[0] relative to the start of the recording.
    buf_start_ms: i64,
    /// Total samples consumed so far (drives the timeline).
    consumed: u64,
    /// Partial frame waiting for more samples.
    pending: Vec<f32>,
    speech_ms: usize,
    trailing_silence_ms: usize,
    noise_floor: f32,
}

impl Chunker {
    pub fn new(cfg: ChunkerConfig) -> Self {
        Chunker {
            cfg,
            buf: Vec::new(),
            buf_start_ms: 0,
            consumed: 0,
            pending: Vec::new(),
            speech_ms: 0,
            trailing_silence_ms: 0,
            noise_floor: 0.002,
        }
    }

    fn ms(samples: u64) -> i64 {
        (samples * 1000 / TARGET_RATE as u64) as i64
    }

    fn buf_ms(&self) -> usize {
        self.buf.len() * 1000 / TARGET_RATE as usize
    }

    fn is_speech(&mut self, frame: &[f32]) -> bool {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        // Adaptive noise floor: follow quiet frames quickly, loud ones slowly.
        if rms < self.noise_floor * 1.5 {
            self.noise_floor = self.noise_floor * 0.95 + rms * 0.05;
        } else {
            self.noise_floor = (self.noise_floor * 1.002).min(0.05);
        }
        self.noise_floor = self.noise_floor.max(0.0005);
        rms > (self.noise_floor * 3.0).max(0.008)
    }

    /// Push samples; returns any completed chunks.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Chunk> {
        let mut out = Vec::new();
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while self.pending.len() - offset >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.pending[offset..offset + FRAME_SAMPLES].to_vec();
            offset += FRAME_SAMPLES;
            if let Some(c) = self.push_frame(&frame) {
                out.push(c);
            }
        }
        self.pending.drain(..offset);
        out
    }

    fn push_frame(&mut self, frame: &[f32]) -> Option<Chunk> {
        let speech = self.is_speech(frame);

        if self.buf.is_empty() {
            self.buf_start_ms = Self::ms(self.consumed);
        }
        self.buf.extend_from_slice(frame);
        self.consumed += frame.len() as u64;

        if speech {
            self.speech_ms += FRAME_MS;
            self.trailing_silence_ms = 0;
        } else {
            self.trailing_silence_ms += FRAME_MS;
        }

        let len_ms = self.buf_ms();

        // Nothing but silence so far: keep the buffer short and move on.
        if self.speech_ms == 0 && len_ms >= self.cfg.min_chunk_ms {
            self.reset_buffer();
            return None;
        }

        let pause = self.trailing_silence_ms >= self.cfg.silence_ms;
        let long_enough = len_ms >= self.cfg.min_chunk_ms;
        let past_target = len_ms >= self.cfg.target_chunk_ms;
        let too_long = len_ms >= self.cfg.max_chunk_ms;

        // Cut at a pause once we have enough, or at a shorter pause once past
        // the target length, or unconditionally when too long.
        let cut = too_long
            || (pause && long_enough)
            || (past_target && self.trailing_silence_ms >= self.cfg.silence_ms / 2);

        if cut {
            return self.take_chunk();
        }
        None
    }

    fn reset_buffer(&mut self) {
        self.buf.clear();
        self.speech_ms = 0;
        self.trailing_silence_ms = 0;
    }

    fn take_chunk(&mut self) -> Option<Chunk> {
        let total_ms = self.buf_ms();
        // Keep a little trailing silence so Whisper sees the word end.
        let keep_silence_ms = 250.min(self.trailing_silence_ms);
        let trim_ms = self.trailing_silence_ms.saturating_sub(keep_silence_ms);
        let trim_samples = trim_ms * TARGET_RATE as usize / 1000;
        let mut samples = std::mem::take(&mut self.buf);
        let keep = samples.len().saturating_sub(trim_samples);
        let trimmed: Vec<f32> = samples.split_off(keep);
        let start_ms = self.buf_start_ms;
        let end_ms = start_ms + (total_ms - trim_ms) as i64;
        let speech_ms = self.speech_ms;
        self.reset_buffer();

        // The trimmed trailing silence starts the next buffer so the timeline stays continuous.
        if !trimmed.is_empty() {
            self.buf_start_ms = end_ms;
            self.buf = trimmed;
            self.trailing_silence_ms = trim_ms;
        }

        if speech_ms < self.cfg.min_speech_ms {
            return None;
        }
        Some(Chunk {
            start_ms,
            end_ms,
            samples,
        })
    }

    /// Flush whatever is buffered (end of recording).
    pub fn flush(&mut self) -> Option<Chunk> {
        if !self.pending.is_empty() {
            let frame = std::mem::take(&mut self.pending);
            if self.buf.is_empty() {
                self.buf_start_ms = Self::ms(self.consumed);
            }
            self.buf.extend_from_slice(&frame);
            self.consumed += frame.len() as u64;
        }
        if self.buf.is_empty() || self.speech_ms < self.cfg.min_speech_ms {
            self.reset_buffer();
            return None;
        }
        let samples = std::mem::take(&mut self.buf);
        let start_ms = self.buf_start_ms;
        let end_ms = start_ms + (samples.len() * 1000 / TARGET_RATE as usize) as i64;
        self.reset_buffer();
        Some(Chunk {
            start_ms,
            end_ms,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amp: f32) -> Vec<f32> {
        let n = ms * TARGET_RATE as usize / 1000;
        (0..n)
            .map(|i| amp * ((i as f32 * 0.1).sin()))
            .collect()
    }

    #[test]
    fn silence_produces_nothing() {
        let mut c = Chunker::new(ChunkerConfig::for_target_seconds(8));
        let chunks = c.push(&vec![0.0f32; TARGET_RATE as usize * 30]);
        assert!(chunks.is_empty());
        assert!(c.flush().is_none());
    }

    #[test]
    fn speech_then_pause_cuts_a_chunk() {
        let mut c = Chunker::new(ChunkerConfig::for_target_seconds(8));
        let mut audio = tone(3000, 0.3);
        audio.extend(vec![0.0f32; TARGET_RATE as usize]); // 1 s pause
        let chunks = c.push(&audio);
        assert_eq!(chunks.len(), 1);
        let ch = &chunks[0];
        assert_eq!(ch.start_ms, 0);
        assert!(ch.end_ms >= 3000 && ch.end_ms <= 3400, "end {}", ch.end_ms);
    }

    #[test]
    fn forced_cut_when_too_long() {
        let mut c = Chunker::new(ChunkerConfig::for_target_seconds(4));
        let chunks = c.push(&tone(20_000, 0.3));
        assert!(chunks.len() >= 2);
        for w in chunks.windows(2) {
            assert_eq!(w[0].end_ms, w[1].start_ms);
        }
    }
}
