//! Cloud transcription through any OpenAI-compatible
//! `POST {base_url}/audio/transcriptions` endpoint (OpenAI, Groq, local servers…).

use super::{encode_wav_pcm16, is_hallucination, Transcriber, Utterance};
use crate::audio::resample::TARGET_RATE;
use anyhow::{anyhow, Context, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use std::time::Duration;

/// Largest transcription response we accept (a chunk is at most ~60 s of audio).
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// Per-request ceiling, so a hung endpoint can't stall live transcription for long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CloudTranscriber {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

#[derive(Deserialize)]
struct VerboseSegment {
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    no_speech_prob: Option<f64>,
}

#[derive(Deserialize)]
struct VerboseResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    segments: Option<Vec<VerboseSegment>>,
}

impl CloudTranscriber {
    pub fn new(base_url: &str, model: &str, api_key: &str) -> Result<Self> {
        // Re-validate here too: settings.json could have been edited by hand.
        let base_url = crate::settings::normalize_cloud_url(base_url)?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        Ok(CloudTranscriber {
            client,
            endpoint: format!("{base_url}/audio/transcriptions"),
            model: model.to_string(),
            api_key: api_key.to_string(),
        })
    }

    async fn request(&self, wav: Vec<u8>, language: &str) -> Result<Vec<Utterance>> {
        let mut form = Form::new()
            .text("model", self.model.clone())
            .text("response_format", "verbose_json")
            .text("temperature", "0")
            .part(
                "file",
                Part::bytes(wav)
                    .file_name("chunk.wav")
                    .mime_str("audio/wav")?,
            );
        let lang = language.trim();
        if !lang.is_empty() && lang != "auto" {
            form = form.text("language", lang.to_string());
        }

        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .context("sending audio to transcription API")?;

        let status = resp.status();
        let body = crate::net::read_text_capped(resp, MAX_RESPONSE_BYTES, "transcription API").await?;
        if !status.is_success() {
            let snippet: String = body.chars().take(300).collect();
            return Err(anyhow!("transcription API returned {status}: {snippet}"));
        }

        let parsed: VerboseResponse = serde_json::from_str(&body)
            .with_context(|| format!("unexpected transcription response: {}", body.chars().take(200).collect::<String>()))?;

        let mut out = Vec::new();
        match parsed.segments {
            Some(segments) if !segments.is_empty() => {
                for s in segments {
                    let text = s.text.trim().to_string();
                    if is_hallucination(&text) || s.no_speech_prob.unwrap_or(0.0) > 0.85 {
                        continue;
                    }
                    out.push(Utterance {
                        start_ms: (s.start * 1000.0) as i64,
                        end_ms: (s.end * 1000.0) as i64,
                        text,
                    });
                }
            }
            _ => {
                let text = parsed.text.trim().to_string();
                if !is_hallucination(&text) {
                    out.push(Utterance {
                        start_ms: 0,
                        end_ms: -1, // caller fills in the chunk length
                        text,
                    });
                }
            }
        }
        Ok(out)
    }
}

impl Transcriber for CloudTranscriber {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn transcribe(&mut self, samples: &[f32], language: &str) -> Result<Vec<Utterance>> {
        let wav = encode_wav_pcm16(samples, TARGET_RATE);
        let chunk_ms = (samples.len() as i64 * 1000) / TARGET_RATE as i64;
        let mut utterances = tauri::async_runtime::block_on(self.request(wav, language))?;
        for u in &mut utterances {
            if u.end_ms < 0 {
                u.end_ms = chunk_ms;
            }
        }
        Ok(utterances)
    }
}
