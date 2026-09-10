//! dictate — Rust port (strangler phase).
//!
//! Phase 1: native core (config, text, history, timings, state) + process
//! calls to external backends (ffmpeg, whisper, xdotool). The legacy
//! `bin/dictate` bash script remains the reference implementation until each
//! command is verified against it.
//!
//! Platform seam: Linux/X11 specifics live behind traits in
//! `audio`, `overlay`, and `delivery` so Wayland/macOS can be added later
//! without touching core.

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
