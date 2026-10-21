//! ElevenLabs Scribe as the final-pass recognizer (`DICTATE_FINAL=elevenlabs`).
//! Best accuracy in the 2026-10-21 bench (4.5% WER vs whisper's 9.6% on the
//! 12 reference clips, every glossary term right), at ~1 s per request
//! instead of whisper's ~0.1 s. The glossary goes as keyterms. The key is
//! `ELEVENLABS_API_KEY` from the environment or the private keys file.

use anyhow::{Context, Result};
use std::time::Duration;

use crate::config::Config;

pub const URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
pub const DEFAULT_MODEL: &str = "scribe_v2";
/// The API's keyterm limit.
const MAX_TERMS: usize = 100;

pub struct Scribe {
    key: String,
    model: String,
    lang: String,
    terms: Vec<String>,
    timeout: Duration,
    pub meter: Option<crate::usage::Meter>,
}

impl Scribe {
    pub fn from_config(cfg: &Config, terms: &str) -> Result<Self> {
        let key = crate::correct::key("ELEVENLABS_API_KEY")?;
        let lang = cfg.get_or("DICTATE_LANG", "en");
        Ok(Self {
            key,
            model: cfg.get_or("DICTATE_ELEVENLABS_MODEL", DEFAULT_MODEL),
            lang: if lang == "auto" { String::new() } else { lang },
            terms: terms.split(',').map(str::trim).filter(|t| !t.is_empty()).take(MAX_TERMS).map(String::from).collect(),
            timeout: Duration::from_secs_f64(cfg.get_or("DICTATE_ELEVENLABS_TIMEOUT", "10").parse().unwrap_or(10.0)),
            meter: None,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Text for 16 kHz mono samples, sent whole (no 30 s window to respect).
    pub fn transcribe(&self, samples: &[i16]) -> Result<String> {
        let mut wav = Vec::new();
        crate::wav::encode_mono16(&mut wav, samples);
        let boundary = format!("dictate{:x}", std::process::id() as u64 ^ crate::pipeline::now_unix().to_bits());
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        };
        field("model_id", &self.model);
        field("tag_audio_events", "false");
        if !self.lang.is_empty() {
            field("language_code", &self.lang);
        }
        for t in &self.terms {
            field("keyterms", t);
        }
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
        body.extend_from_slice(&wav);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let t = std::time::Instant::now();
        let resp = self.request(&boundary, &body);
        if let Some(m) = &self.meter {
            let secs = samples.len() as f64 / crate::wav::RATE as f64;
            // Billed per second of audio sent, failed requests included.
            m.record(crate::usage::ApiCall { api: "elevenlabs".into(), model: self.model.clone(), audio_secs: secs,
                ms: t.elapsed().as_secs_f64() * 1000.0, ok: resp.is_ok(), usd: secs / 3600.0 * m.elevenlabs_hour,
                ..Default::default() });
        }
        Ok(resp?.get("text").and_then(|t| t.as_str()).context("no text in response")?.trim().to_string())
    }

    fn request(&self, boundary: &str, body: &[u8]) -> Result<serde_json::Value> {
        Ok(ureq::post(URL)
            .timeout(self.timeout)
            .set("xi-api-key", &self.key)
            .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .send_bytes(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => anyhow::anyhow!("HTTP {code}: {}",
                    r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
                ureq::Error::Transport(t) => anyhow::anyhow!("{}", t.kind()),
            })?
            .into_json()?)
    }
}
