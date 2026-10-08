//! Downmix + resample to 16 kHz mono f32 (what Whisper and the cloud APIs want).
//! Linear interpolation with a light box pre-filter is plenty for speech.

pub const TARGET_RATE: u32 = 16_000;

pub struct Resampler {
    src_rate: u32,
    /// Fractional read position in the source stream, in source samples.
    pos: f64,
    /// Last source sample carried over for interpolation across calls.
    last: Option<f32>,
}

impl Resampler {
    pub fn new(src_rate: u32) -> Self {
        Resampler {
            src_rate,
            pos: 0.0,
            last: None,
        }
    }

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    /// Feed interleaved samples; returns 16 kHz mono samples.
    pub fn process(&mut self, interleaved: &[f32], channels: u16) -> Vec<f32> {
        let ch = channels.max(1) as usize;
        let frames = interleaved.len() / ch;
        if frames == 0 {
            return Vec::new();
        }

        // Downmix to mono.
        let mut mono: Vec<f32> = Vec::with_capacity(frames + 1);
        if let Some(l) = self.last {
            mono.push(l);
        }
        for f in 0..frames {
            let mut acc = 0.0f32;
            for c in 0..ch {
                acc += interleaved[f * ch + c];
            }
            mono.push(acc / ch as f32);
        }

        if self.src_rate == TARGET_RATE {
            self.last = None;
            return if self.last.is_none() && mono.len() == frames {
                mono
            } else {
                mono.split_off(mono.len() - frames)
            };
        }

        let ratio = self.src_rate as f64 / TARGET_RATE as f64;
        let mut out = Vec::with_capacity((frames as f64 / ratio) as usize + 2);
        // `pos` is relative to mono[0]; mono[0] may be the carried sample.
        let n = mono.len();
        while (self.pos as usize) + 1 < n {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = mono[i];
            let b = mono[i + 1];
            out.push(a + (b - a) * frac);
            self.pos += ratio;
        }
        // Carry the last sample so interpolation continues across the boundary.
        let last_idx = n - 1;
        self.last = Some(mono[last_idx]);
        self.pos -= last_idx as f64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_sample_count_for_32k_stereo() {
        let mut r = Resampler::new(32_000);
        // 32000 frames stereo = 1 s -> ~16000 mono samples
        let data = vec![0.5f32; 32_000 * 2];
        let mut total = 0;
        for chunk in data.chunks(1024) {
            total += r.process(chunk, 2).len();
        }
        assert!((15_990..=16_000).contains(&total), "got {total}");
    }

    #[test]
    fn passthrough_at_16k() {
        let mut r = Resampler::new(16_000);
        let data = vec![0.25f32; 320];
        assert_eq!(r.process(&data, 1).len(), 320);
        assert_eq!(r.process(&data, 1).len(), 320);
    }
}
