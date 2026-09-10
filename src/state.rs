//! Recording lifecycle state: pidfile + mode + start marker.
//!
//! Ports the `start()` idempotency guard, `reap_recorders`, and `MODEFILE`
//! handling. Improvement over bash: `claim()` uses atomic `O_CREAT|O_EXCL`
//! creation so two concurrent `start` calls (key autorepeat, `toggle`)
//! cannot both win; stale pidfiles (dead pid) are reaped and retried once.
//! Full `flock(2)` around the whole start/stop critical section is phase 2
//! (needs a lock crate or raw `libc`; std alone has no `flock`).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RecordingState {
    pub pidfile: PathBuf,
    pub startfile: PathBuf,
    pub modefile: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// We created the pidfile; caller owns the recording.
    Claimed,
    /// A live recorder already holds it; caller must be a no-op (autorepeat).
    AlreadyRecording,
}

impl RecordingState {
    /// True when a live recorder pid is recorded. Stale pidfiles (dead pid)
    /// report false so a new `start` can proceed after cleanup.
    pub fn is_recording(&self) -> bool {
        read_live_pid(&self.pidfile).is_some()
    }

    /// Atomically claim the pidfile for `owner_pid`.
    /// - Missing file: create exclusively -> `Claimed`.
    /// - Live pid inside: -> `AlreadyRecording` (idempotent, like bash).
    /// - Stale/unparsable: remove and retry once -> `Claimed`, else propagate.
    pub fn claim(&self, owner_pid: u32) -> Result<Claim> {
        match create_exclusive(&self.pidfile, format!("{owner_pid}\n").as_bytes()) {
            Ok(()) => Ok(Claim::Claimed),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_live_pid(&self.pidfile).is_some() {
                    Ok(Claim::AlreadyRecording)
                } else {
                    let _ = std::fs::remove_file(&self.pidfile);
                    create_exclusive(&self.pidfile, format!("{owner_pid}\n").as_bytes())
                        .with_context(|| {
                            format!("dictate: cannot write {}", self.pidfile.display())
                        })?;
                    Ok(Claim::Claimed)
                }
            }
            Err(e) => Err(e).with_context(|| {
                format!("dictate: cannot write {}", self.pidfile.display())
            }),
        }
    }

    /// Release our claim. Only removes the pidfile when it holds `owner_pid`,
    /// so `stop` never deletes a *new* recording's pidfile.
    pub fn release_if_owner(&self, owner_pid: u32) -> Result<Option<u32>> {
        let content = std::fs::read_to_string(&self.pidfile).unwrap_or_default();
        let recorded: Option<u32> = content.trim().parse().ok();
        if recorded == Some(owner_pid) || content.trim().is_empty() {
            let _ = std::fs::remove_file(&self.pidfile);
        }
        // Return the recorded pid for the caller to signal (even if we did
        // not own it, `stop` still signals the recorded pid like bash).
        Ok(recorded)
    }

    /// Overwrite the pidfile with the spawned recorder pid (bash `start`
    /// writes `$!` after claiming; the claim placeholder only won the race).
    pub fn set_pid(&self, pid: u32) -> Result<()> {
        std::fs::write(&self.pidfile, format!("{pid}\n"))
            .with_context(|| format!("dictate: cannot write {}", self.pidfile.display()))
    }

    /// Read, remove, and return the recorded pid (bash `stop` semantics:
    /// any `stop` — not just the owner — stops the recording).
    pub fn take_recorded_pid(&self) -> Option<u32> {
        let content = std::fs::read_to_string(&self.pidfile).ok()?;
        let _ = std::fs::remove_file(&self.pidfile);
        content.trim().parse().ok()
    }

    pub fn read_mode(&self) -> String {
        std::fs::read_to_string(&self.modefile)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "fast".into())
    }

    pub fn write_mode(&self, mode: &str) -> Result<()> {
        std::fs::write(&self.modefile, format!("{mode}\n"))
            .with_context(|| format!("dictate: cannot write {}", self.modefile.display()))
    }

    /// One-shot consume: read mode, then remove (bash `stop()` semantics).
    pub fn take_mode(&self) -> String {
        let mode = self.read_mode();
        let _ = std::fs::remove_file(&self.modefile);
        mode
    }

    pub fn clear(&self) -> Result<()> {
        for p in [&self.pidfile, &self.modefile] {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }
}

fn create_exclusive(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(contents)?;
    Ok(())
}

/// Parse the pidfile and return the pid only when that process is alive.
pub fn read_live_pid(pidfile: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pidfile).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    if pid <= 0 {
        return None;
    }
    if unsafe { libc_kill0(pid) } {
        Some(pid as u32)
    } else {
        None
    }
}

/// `kill(pid, 0)` liveness probe without a dependency.
#[cfg(unix)]
unsafe fn libc_kill0(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
unsafe fn libc_kill0(_pid: i32) -> bool {
    false
}

/// Recorder-match pattern (mirrors `reap_recorders` in bash): any process
/// whose command line mentions the raw capture filename.
pub const RECORDER_PATTERN: &str = "dictate-raw.pcm";

/// Send SIGINT (bash `kill -INT`; both backends only finalise on SIGINT).
/// Returns false when the process is already gone or signaling failed.
pub fn send_int(pid: u32) -> bool {
    unsafe { libc_signal(pid as i32, 2) }
}

/// Send SIGTERM.
pub fn terminate_pid(pid: u32) -> bool {
    unsafe { libc_signal(pid as i32, 15) }
}

/// Wait up to `tenths` x 100ms for `pid` to exit. True when gone.
pub fn wait_exit(pid: u32, tenths: u32) -> bool {
    for _ in 0..tenths {
        if !unsafe { libc_kill0(pid as i32) } {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !unsafe { libc_kill0(pid as i32) }
}

#[cfg(unix)]
unsafe fn libc_signal(pid: i32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) == 0 }
}

#[cfg(not(unix))]
unsafe fn libc_signal(_pid: i32, _sig: i32) -> bool {
    false
}

/// Pids of stray recorders owned by `uid`, via `pgrep -u uid -f pattern`.
/// Empty vec when none (the common case) — never an error.
pub fn stray_recorder_pids(uid: &str, pattern: &str) -> Vec<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-u", uid, "-f", pattern])
        .output();
    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_in(tmp: &Path) -> RecordingState {
        RecordingState {
            pidfile: tmp.join("dictate.pid"),
            startfile: tmp.join("dictate.started"),
            modefile: tmp.join("dictate.mode"),
        }
    }

    #[test]
    fn claim_is_exclusive_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state_in(tmp.path());
        let me = std::process::id();
        assert_eq!(s.claim(me).unwrap(), Claim::Claimed);
        assert!(s.is_recording(), "own live pid must report recording");
        // Any further claim while the owner lives -> AlreadyRecording.
        assert_eq!(s.claim(me).unwrap(), Claim::AlreadyRecording);
        assert_eq!(s.claim(me + 1).unwrap(), Claim::AlreadyRecording);
    }

    #[test]
    fn stale_pidfile_is_reaped_and_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state_in(tmp.path());
        // 2^31-1 is virtually never a live pid; definitely not ours.
        std::fs::write(&s.pidfile, "2147483647\n").unwrap();
        assert!(!s.is_recording());
        assert_eq!(s.claim(4321).unwrap(), Claim::Claimed);
        assert_eq!(std::fs::read_to_string(&s.pidfile).unwrap(), "4321\n");
    }

    #[test]
    fn unparsable_pidfile_is_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state_in(tmp.path());
        std::fs::write(&s.pidfile, "not-a-pid\n").unwrap();
        assert_eq!(s.claim(99).unwrap(), Claim::Claimed);
    }

    #[test]
    fn release_only_removes_own_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state_in(tmp.path());
        std::fs::write(&s.pidfile, "555\n").unwrap();
        s.release_if_owner(999).unwrap();
        assert!(s.pidfile.exists(), "must not delete another recording");
        std::fs::write(&s.pidfile, "999\n").unwrap();
        s.release_if_owner(999).unwrap();
        assert!(!s.pidfile.exists());
    }

    #[test]
    fn mode_roundtrip_and_take() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state_in(tmp.path());
        assert_eq!(s.read_mode(), "fast");
        s.write_mode("llm").unwrap();
        assert_eq!(s.read_mode(), "llm");
        assert_eq!(s.take_mode(), "llm");
        assert_eq!(s.read_mode(), "fast");
    }

    #[test]
    fn no_strays_in_clean_env() {
        let pids = stray_recorder_pids("4294967295", "definitely-not-dictate-xyz");
        assert!(pids.is_empty());
    }
}
