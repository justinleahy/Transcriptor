//! Transcription engines. Both engines take 16 kHz mono f32 audio and return
//! timed utterances relative to the start of the chunk.

pub mod chunker;
pub mod cloud;
pub mod local;
pub mod models;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Utterance {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

pub trait Transcriber: Send {
    fn name(&self) -> &'static str;
    fn transcribe(&mut self, samples: &[f32], language: &str) -> Result<Vec<Utterance>>;
}

/// Whisper is prone to emitting these on near-silent audio.
pub fn is_hallucination(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_lowercase();
    if (lower.starts_with('[') && lower.ends_with(']'))
        || (lower.starts_with('(') && lower.ends_with(')'))
        || (lower.starts_with('*') && lower.ends_with('*'))
        || (lower.starts_with('♪'))
    {
        return true;
    }
    const KNOWN: &[&str] = &[
        "thank you.",
        "thanks for watching.",
        "thanks for watching!",
        "thank you for watching.",
        "thank you for watching!",
        "subtitles by the amara.org community",
        "you",
        "bye.",
        ".",
    ];
    KNOWN.contains(&lower.as_str())
}

/// Minimal in-memory WAV (PCM16 mono 16 kHz) encoder for upload.
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_sane() {
        let wav = encode_wav_pcm16(&[0.0; 160], 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 320);
    }

    #[test]
    fn hallucination_filter() {
        assert!(is_hallucination("[BLANK_AUDIO]"));
        assert!(is_hallucination(" (music) "));
        assert!(is_hallucination("Thanks for watching!"));
        assert!(!is_hallucination("Let's move the launch to Friday."));
    }
}
