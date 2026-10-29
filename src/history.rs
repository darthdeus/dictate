//! Recoverable history: one directory per utterance.
//!
//! Ports `history_*` in `bin/dictate`. Audio is copied before transcription,
//! transcript written before delivery, so either half survives a later crash.
//! Bounded by age (`HISTORY_DAYS`) and count (`HISTORY_MAX`).

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct HistoryStore {
    pub dir: PathBuf,
    pub enabled: bool,
    pub max_days: u64,
    pub max_entries: usize,
}

impl HistoryStore {
    /// `ready` folds in storage usability (bash `HISTORY_READY`): a history
    /// dir that could not be created degrades to disabled, never fatal.
    /// Bounds come from the merged config (file values included), not just
    /// the process environment.
    pub fn new(dir: PathBuf, enabled: bool, max_days: u64, max_entries: usize) -> Result<Self> {
        Ok(Self { dir, enabled, max_days, max_entries })
    }

    pub fn entries(&self) -> Result<Vec<PathBuf>> {
        if !self.dir.is_dir() {
            return Ok(vec![]);
        }
        let mut out: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(is_entry_name)
                        .unwrap_or(false)
            })
            .collect();
        out.sort();
        out.reverse();
        Ok(out)
    }

    pub fn resolve(&self, wanted: &str) -> Result<PathBuf> {
        if wanted == "latest" {
            return self
                .entries()?
                .into_iter()
                .next()
                .context("dictate: history is empty");
        }
        if !wanted
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            bail!("dictate: history entry not found or prefix is ambiguous: {wanted}");
        }
        let direct = self.dir.join(wanted);
        if direct.is_dir() {
            return Ok(direct);
        }
        let matches: Vec<PathBuf> = self
            .entries()?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(wanted))
                    .unwrap_or(false)
            })
            .collect();
        if matches.len() == 1 {
            Ok(matches[0].clone())
        } else {
            bail!("dictate: history entry not found or prefix is ambiguous: {wanted}");
        }
    }

    fn read_opt(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|_| "unknown".into())
    }

    /// `history [n]` table. Matches bash column layout.
    pub fn list(&self, limit: usize) -> Result<String> {
        let mut out = String::new();
        for (n, entry) in self.entries()?.into_iter().enumerate() {
            if n >= limit {
                break;
            }
            let id = entry
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            let status = Self::read_opt(&entry.join("status"));
            let details = Self::read_opt(&entry.join("details"));
            let dur = details
                .lines()
                .find_map(|l| l.strip_prefix("duration_seconds="))
                .unwrap_or("unknown")
                .split('.')
                .next()
                .unwrap_or("unknown")
                .trim()
                .to_string();
            let text = fs::read_to_string(entry.join("transcript.txt"))
                .or_else(|_| fs::read_to_string(entry.join("transcript.raw.txt")))
                .unwrap_or_default()
                .replace(['\n', '\t'], " ");
            let short = if text.len() > 72 { format!("{}...", &text[..69]) } else { text };
            out.push_str(&format!(
                "{:<27}  {:<18}  {:>6}s  {}\n",
                id,
                status.trim(),
                dur,
                short
            ));
        }
        Ok(out)
    }

    /// `show [id]` dump.
    pub fn show(&self, wanted: &str) -> Result<String> {
        let entry = self.resolve(wanted).map_err(|_| {
            anyhow::anyhow!("dictate: history entry not found or prefix is ambiguous: {wanted}")
        })?;
        let mut out = String::new();
        out.push_str(&Self::read_opt(&entry.join("details")));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("status={}\n", Self::read_opt(&entry.join("status")).trim()));
        for (label, name) in [("audio", "audio.wav"), ("raw_audio", "audio.raw.pcm")] {
            if entry.join(name).exists() {
                out.push_str(&format!("{label}={}\n", entry.join(name).display()));
            }
        }
        let raw = fs::read_to_string(entry.join("transcript.raw.txt")).ok();
        let txt = fs::read_to_string(entry.join("transcript.txt")).ok();
        if let Some(t) = txt {
            out.push_str("\ntranscript:\n");
            out.push_str(&t);
        }
        if let Some(r) = raw {
            let same = fs::read(entry.join("transcript.txt"))
                .ok()
                .and_then(|t| fs::read(entry.join("transcript.raw.txt")).ok().map(|x| x == t))
                .unwrap_or(false);
            if !same {
                out.push_str("\nraw_transcript:\n");
                out.push_str(&r);
            }
        }
        Ok(out)
    }

    /// Latest entry containing a non-empty transcript (for bare `retry`).
    pub fn latest_with_transcript(&self) -> Result<PathBuf> {
        for e in self.entries()? {
            if fs::metadata(e.join("transcript.txt")).map(|m| m.len() > 0).unwrap_or(false) {
                return Ok(e);
            }
        }
        bail!("dictate: no saved transcript found; use 'dictate history' and 'dictate show ID'");
    }

    /// Archive `audio` as a new entry with `status`; returns entry path.
    /// Mirrors `history_archive_audio`: tmp dir + details + atomic rename,
    /// then prune. Returns `None` when disabled or audio missing/empty.
    /// `extra_details` lines are appended to the details file.
    pub fn archive_audio(
        &self,
        audio: &Path,
        status: &str,
        extra_details: &[(&str, &str)],
    ) -> Result<Option<PathBuf>> {
        if !self.enabled {
            return Ok(None);
        }
        if fs::metadata(audio).map(|m| m.len() == 0).unwrap_or(true) {
            return Ok(None);
        }
        let id = unique_id();
        let tmp = self.dir.join(format!(".{id}.tmp"));
        fs::create_dir_all(&tmp)?;
        fs::copy(audio, tmp.join("audio.wav"))?;
        fs::write(tmp.join("status"), format!("{status}\n"))?;
        // Bash key order: id, started (caller extra), saved, duration, ...
        // `saved`/`duration_seconds` are emitted here so every entry has
        // them even when a caller forgets.
        let mut details = format!("id={id}\n");
        for (k, v) in extra_details {
            details.push_str(&format!("{k}={v}\n"));
        }
        if !extra_details.iter().any(|(k, _)| *k == "saved") {
            details.push_str(&format!("saved={}\n", local_iso_now()));
        }
        if !extra_details.iter().any(|(k, _)| *k == "duration_seconds") {
            details.push_str(&format!("duration_seconds={}\n", wav_duration(&tmp.join("audio.wav"))));
        }
        fs::write(tmp.join("details"), details)?;
        let entry = self.dir.join(&id);
        fs::rename(&tmp, &entry)?;
        let _ = self.prune();
        Ok(Some(entry))
    }

    /// Archive a headerless raw capture (`history_archive_raw_failure`).
    /// Stored as `audio.raw.pcm` with `raw_format=` in details.
    pub fn archive_raw_audio(
        &self,
        raw: &Path,
        status: &str,
        extra_details: &[(&str, &str)],
    ) -> Result<Option<PathBuf>> {
        if !self.enabled {
            return Ok(None);
        }
        if fs::metadata(raw).map(|m| m.len() == 0).unwrap_or(true) {
            return Ok(None);
        }
        let id = unique_id();
        let tmp = self.dir.join(format!(".{id}.tmp"));
        fs::create_dir_all(&tmp)?;
        fs::copy(raw, tmp.join("audio.raw.pcm"))?;
        fs::write(tmp.join("status"), format!("{status}\n"))?;
        let mut details = format!("id={id}\n");
        for (k, v) in extra_details {
            details.push_str(&format!("{k}={v}\n"));
        }
        if !extra_details.iter().any(|(k, _)| *k == "saved") {
            details.push_str(&format!("saved={}\n", local_iso_now()));
        }
        fs::write(tmp.join("details"), details)?;
        let entry = self.dir.join(&id);
        fs::rename(&tmp, &entry)?;
        let _ = self.prune();
        Ok(Some(entry))
    }

    pub fn set_status(&self, entry: &Path, status: &str) {
        let _ = fs::write(entry.join("status"), format!("{status}\n"));
    }

    pub fn save_text(&self, entry: &Path, text: &str, name: &str) {
        let _ = fs::write(entry.join(name), format!("{text}\n"));
    }

    /// Enforce age + count bounds. Failures are non-fatal (bash warns).
    pub fn prune(&self) -> Result<()> {
        if !self.enabled || !self.dir.is_dir() {
            return Ok(());
        }
        if self.max_days > 0 {
            let cutoff = std::time::SystemTime::now()
                - std::time::Duration::from_secs(self.max_days * 86400);
            for e in self.entries()? {
                if let Ok(md) = fs::metadata(&e) {
                    if let Ok(mtime) = md.modified() {
                        if mtime < cutoff {
                            let _ = fs::remove_dir_all(&e);
                        }
                    }
                }
            }
        }
        if self.max_entries > 0 {
            let entries = self.entries()?;
            for old in entries.iter().skip(self.max_entries) {
                let _ = fs::remove_dir_all(old);
            }
        }
        Ok(())
    }
}

/// Bash entry glob `20??-??-??_??-??-??-*`: dated prefix, then anything
/// (pid, pid+counter). Tmp dirs (`.id.tmp`) never match the `20` prefix.
fn is_entry_name(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() >= 21
        && b.starts_with(b"20")
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'_'
        && b[13] == b'-'
        && b[16] == b'-'
        && b[19] == b'-'
        && b[..19]
            .iter()
            .enumerate()
            .filter(|(i, _)| ![4, 7, 10, 13, 16].contains(i))
            .all(|(_, c)| c.is_ascii_digit())
}

/// Local `date` output (bash uses the `date` binary everywhere for stamps).
/// Falls back to UTC computation when `date` is unavailable.
fn local_stamp() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d_%H-%M-%S")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| is_entry_name(&format!("{s}-x")))
        .unwrap_or_else(chrono_stamp)
}

/// Local `date --iso-8601=seconds` (bash `date` parity), UTC fallback.
/// Shared by history details and the recording start marker so `started=`
/// and `saved=` never disagree on zone.
pub fn local_iso_now() -> String {
    std::process::Command::new("date")
        .arg("--iso-8601=seconds")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.len() >= 20)
        .unwrap_or_else(|| format!("{}+00:00", chrono_iso_utc()))
}

/// Fine-tuning data so far, in seconds: (all history audio, the part with a
/// trusted label). Labelled = an ElevenLabs shadow or gold transcript, plus
/// read-aloud bench clips (`<bench>/<date session>/NN.wav` next to NN.txt).
/// Durations come from file sizes (16 kHz mono s16), so no audio is read.
pub fn training_audio(history: &Path, bench: &Path) -> (f64, f64) {
    let secs =
        |wav: &Path| fs::metadata(wav).map_or(0.0, |m| m.len().saturating_sub(44) as f64 / 32000.0);
    let dirs = |d: &Path| {
        fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
    };
    let (mut total, mut labelled) = (0.0, 0.0);
    for entry in dirs(history).filter(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_entry_name)
    }) {
        let s = secs(&entry.join("audio.wav"));
        total += s;
        if entry.join("transcript.elevenlabs.raw.txt").exists()
            || entry.join("transcript.gold.txt").exists()
        {
            labelled += s;
        }
    }
    // Only date-named sessions: lastmsg/, shadow35/ etc. are copies of history clips.
    for session in dirs(bench).filter(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("20"))
    }) {
        for wav in dirs(&session).filter(|p| {
            p.extension().is_some_and(|e| e == "wav") && p.with_extension("txt").exists()
        }) {
            let s = secs(&wav);
            total += s;
            labelled += s;
        }
    }
    (total, labelled)
}

/// `ffprobe` duration of an archived clip, or `unknown` (bash default when
/// `ffprobe` yields nothing).
fn wav_duration(wav: &Path) -> String {
    // Our own clips are read directly; ffprobe costs ~70 ms per dictation.
    if let Ok(samples) = crate::wav::read_mono16(wav) {
        return format!("{:.6}", samples.len() as f64 / crate::wav::RATE as f64);
    }
    std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(wav)
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_else(|| "unknown".into())
}

/// `date '+%Y-%m-%d_%H-%M-%S'` without a subprocess.
fn chrono_stamp() -> String {
    // Seconds since epoch -> civil date via libc gmtime. No `chrono` dep.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_stamp(secs)
}

/// Uniqueness within a second: pid alone collides when tests archive twice
/// in the same second, so each process appends a monotonic counter.
/// (Bash uses `DATE-PID`; the counter keeps the entry glob/sort compatible.)
fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", local_stamp(), std::process::id(), n)
}

/// UTC `YYYY-MM-DDTHH:MM:SS` fallback when `date` is missing.
fn chrono_iso_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let date = civil_ymd(days);
    format!("{date}T{:02}:{:02}:{:02}", sod / 3600, (sod % 3600) / 60, sod % 60)
}

fn civil_stamp(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86400);
    let secs_of_day = unix_secs.rem_euclid(86400);
    format!(
        "{}_{:02}-{:02}-{:02}",
        civil_ymd(days),
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn civil_ymd(days: i64) -> String {
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 }.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("history");
        fs::create_dir_all(dir.join("2026-09-10_10-00-00-1")).unwrap();
        fs::create_dir_all(dir.join("2026-09-10_11-00-00-2")).unwrap();
        fs::write(dir.join("2026-09-10_10-00-00-1/status"), "ready\n").unwrap();
        fs::write(dir.join("2026-09-10_10-00-00-1/details"), "duration_seconds=1.2\n").unwrap();
        fs::write(dir.join("2026-09-10_10-00-00-1/transcript.txt"), "hello\n").unwrap();
        fs::write(dir.join("2026-09-10_11-00-00-2/status"), "ready\n").unwrap();
        fs::write(dir.join("2026-09-10_11-00-00-2/details"), "duration_seconds=2.5\n").unwrap();
        fs::write(dir.join("2026-09-10_11-00-00-2/transcript.txt"), "world\n").unwrap();
        let s = HistoryStore { dir: dir.clone(), enabled: true, max_days: 30, max_entries: 200 };
        (tmp, s)
    }

    #[test]
    fn training_audio_counts_labelled_history_and_bench() {
        let (t, s) = store();
        let wav =
            |p: &Path, secs: u64| fs::write(p, vec![0u8; 44 + 32000 * secs as usize]).unwrap();
        wav(&s.dir.join("2026-09-10_10-00-00-1/audio.wav"), 2);
        wav(&s.dir.join("2026-09-10_11-00-00-2/audio.wav"), 3);
        fs::write(
            s.dir
                .join("2026-09-10_11-00-00-2/transcript.elevenlabs.raw.txt"),
            "x\n",
        )
        .unwrap();
        let bench = t.path().join("bench");
        for d in ["2026-10-22_readaloud2", "lastmsg"] {
            fs::create_dir_all(bench.join(d)).unwrap();
            wav(&bench.join(d).join("01.wav"), 5);
            fs::write(bench.join(d).join("01.txt"), "ref\n").unwrap();
        }
        wav(&bench.join("2026-10-22_readaloud2/02.wav"), 7); // no reference yet
        let (total, labelled) = training_audio(&s.dir, &bench);
        assert!((total - 10.0).abs() < 1e-9, "{total}");
        assert!((labelled - 8.0).abs() < 1e-9, "{labelled}");
    }

    #[test]
    fn lists_newest_first_and_resolves() {
        let (_t, s) = store();
        let list = s.list(20).unwrap();
        assert!(list.find("11-00-00-2").unwrap() < list.find("10-00-00-1").unwrap());
        let latest = s.resolve("latest").unwrap();
        assert_eq!(
            latest.file_name().and_then(|n| n.to_str()),
            Some("2026-09-10_11-00-00-2")
        );
        assert!(s.resolve("2026-09-10_10-00-00-1").is_ok());
    }

    #[test]
    fn ambiguous_prefix_fails() {
        let (_t, s) = store();
        assert!(s.resolve("2026-09-10").is_err());
    }

    #[test]
    fn civil_stamp_known_date() {
        // 2026-09-10 12:00:00 UTC = 1789041600.
        assert_eq!(civil_stamp(1789041600), "2026-09-10_12-00-00");
        assert_eq!(civil_stamp(0), "1970-01-01_00-00-00");
    }

    #[test]
    fn archive_roundtrip_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("h");
        std::fs::create_dir_all(&dir).unwrap();
        let s = HistoryStore { dir: dir.clone(), enabled: true, max_days: 30, max_entries: 1 };
        let wav = tmp.path().join("a.wav");
        std::fs::write(&wav, b"RIFFfake").unwrap();
        let e1 = s.archive_audio(&wav, "captured", &[("engine", "whisper")]).unwrap().unwrap();
        assert!(e1.join("audio.wav").exists());
        s.set_status(&e1, "ready");
        s.save_text(&e1, "hello", "transcript.txt");
        assert_eq!(std::fs::read_to_string(e1.join("status")).unwrap(), "ready\n");
        // Second entry exceeds max_entries=1 -> oldest pruned.
        let e2 = s.archive_audio(&wav, "captured", &[]).unwrap().unwrap();
        assert!(e2.exists());
        assert!(!e1.exists(), "oldest entry should be pruned");
        // Disabled store archives nothing.
        let off = HistoryStore { dir: dir.clone(), enabled: false, max_days: 30, max_entries: 200 };
        assert!(off.archive_audio(&wav, "captured", &[]).unwrap().is_none());
    }
}
