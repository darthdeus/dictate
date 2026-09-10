//! Transcription backends.
//!
//! Ports `transcribe_whisper` (resident server preferred, `whisper-cli`
//! fallback — server is an optimization, never a dependency) and
//! `transcribe_parakeet` (one-shot only, always pays model load).
//!
//! Phase 1 shells out to `curl` + `whisper-cli` / `parakeet-cli` exactly
//! like bash. `--form-string` semantics preserved: prompt/language sent
//! literally, `@`-leading values must not become file references.
//! Binary paths are struct fields (defaulting to `curl`/`whisper-cli`)
//! so tests inject fakes without touching `PATH`.

use anyhow::{Context, Result};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Whisper,
    Parakeet,
}

impl Engine {
    pub fn parse(s: &str) -> Self {
        match s {
            "parakeet" => Self::Parakeet,
            _ => Self::Whisper,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WhisperConfig {
    /// Empty string forces CLI (mirrors `${SERVER-...}` + `-n` checks).
    pub server: String,
    pub model: String,
    pub language: String,
    pub prompt: String,
    pub threads: u32,
    pub vad_model: Option<String>,
    pub vad_threshold: String,
    pub curl_bin: String,
    pub whisper_bin: String,
}

impl WhisperConfig {
    pub fn new(server: &str, model: &str, language: &str, prompt: &str, threads: u32) -> Self {
        Self {
            server: server.to_string(),
            model: model.to_string(),
            language: language.to_string(),
            prompt: prompt.to_string(),
            threads,
            vad_model: None,
            vad_threshold: "0.5".into(),
            curl_bin: "curl".into(),
            whisper_bin: "whisper-cli".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParakeetConfig {
    pub model: String,
    pub threads: u32,
    pub bin: String,
}

pub trait Transcriber {
    fn transcribe(&self, wav: &Path) -> Result<String>;
}

/// `jq -er '.text'` semantics: response must contain a string `.text`;
/// missing/null/non-string is an error (falls through to CLI fallback),
/// never silent empty success.
pub fn parse_inference_text(json: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(json).context("dictate: bad transcription response")?;
    match v.get("text") {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        _ => anyhow::bail!("dictate: transcription response has no .text"),
    }
}

/// Exact curl argv (after binary). `--form-string` keeps `@...` literal.
pub fn curl_inference_args(server: &str, wav: &Path, language: &str, prompt: &str) -> Vec<String> {
    vec![
        "-sf".into(),
        "--connect-timeout".into(),
        "2".into(),
        "--max-time".into(),
        "120".into(),
        format!("{}/inference", server.trim_end_matches('/')),
        "-F".into(),
        format!("file=@{}", wav.display()),
        "-F".into(),
        "response_format=json".into(),
        "--form-string".into(),
        format!("language={language}"),
        "--form-string".into(),
        format!("prompt={prompt}"),
        "-F".into(),
        "temperature=0.0".into(),
    ]
}

pub fn whisper_cli_args(cfg: &WhisperConfig, wav: &Path) -> Vec<String> {
    let mut a = vec![
        "-m".into(),
        cfg.model.clone(),
        "-f".into(),
        wav.display().to_string(),
        "-l".into(),
        cfg.language.clone(),
        "-nt".into(),
        "-np".into(),
        "-sns".into(),
    ];
    if let Some(vm) = &cfg.vad_model {
        a.extend(["--vad".into(), "-vm".into(), vm.clone(), "-vt".into(), cfg.vad_threshold.clone()]);
    }
    a.extend(["-t".into(), cfg.threads.to_string(), "--prompt".into(), cfg.prompt.clone()]);
    a
}

/// Full whisper path: server first, CLI fallback on any failure.
/// `log: &Path` receives the same `note: ...` lines bash appends to `$LOG`,
/// plus backend stderr (bash `2>>"$LOG"` on every backend call, so a
/// "transcription failed" always has its backend message in the log).
pub fn transcribe_whisper(wav: &Path, cfg: &WhisperConfig, log: &Path) -> Result<String> {
    if !cfg.server.is_empty() {
        let args = curl_inference_args(&cfg.server, wav, &cfg.language, &cfg.prompt);
        if let Ok(out) = std::process::Command::new(&cfg.curl_bin).args(&args).output() {
            append_bytes(log, &out.stderr);
            if out.status.success() {
                let body = String::from_utf8_lossy(&out.stdout);
                if let Ok(text) = parse_inference_text(&body) {
                    return Ok(text);
                }
            }
        }
        append_line(log, "note: server unreachable or bad response, using whisper-cli");
    }
    let args = whisper_cli_args(cfg, wav);
    let out = std::process::Command::new(&cfg.whisper_bin)
        .args(&args)
        .output()
        .with_context(|| format!("dictate: cannot run {}", cfg.whisper_bin))?;
    append_bytes(log, &out.stderr);
    if !out.status.success() {
        anyhow::bail!("dictate: transcription failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn transcribe_parakeet(wav: &Path, cfg: &ParakeetConfig, log: &Path) -> Result<String> {
    let out = std::process::Command::new(&cfg.bin)
        .args([
            "-m", &cfg.model,
            "-f", &wav.to_string_lossy(),
            "-np", "-t", &cfg.threads.to_string(),
        ])
        .output()
        .with_context(|| format!("dictate: cannot run {}", cfg.bin))?;
    append_bytes(log, &out.stderr);
    if !out.status.success() {
        anyhow::bail!("dictate: transcription failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    if bytes.is_empty() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(bytes);
        if !bytes.ends_with(b"\n") {
            let _ = writeln!(f);
        }
    }
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn inference_text_requires_string_field() {
        assert_eq!(parse_inference_text(r#"{"text":"hello"}"#).unwrap(), "hello");
        assert!(parse_inference_text(r#"{"nope":1}"#).is_err());
        assert!(parse_inference_text(r#"{"text":null}"#).is_err());
        assert!(parse_inference_text(r#"{"text":42}"#).is_err());
        assert!(parse_inference_text("not json").is_err());
    }

    #[test]
    fn curl_uses_form_string_for_literal_values() {
        // Port of check_transcription.py: language/prompt forwarded literally.
        for (lang, prompt) in [
            ("auto", "example-project, sub-agent"),
            ("en", ""),
            ("", "@not-a-file;type=text/plain, $(literal) café"),
        ] {
            let args = curl_inference_args("http://127.0.0.1:8127", Path::new("f.wav"), lang, prompt);
            let forms: Vec<&str> = args
                .windows(2)
                .filter(|w| w[0] == "--form-string")
                .map(|w| w[1].as_str())
                .collect();
            assert!(forms.contains(&format!("prompt={prompt}").as_str()), "{forms:?}");
            assert!(forms.contains(&format!("language={lang}").as_str()), "{forms:?}");
            // No `-F prompt=...` (would interpret `@` as file).
            assert!(!args.windows(2).any(|w| w[0] == "-F" && w[1].starts_with("prompt=")));
        }
    }

    fn fake_bin(dir: &Path, name: &str, script: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn server_success_used_and_cli_skipped() {
        let _exec_guard = crate::FAKE_EXEC_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let curl = fake_bin(tmp.path(), "curl", "#!/bin/sh\necho '{\"text\":\"recognized text\"}'\n");
        let cli = fake_bin(tmp.path(), "whisper-cli", "#!/bin/sh\necho CLI-SHOULD-NOT-RUN; exit 3\n");
        let mut cfg = WhisperConfig::new("http://127.0.0.1:8127", "m.bin", "auto", "p", 8);
        cfg.curl_bin = curl;
        cfg.whisper_bin = cli;
        let log = tmp.path().join("log");
        let text = transcribe_whisper(Path::new("f.wav"), &cfg, &log).unwrap();
        assert_eq!(text, "recognized text");
    }

    #[test]
    fn bad_server_response_falls_back_to_cli() {
        let _exec_guard = crate::FAKE_EXEC_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let curl = fake_bin(tmp.path(), "curl", "#!/bin/sh\necho '{\"error\":1}'\n");
        let cli = fake_bin(tmp.path(), "whisper-cli", "#!/bin/sh\necho fallback-text\n");
        let mut cfg = WhisperConfig::new("http://x", "m.bin", "auto", "p", 8);
        cfg.curl_bin = curl;
        cfg.whisper_bin = cli;
        let log = tmp.path().join("log");
        let text = transcribe_whisper(Path::new("f.wav"), &cfg, &log).unwrap();
        assert_eq!(text.trim(), "fallback-text");
        assert!(std::fs::read_to_string(&log).unwrap().contains("using whisper-cli"));
    }

    #[test]
    fn unreachable_server_falls_back_to_cli() {
        let _exec_guard = crate::FAKE_EXEC_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let curl = fake_bin(tmp.path(), "curl", "#!/bin/sh\nexit 7\n");
        let cli = fake_bin(tmp.path(), "whisper-cli", "#!/bin/sh\necho cli-text\n");
        let mut cfg = WhisperConfig::new("http://x", "m.bin", "auto", "p", 8);
        cfg.curl_bin = curl;
        cfg.whisper_bin = cli;
        let text = transcribe_whisper(Path::new("f.wav"), &cfg, &tmp.path().join("log")).unwrap();
        assert_eq!(text.trim(), "cli-text");
    }

    #[test]
    fn empty_server_forces_cli() {
        let _exec_guard = crate::FAKE_EXEC_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let curl = fake_bin(tmp.path(), "curl", "#!/bin/sh\necho SHOULD-NOT-RUN; exit 0\n");
        let cli = fake_bin(tmp.path(), "whisper-cli", "#!/bin/sh\necho direct\n");
        let mut cfg = WhisperConfig::new("", "m.bin", "auto", "p", 8);
        cfg.curl_bin = curl;
        cfg.whisper_bin = cli;
        let text = transcribe_whisper(Path::new("f.wav"), &cfg, &tmp.path().join("log")).unwrap();
        assert_eq!(text.trim(), "direct");
    }
}
