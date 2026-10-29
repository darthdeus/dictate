//! What dictation costs and how fast each setup is. Every billed API request
//! appends a line to `usage.jsonl` (cost computed at request time from the
//! `DICTATE_PRICE_*` settings, so later price changes don't rewrite the
//! past), and every delivered dictation appends one to `metrics.jsonl`.
//! `dictate stats [days]` summarizes both.
//!
//! Pause passes count. With `DICTATE_INCREMENTAL=0` each one sends the whole
//! recording so far, so a dictation with several pauses is billed for its
//! audio several times; incrementally each second is sent once.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// ElevenLabs Scribe v2, USD per hour of audio: $0.22 base + $0.05 keyterms
/// (elevenlabs.io/pricing/api, 2026-10-21).
pub const ELEVENLABS_HOUR: &str = "0.27";
/// Groq qwen/qwen3.8-27b, USD per million tokens (console.groq.com, 2026-10-21).
pub const GROQ_IN: &str = "0.80";
pub const GROQ_OUT: &str = "4.00";

#[derive(Serialize, Deserialize, Default)]
pub struct ApiCall {
    pub ts: f64,
    /// `early` (pause pass) or `final`.
    pub purpose: String,
    pub api: String,
    pub model: String,
    #[serde(default)]
    pub audio_secs: f64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    pub ms: f64,
    pub ok: bool,
    pub usd: f64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Dictation {
    pub ts: f64,
    pub id: String,
    pub audio_secs: f64,
    /// The final-pass setup, as shown on the card.
    pub setup: String,
    pub early_used: bool,
    pub total_ms: f64,
    pub recognizer_ms: Option<f64>,
    pub correction_ms: Option<f64>,
    pub paste_ms: Option<f64>,
    /// Incremental final pass: pause segments recognized while recording,
    /// and the seconds after the last one recognized at stop.
    #[serde(default)]
    pub segments: usize,
    #[serde(default)]
    pub tail_secs: Option<f64>,
}

/// Where a process records its API calls, and why it is calling.
#[derive(Clone)]
pub struct Meter {
    file: PathBuf,
    purpose: &'static str,
    pub elevenlabs_hour: f64,
    pub groq_in: f64,
    pub groq_out: f64,
}

impl Meter {
    pub fn new(state_dir: &Path, cfg: &Config, purpose: &'static str) -> Self {
        let price = |k: &str, d: &str| cfg.get_or(k, d).parse().unwrap_or_else(|_| d.parse().unwrap());
        Self {
            file: state_dir.join("usage.jsonl"),
            purpose,
            elevenlabs_hour: price("DICTATE_PRICE_ELEVENLABS_HOUR", ELEVENLABS_HOUR),
            groq_in: price("DICTATE_PRICE_GROQ_IN", GROQ_IN),
            groq_out: price("DICTATE_PRICE_GROQ_OUT", GROQ_OUT),
        }
    }

    pub fn record(&self, mut call: ApiCall) {
        call.ts = crate::pipeline::now_unix();
        call.purpose = self.purpose.into();
        append(&self.file, &call);
    }
}

/// Minutes of audio sent for `purpose` so far, all time (the shadow budget).
pub fn audio_minutes(state_dir: &Path, purpose: &str) -> f64 {
    read(&state_dir.join("usage.jsonl"), 0.0, |c: &ApiCall| c.ts).iter()
        .filter(|c: &&ApiCall| c.purpose == purpose)
        .map(|c| c.audio_secs).sum::<f64>() / 60.0
}

/// Dollars spent on a dictation's own requests (pause passes and the final
/// pass, not the shadow label made after it) since `since`.
pub fn dictation_usd(state_dir: &Path, since: f64) -> f64 {
    read(&state_dir.join("usage.jsonl"), since, |c: &ApiCall| c.ts)
        .iter()
        .filter(|c: &&ApiCall| c.purpose == "early" || c.purpose == "final")
        .map(|c| c.usd)
        .sum()
}

/// Shadow labels against what was pasted: (dictations, identical, words, words differing).
fn shadow_agreement(state_dir: &Path, since: f64) -> (usize, usize, usize, usize) {
    let mut out = (0, 0, 0, 0);
    let Ok(dirs) = std::fs::read_dir(state_dir.join("history")) else { return out };
    for d in dirs.flatten().map(|d| d.path()) {
        let read = |n: &str| std::fs::read_to_string(d.join(n)).ok();
        let (Some(shadow), Some(pasted)) = (read("transcript.elevenlabs.txt"), read("transcript.txt")) else { continue };
        let fresh = std::fs::metadata(d.join("transcript.elevenlabs.txt")).and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).is_some_and(|t| t.as_secs_f64() >= since);
        if !fresh {
            continue;
        }
        let (a, b) = (crate::correct::words(&shadow), crate::correct::words(&pasted));
        let diff = crate::correct::edit_distance(&a, &b);
        out.0 += 1;
        out.1 += usize::from(diff == 0);
        out.2 += a.len();
        out.3 += diff;
    }
    out
}

pub fn record_dictation(state_dir: &Path, d: &Dictation) {
    append(&state_dir.join("metrics.jsonl"), d);
}

/// One `dictate hotkey` action (`keys.jsonl`): which key did what where, so
/// the bindings can be tuned to how they are used.
#[derive(Serialize, Deserialize, Default)]
pub struct KeyPress {
    pub ts: f64,
    /// `f17`, or the X keycode for keys outside f13-f24.
    pub key: String,
    /// The binding, as written in `DICTATE_HOTKEYS` (`talk`, `key:Return`).
    pub action: String,
    #[serde(default)]
    pub layer: u8,
    /// Class of the focused window.
    pub app: String,
    /// Command in the focused tmux pane, when there is one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane: String,
    /// Talk keys: seconds held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<f64>,
}

pub fn log_key(state_dir: &Path, row: &KeyPress) {
    append(&state_dir.join("keys.jsonl"), row);
}

/// `dictate keys [days]`: use per action and where, then what follows what.
pub fn summarize_keys(state_dir: &Path, days: f64) -> Result<String> {
    let since = crate::pipeline::now_unix() - days * 86400.0;
    let mut rows: Vec<KeyPress> = read(&state_dir.join("keys.jsonl"), since, |k: &KeyPress| k.ts);
    rows.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let mut out = format!("Last {days} days: {} key actions\n\n", rows.len());
    let label = |k: &KeyPress| if k.layer > 0 { format!("{} (layer {})", k.action, k.layer) } else { k.action.clone() };
    let mut by: BTreeMap<String, (String, BTreeMap<String, usize>)> = BTreeMap::new();
    for k in &rows {
        let e = by.entry(label(k)).or_insert_with(|| (k.key.clone(), BTreeMap::new()));
        *e.1.entry(if k.pane.is_empty() { k.app.clone() } else { k.pane.clone() }).or_default() += 1;
    }
    let mut actions: Vec<_> = by.iter().map(|(a, (key, apps))| (apps.values().sum::<usize>(), a, key, apps)).collect();
    actions.sort_by_key(|a| std::cmp::Reverse(a.0));
    out += "action                       key     count  where\n";
    for (n, action, key, apps) in &actions {
        let mut apps: Vec<_> = apps.iter().collect();
        apps.sort_by(|a, b| b.1.cmp(a.1));
        let wher = apps.iter().take(4).map(|(a, c)| format!("{a} {c}")).collect::<Vec<_>>().join(", ");
        out += &format!("{action:<28} {key:<7} {n:>5}  {wher}\n");
    }
    let held: Vec<f64> = rows.iter().filter_map(|k| k.held).collect();
    if let (Some(p50), Some(p90)) = (pct(held.clone(), 0.5), pct(held, 0.9)) {
        out += &format!("\ntalk held: p50 {p50:.1} s, p90 {p90:.1} s\n");
    }
    // Pairs within 30 s: habits worth a single button (talk then Enter).
    let mut pairs: BTreeMap<(String, String), usize> = BTreeMap::new();
    for w in rows.windows(2) {
        if w[1].ts - w[0].ts <= 30.0 {
            *pairs.entry((label(&w[0]), label(&w[1]))).or_default() += 1;
        }
    }
    let mut pairs: Vec<_> = pairs.into_iter().collect();
    pairs.sort_by_key(|a| std::cmp::Reverse(a.1));
    if !pairs.is_empty() {
        out += "\nthen within 30 s\n";
        for ((a, b), n) in pairs.iter().take(10) {
            out += &format!("  {a} -> {b}: {n}\n");
        }
    }
    Ok(out)
}

fn append<T: Serialize>(file: &Path, row: &T) {
    if let (Ok(line), Ok(mut f)) = (serde_json::to_string(row),
        std::fs::OpenOptions::new().create(true).append(true).open(file)) {
        let _ = writeln!(f, "{line}");
    }
}

fn read<T: for<'de> Deserialize<'de>>(file: &Path, since: f64, ts: impl Fn(&T) -> f64) -> Vec<T> {
    std::fs::read_to_string(file).unwrap_or_default().lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|r| ts(r) >= since)
        .collect()
}

fn pct(mut v: Vec<f64>, p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[((v.len() - 1) as f64 * p).round() as usize])
}

fn ms(v: Option<f64>) -> String {
    v.map_or("-".into(), |x| format!("{x:.0}"))
}

/// `dictate stats [days]`: cost per API and model, then speed per setup.
pub fn summarize(state_dir: &Path, days: f64) -> Result<String> {
    let since = crate::pipeline::now_unix() - days * 86400.0;
    let calls: Vec<ApiCall> = read(&state_dir.join("usage.jsonl"), since, |c: &ApiCall| c.ts);
    let dictations: Vec<Dictation> = read(&state_dir.join("metrics.jsonl"), since, |d: &Dictation| d.ts);
    let mut out = format!("Last {days} days: {} dictations, {:.1} min of audio\n\n",
        dictations.len(), dictations.iter().map(|d| d.audio_secs).sum::<f64>() / 60.0 + 0.0);

    out += "Cost (USD)             requests  pause/final  audio min  tokens in/out      cost\n";
    let mut by: BTreeMap<String, Vec<&ApiCall>> = BTreeMap::new();
    for c in &calls {
        by.entry(format!("{} {}", c.api, c.model.rsplit('/').next().unwrap_or(&c.model))).or_default().push(c);
    }
    let total: f64 = calls.iter().map(|c| c.usd).sum::<f64>() + 0.0;
    for (name, cs) in &by {
        let early = cs.iter().filter(|c| c.purpose == "early").count();
        let audio: f64 = cs.iter().map(|c| c.audio_secs).sum();
        let (ti, to): (u64, u64) = cs.iter().fold((0, 0), |a, c| (a.0 + c.input_tokens, a.1 + c.output_tokens));
        out += &format!("{name:<22} {:>8}  {:>5}/{:<5}  {:>9.1}  {:>8}/{:<7}  {:>8.4}\n",
            cs.len(), early, cs.len() - early, audio / 60.0, ti, to, cs.iter().map(|c| c.usd).sum::<f64>());
    }
    out += &format!("{:<22} {:>8}  {:>45}\n", "total", calls.len(), format!("{total:.4}"));
    if !dictations.is_empty() {
        let audio_h = dictations.iter().map(|d| d.audio_secs).sum::<f64>() / 3600.0;
        out += &format!("per dictation {:.5} \u{b7} per hour dictated {:.3}\n",
            total / dictations.len() as f64, if audio_h > 0.0 { total / audio_h } else { 0.0 });
    }

    let mut setups: BTreeMap<&str, Vec<&Dictation>> = BTreeMap::new();
    for d in &dictations {
        setups.entry(d.setup.trim_start_matches("Rust \u{b7} final: ")).or_default().push(d);
    }
    let w = setups.keys().map(|k| k.chars().count()).max().unwrap_or(0).max(19);
    out += &format!("\n{:<w$} {:>4}  release p50/p90  recognizer p50  correction p50  paste p50  pause pass\n",
        "Speed by setup (ms)", "n");
    for (setup, ds) in setups {
        let col = |f: fn(&Dictation) -> Option<f64>| pct(ds.iter().filter_map(|d| f(d)).collect(), 0.5);
        let early = ds.iter().filter(|d| d.early_used).count();
        out += &format!("{setup:<w$} {:>4}  {:>7}/{:<7}  {:>14}  {:>14}  {:>9}  {:>4}/{}\n", ds.len(),
            ms(pct(ds.iter().map(|d| d.total_ms).collect(), 0.5)), ms(pct(ds.iter().map(|d| d.total_ms).collect(), 0.9)),
            ms(col(|d| d.recognizer_ms)), ms(col(|d| d.correction_ms)), ms(col(|d| d.paste_ms)), early, ds.len());
    }
    let (n, same, words, diff) = shadow_agreement(state_dir, since);
    if n > 0 {
        out += &format!("\nElevenLabs shadow labels: {n} dictations, {same} identical to what was pasted, \
            {diff} of {words} words differ ({:.1}%); budget used {:.0} min\n",
            100.0 * diff as f64 / words.max(1) as f64, audio_minutes(state_dir, "shadow"));
    }
    out += "\n(recognizer/correction times exclude dictations finished during a pause; paid\n\
            requests include pause passes whose text was later replaced)\n";
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(pct(vec![3.0, 1.0, 2.0], 0.5), Some(2.0));
        assert_eq!(pct(vec![], 0.5), None);
    }

    #[test]
    fn summary_reads_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load_from_env(&crate::config::Env::with_vars([])).unwrap();
        Meter::new(dir.path(), &cfg, "early").record(ApiCall { api: "elevenlabs".into(), model: "scribe_v2".into(),
            audio_secs: 60.0, ok: true, usd: 0.0045, ..Default::default() });
        record_dictation(dir.path(), &Dictation { ts: crate::pipeline::now_unix(), setup: "Rust \u{b7} final: x".into(),
            total_ms: 500.0, audio_secs: 60.0, ..Default::default() });
        let s = summarize(dir.path(), 1.0).unwrap();
        assert!(s.contains("elevenlabs scribe_v2"), "{s}");
        assert!(s.contains("1 dictations"), "{s}");
        assert!(s.contains("x "), "{s}");
    }
}
