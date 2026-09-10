//! Overlay protocol + future native renderer.
//!
//! Phase 1 keeps the wire protocol: `bin/dictate` atomically writes
//! `$RUNDIR/dictate-overlay-$UID/state.json`
//! (`{phase,label,text,audio,channels,channel,duration,expires}`) and the
//! existing Python/GTK renderer (`overlay.py`) displays it. Rust owns the
//! *writer* now; the renderer moves to `winit`/`gtk-rs` in phase 2.
//! Ports `notify()`, `next_state`, `changed_words` semantics.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Recording,
    Processing,
    Success,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OverlayMessage {
    pub phase: Phase,
    pub label: String,
    pub text: String,
    pub audio: String,
    pub channels: u32,
    pub channel: u32,
    pub duration: f64,
    pub expires: f64,
}

pub trait OverlayBackend {
    /// Atomically publish one message (temp file + rename, never partial).
    fn show(&self, msg: &OverlayMessage) -> Result<()>;
    fn demo(&self) -> Result<()>;
}

/// JSON-file backend consumed by the current Python renderer.
pub struct JsonFileOverlay {
    pub state_path: std::path::PathBuf,
}

impl OverlayBackend for JsonFileOverlay {
    fn show(&self, msg: &OverlayMessage) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(msg)?)?;
        std::fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }
    fn demo(&self) -> Result<()> {
        anyhow::bail!("demo not yet ported — using legacy bin/dictate")
    }
}

/// Port of the `notify()` title dispatch in `bin/dictate`.
/// Maps `(title, body)` to `(phase, label, text)` for the state.json writer.
pub fn map_notify(title: &str, body: &str) -> (Phase, String, String) {
    if title == "\u{1f3a4} recording\u{2026}" {
        return (Phase::Recording, "Listening".into(), String::new());
    }
    if let Some(rest) = title.strip_prefix("\u{1f3a4} ") {
        return (Phase::Recording, "Listening".into(), rest.to_string());
    }
    if let Some(rest) = title.strip_prefix("\u{23f3} ") {
        return (Phase::Processing, rest.to_string(), body.to_string());
    }
    if let Some(rest) = title.strip_prefix("dictate:") {
        return (Phase::Error, rest.trim_start().to_string(), body.to_string());
    }
    if let Some(rest) = title.strip_prefix("\u{1f4cb} ") {
        return (Phase::Success, "Copied to clipboard".into(), rest.to_string());
    }
    let mut text = title.to_string();
    if !body.is_empty() {
        text.push('\n');
        text.push_str(body);
    }
    (Phase::Success, "Ready".into(), text)
}

/// UTF-8 byte ranges of inserted/replaced words (for highlight).
/// Ports `changed_words()` in `overlay.py`; byte offsets are for Pango.
pub fn changed_words(previous: &str, current: &str) -> Vec<(usize, usize)> {
    let prev_words: Vec<&str> = previous.split_whitespace().collect();
    let cur_words: Vec<&str> = current.split_whitespace().collect();
    if prev_words == cur_words {
        return vec![];
    }
    // Byte offset table for current text.
    let mut offsets = vec![0usize];
    for ch in current.chars() {
        offsets.push(offsets.last().unwrap() + ch.len_utf8());
    }
    // Word char-span -> byte-span lookup.
    let mut spans: Vec<(usize, usize)> = vec![];
    let mut ci = 0usize;
    for w in &cur_words {
        let rel = current[ci..].find(w).unwrap_or(0);
        let s = ci + rel;
        spans.push((s, s + w.len()));
        ci = s + w.len();
    }
    // Longest-common-subsequence on words to find inserts/replaces.
    // Small N (preview text); O(n*m) DP is fine and dependency-free.
    let n = prev_words.len();
    let m = cur_words.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if prev_words[i] == cur_words[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Walk: skip matches, emit each maximal non-match run in `current`.
    let mut ranges = vec![];
    let (mut i, mut j) = (0, 0);
    while j < m {
        if i < n && prev_words[i] == cur_words[j] {
            i += 1;
            j += 1;
        } else {
            let start = j;
            // Advance j past words not aligned; advance i opportunistically.
            while j < m && (i >= n || prev_words[i] != cur_words[j]) {
                // If cur[j] appears later in prev, treat as delete-skip.
                if i < n && prev_words[i..].contains(&cur_words[j]) {
                    break;
                }
                j += 1;
                // Consume one prev word per cur word when both advance (replace).
                if i < n {
                    // Only consume if prev[i] doesn't match a later cur word.
                    if !cur_words[j..].contains(&prev_words[i]) {
                        i += 1;
                    }
                }
            }
            if j > start {
                // Convert char-span run to byte range.
                let char_start: usize = current[..spans[start].0].chars().count();
                let _ = char_start;
                ranges.push((spans[start].0, spans[j - 1].1));
            }
            // Skip deleted prev words.
            while i < n && !cur_words[j..].contains(&prev_words[i]) {
                i += 1;
            }
        }
    }
    // Merge adjacent ranges separated only by whitespace into one, matching
    // Python difflib opcode coalescing closely enough for highlight use.
    let mut merged: Vec<(usize, usize)> = vec![];
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if current[last.1..s].trim().is_empty() {
                last.1 = e;
                continue;
            }
        }
        merged.push((s, e));
    }
    let _ = offsets;
    merged
}

pub fn keep_preview_on_empty_processing(previous: &str, incoming_text: &str) -> String {
    if incoming_text.is_empty() {
        previous.to_string()
    } else {
        incoming_text.to_string()
    }
}

pub fn _unused_path_hook(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_changes() {
        assert!(changed_words("same words", "same words").is_empty());
    }

    #[test]
    fn appended_word_highlighted() {
        let r = changed_words("keep these words", "keep these words steady");
        assert_eq!(r.len(), 1);
        assert_eq!(&"keep these words steady"[r[0].0..r[0].1], "steady");
    }

    #[test]
    fn unicode_ranges_are_byte_offsets() {
        let cur = "P\u{159}\u{ED}li\u{161} kr\u{e1}sn\u{e9} \u{65e5}\u{672c}\u{8a9e}";
        let r = changed_words("P\u{159}\u{ED}li\u{161} \u{65e5}\u{672c}\u{8a9e}", cur);
        assert_eq!(r.len(), 1);
        assert_eq!(&cur[r[0].0..r[0].1], "kr\u{e1}sn\u{e9}");
    }

    #[test]
    fn empty_processing_keeps_preview() {
        assert_eq!(keep_preview_on_empty_processing("Keep this", ""), "Keep this");
        assert_eq!(keep_preview_on_empty_processing("Keep this", "New"), "New");
    }

    #[test]
    fn notify_titles_map_to_phases() {
        use super::{map_notify, Phase};
        let (p, l, t) = map_notify("\u{1f3a4} recording\u{2026}", "");
        assert_eq!((p, l.as_str(), t.as_str()), (Phase::Recording, "Listening", ""));
        let (p, l, t) = map_notify("\u{1f3a4} hello world", "");
        assert_eq!((p, l.as_str(), t.as_str()), (Phase::Recording, "Listening", "hello world"));
        let (p, l, t) = map_notify("\u{23f3} Transcribing", "");
        assert_eq!((p, l.as_str(), t.as_str()), (Phase::Processing, "Transcribing", ""));
        let (p, l, _) = map_notify("dictate: model missing", "/path — run: dictate doctor");
        assert_eq!(p, Phase::Error);
        assert_eq!(l, "model missing");
        let (p, l, t) = map_notify("\u{1f4cb} hello", "");
        assert_eq!((p, l.as_str(), t.as_str()), (Phase::Success, "Copied to clipboard", "hello"));
        let (p, l, t) = map_notify("final text", "");
        assert_eq!((p, l.as_str(), t.as_str()), (Phase::Success, "Ready", "final text"));
        let (_, _, t) = map_notify("final text", "extra body");
        assert_eq!(t, "final text\nextra body");
    }
}
