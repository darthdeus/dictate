//! dictate — native implementation sharing `bin/dictate`'s state directories,
//! config keys, overlay protocol, and history/timing formats.
//!
//! External backends (`pw-record`/`ffmpeg`, `whisper-cli`/`whisper-server`,
//! `xdotool`, `xclip`/`copyq`) are invoked as subprocesses. Linux/X11
//! specifics live behind traits in `audio`, `overlay`, `delivery`, and
//! `transcribe` so other platforms can be added without touching core.

pub mod audio;
pub mod config;
pub mod delivery;
pub mod doctor;
pub mod history;
pub mod overlay;
pub mod paths;
pub mod pipeline;
pub mod preview;
pub mod state;
pub mod text;
pub mod timings;
pub mod transcribe;

/// Serializes tests that shell out to helper binaries (fake-script exec,
/// `grep -E` matching). Under parallel load, back-to-back exec in this
/// environment flakes intermittently; pure-logic tests stay parallel.
#[cfg(test)]
pub(crate) static FAKE_EXEC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
