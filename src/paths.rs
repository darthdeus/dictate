//! Scratch/state/history directory layout.
//!
//! Ports the `RUNDIR / DATADIR / STATE_DIR / HISTORY_DIR` block in
//! `bin/dictate`. Split is deliberate: pidfiles live on tmpfs (`RUNDIR`,
//! must not survive reboot), audio lives on real disk (`DATADIR`, unbounded
//! recorder must not exhaust the runtime fs).

use anyhow::Result;
use std::path::PathBuf;

use crate::config::Env;

/// Process-wide `umask 077`, mirroring `umask 077` at the top of
/// `bin/dictate` / `bin/dictate-overlay`: every file created afterwards
/// (pidfiles, state.json, history payloads, restore temp files) is
/// owner-only even where an explicit chmod was forgotten. Call once at
/// process start, before any file creation.
pub fn set_private_umask() {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn umask(mask: u32) -> u32;
        }
        unsafe {
            umask(0o077);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub rundir: PathBuf,
    pub datadir: PathBuf,
    pub state_dir: PathBuf,
    pub history_dir: PathBuf,
    pub history_enabled: bool,
    pub pidfile: PathBuf,
    pub startfile: PathBuf,
    pub wav: PathBuf,
    pub wav_proc: PathBuf,
    pub raw: PathBuf,
    pub log: PathBuf,
    pub release_timing: PathBuf,
    pub preview_timing: PathBuf,
    pub preview_pidfile: PathBuf,
    pub modefile: PathBuf,
}

impl Paths {
    pub fn resolve(env: &Env) -> Result<Self> {
        let rundir = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let datadir = env
            .get("DICTATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var("XDG_CACHE_HOME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".cache"))
                    })
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("dictate")
            });
        let state_dir = std::env::var("XDG_STATE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("dictate");
        let history_enabled = env.get("DICTATE_HISTORY").unwrap_or("1") == "1";
        let history_dir = env
            .get("DICTATE_HISTORY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("history"));

        Ok(Self {
            pidfile: rundir.join("dictate.pid"),
            startfile: rundir.join("dictate.started"),
            preview_pidfile: rundir.join("dictate-preview.pid"),
            modefile: rundir.join("dictate.mode"),
            wav: datadir.join("dictate.wav"),
            wav_proc: datadir.join("dictate-proc.wav"),
            raw: datadir.join("dictate-raw.pcm"),
            release_timing: datadir.join("dictate-release-timing.tsv"),
            preview_timing: datadir.join("dictate-preview-timing.tsv"),
            log: state_dir.join("dictate.log"),
            rundir,
            datadir,
            state_dir,
            history_dir,
            history_enabled,
        })
    }

    /// Create datadir/state/history with 0700, mirroring `mkdir -p + chmod`.
    pub fn ensure_private_dirs(&self) -> Result<bool> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [&self.datadir, &self.state_dir] {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if self.history_enabled {
            match std::fs::create_dir_all(&self.history_dir) {
                Ok(()) => {
                    std::fs::set_permissions(
                        &self.history_dir,
                        std::fs::Permissions::from_mode(0o700),
                    )?;
                    return Ok(true);
                }
                Err(_) => return Ok(false),
            }
        }
        Ok(self.history_enabled)
    }
}
