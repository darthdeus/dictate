//! Benchmark the actual correction/style pass without capturing or pasting.
//! Input: one {"id": ..., "text": ...} JSON object per line. OpenRouter only,
//! to avoid consuming the live dictation endpoint's Groq free-tier allowance.
use anyhow::Result;
use dictate_lib::{config, correct::Corrector, paths::Paths, text, usage::Meter};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

fn main() -> Result<()> {
    dictate_lib::paths::set_private_umask();
    let env = config::Env::from_process();
    let cfg = config::load_from_env(&env)?;
    let paths = Paths::resolve(&cfg.as_env(&env))?;
    let terms = cfg
        .get("DICTATE_TERMS")
        .or_else(|| cfg.get("DICTATE_PROMPT"))
        .unwrap_or("");
    let mut corrector = Corrector::from_config(&cfg, terms)?;
    corrector.endpoints.retain(|e| e.api == "openrouter");
    anyhow::ensure!(
        !corrector.endpoints.is_empty(),
        "OpenRouter correction is unavailable"
    );
    corrector.skip_file = None;
    corrector.meter = Some(Meter::new(&paths.state_dir, &cfg, "bench-whisper"));
    let mut cache = HashMap::<String, serde_json::Value>::new();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for line in io::stdin().lock().lines() {
        let row: serde_json::Value = serde_json::from_str(&line?)?;
        let raw = row["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing text"))?;
        let result = cache.entry(raw.to_string()).or_insert_with(|| {
            let (corrected, via, ok) = match corrector.correct_via(raw) {
                Ok(c) => (c.text, c.via, true),
                Err(_) => (raw.to_string(), "failed".into(), false),
            };
            let styled = text::restyle(&corrected, cfg.get_or("DICTATE_STYLE", "0") == "1", terms);
            serde_json::json!({"corrected": corrected, "styled": styled, "via": via, "ok": ok})
        });
        let mut result = result.clone();
        result["id"] = row["id"].clone();
        result["text"] = row["text"].clone();
        writeln!(stdout, "{result}")?;
        stdout.flush()?;
    }
    Ok(())
}
