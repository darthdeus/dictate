//! Command orchestration: faithful port of `start/stop/toggle/demo/doctor/
//! level/meter/serve/scan/bench/retry` in `bin/dictate`.
//!
//! Each heavy step delegates to a focused module (config/paths/state/audio/
//! transcribe/delivery/overlay/history/text/timings); this module only
//! sequences them in bash order, preserving these semantics:
//! - `start` is idempotent (autorepeat) via atomic pidfile claim.
//! - `stop` without pidfile still sweeps strays and salvages audio.
//! - history is written *before* transcription/delivery so crashes recover.
//! - overlay notifications never fail the pipeline (`notify` returns `()`).
//! - clipboard restore is best-effort background with still-holds-text guard.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::audio::{self, AudioBackend, ProcessAudio, RawFormat};
use crate::config::Config;
use crate::correct::Corrector;
use crate::daemon;
use crate::delivery::{self, DeliveryAction, InputMode, OwnerWorker, PasteEngine, ProcessClipboard, Xdotool};
use crate::x11::X;
use crate::history::HistoryStore;
use crate::overlay::{map_notify, JsonFileOverlay, OverlayBackend, OverlayMessage};
use crate::paths::Paths;
use crate::state::{self, Claim, RecordingState};
use crate::text;
use crate::transcribe::{self, Engine, ParakeetConfig, WhisperConfig};
use crate::wav;

/// Everything a command needs, built once in `main`.
pub struct Ctx {
    pub cfg: Config,
    pub paths: Paths,
    pub state: RecordingState,
    pub history: HistoryStore,
    pub overlay_path: PathBuf,
    pub log: PathBuf,
    pub command_started: f64,
    /// Release-timing TSV is written only inside `stop` (bash
    /// `RELEASE_TIMING_ACTIVE`); `retry`/`deliver` otherwise stay silent.
    pub timing_active: std::cell::Cell<bool>,
    /// Set by an incremental `stop`: the daemon's pause segments used and
    /// the seconds of audio after them recognized at stop (0: none).
    pub tail: std::cell::Cell<Option<(usize, f64)>>,
}

impl Ctx {
    pub fn new(cfg: Config, paths: Paths, history_ready: bool) -> Result<Self> {
        // `ensure_private_dirs` reports whether history storage is usable;
        // a mkdir failure degrades to disabled (bash `HISTORY_READY=0`),
        // never to aborting the dictation.
        let history = HistoryStore::new(
            paths.history_dir.clone(),
            paths.history_enabled && history_ready,
            cfg.get("DICTATE_HISTORY_DAYS").unwrap_or("30").parse().unwrap_or(30),
            cfg.get("DICTATE_HISTORY_MAX").unwrap_or("200").parse().unwrap_or(200),
        )?;
        Ok(Self {
            state: RecordingState {
                pidfile: paths.pidfile.clone(),
                startfile: paths.startfile.clone(),
                modefile: paths.modefile.clone(),
            },
            overlay_path: overlay_state_path(&paths.rundir),
            log: paths.log.clone(),
            cfg,
            paths,
            history,
            command_started: now_unix(),
            timing_active: std::cell::Cell::new(false),
            tail: std::cell::Cell::new(None),
        })
    }

    /// Bash `release_mark`: appends `event<TAB>timestamp`, but only while
    /// this process is a `stop` with an active TSV.
    pub fn mark(&self, event: &str) {
        if self.timing_active.get() {
            release_mark(&self.paths, event);
        }
    }

    pub fn mark_at(&self, event: &str, ts: f64) {
        if self.timing_active.get() {
            release_mark_at(&self.paths, event, ts);
        }
    }

    /// TSV path for stages owned by other modules (paste shortcut).
    /// `None` outside `stop`, where no TSV is active.
    pub fn timing_tsv(&self) -> Option<&Path> {
        self.timing_active.get().then_some(self.paths.release_timing.as_path())
    }

    pub(crate) fn c(&self, key: &str, default: &str) -> String {
        self.cfg.get_or(key, default)
    }
    pub(crate) fn mic(&self) -> String {
        self.c("DICTATE_MIC", "auto")
    }
    fn pulse_mic(&self) -> String {
        let m = self.mic();
        if m == "auto" { "@DEFAULT_SOURCE@".into() } else { m }
    }
    fn engine(&self) -> Engine {
        Engine::parse(&self.c("DICTATE_ENGINE", "whisper"))
    }
    pub(crate) fn channels(&self) -> u32 {
        self.c("DICTATE_CHANNELS", "1").parse().unwrap_or(1).max(1)
    }
    pub(crate) fn chan(&self) -> u32 {
        self.c("DICTATE_CHAN", "0").parse().unwrap_or(0)
    }
    pub(crate) fn max_secs(&self) -> u64 {
        self.c("DICTATE_MAX_SECS", "300").parse().unwrap_or(300)
    }
    fn model(&self) -> String {
        self.c("DICTATE_MODEL", &default_model_file("ggml-large-v3-turbo.bin"))
    }
    fn parakeet_model(&self) -> String {
        self.c("DICTATE_PARAKEET_MODEL", &default_model_file("ggml-parakeet-tdt-0.6b-v3-f16.bin"))
    }
    fn vad_model(&self) -> String {
        self.c("DICTATE_VAD_MODEL", &default_model_file("ggml-silero-v5.1.2.bin"))
    }
    /// Glossary for restyle and correction. Separate from the whisper
    /// prompt, which reads best as an example sentence.
    fn terms(&self) -> String {
        self.cfg.get("DICTATE_TERMS").map(str::to_string).unwrap_or_else(|| self.c("DICTATE_PROMPT", ""))
    }
    /// What produces the final text, shown on the overlay.
    pub fn state_dir(&self) -> PathBuf {
        self.log.parent().map(Path::to_path_buf).unwrap_or_default()
    }
    pub(crate) fn final_label(&self) -> String {
        let mut label = if self.c("DICTATE_FINAL", "whisper") == "elevenlabs" {
            format!("Rust \u{b7} final: elevenlabs {}", self.c("DICTATE_ELEVENLABS_MODEL", crate::elevenlabs::DEFAULT_MODEL))
        } else {
            let whisper = Path::new(&self.model()).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("Rust \u{b7} final: whisper {}", whisper.trim_start_matches("ggml-"))
        };
        if self.c("DICTATE_CORRECT", "0") == "1" {
            let model = self.c("DICTATE_CORRECT_MODEL", crate::correct::DEFAULT_MODEL);
            label += &format!(" + {}", model.rsplit('/').next().unwrap_or(&model));
        }
        label
    }
    /// `DICTATE_SEGMENT_MIN_MS` in samples (see `segment::MIN_SEGMENT_MS`).
    /// ElevenLabs is billed per second sent and takes no text context, so
    /// with it every pause cut is kept by default and each second is sent once.
    pub(crate) fn min_segment(&self) -> usize {
        let default = if self.c("DICTATE_FINAL", "whisper") == "elevenlabs" { "0" } else { crate::segment::MIN_SEGMENT_MS };
        self.c("DICTATE_SEGMENT_MIN_MS", default).parse::<usize>().unwrap_or(default.parse().unwrap()) * wav::RATE as usize / 1000
    }
    fn server(&self) -> String {
        // Explicit empty forces CLI; unset means default URL.
        self.cfg.get("DICTATE_SERVER").unwrap_or("http://127.0.0.1:8127").to_string()
    }
    fn audio(&self) -> ProcessAudio {
        ProcessAudio {
            pw_record_bin: "pw-record".into(),
            dictate_pw: self.c("DICTATE_PW", "1"),
            ffmpeg_bin: "ffmpeg".into(),
            timeout_bin: "timeout".into(),
            pulse_mic: self.pulse_mic(),
        }
    }
    fn xdotool(&self) -> Xdotool {
        Xdotool { bin: "xdotool".into() }
    }
    fn clipboard(&self) -> ProcessClipboard {
        ProcessClipboard::new()
    }
}

pub fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Default model locations (bash `MODEL`/`PARAKEET_MODEL`/`VAD_MODEL`
/// defaults, shared by stop/doctor/bench so all three agree).
fn default_model_file(name: &str) -> String {
    format!("{}/.local/share/whisper/{name}", std::env::var("HOME").unwrap_or_default())
}

fn uid_string() -> String {
    std::env::var("UID").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        std::process::Command::new("id")
            .args(["-u"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "0".into())
    })
}

fn overlay_state_path(rundir: &Path) -> PathBuf {
    rundir.join(format!("dictate-overlay-{}", uid_string())).join("state.json")
}

pub(crate) fn append_log(log: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

// ---------------------------------------------------------------- notify

/// Never-failing overlay/dashboard notification (bash `notify` returns 0).
/// Writes state.json atomically and spawns the singleton renderer detached.
///
/// GTK only: `DICTATE_OSD_BACKEND=feh` is honored by `bin/dictate` alone
/// (see README port notes). Under `feh` the state.json is still published
/// for protocol compatibility, but no GTK renderer is spawned.
pub fn notify(ctx: &Ctx, title: &str, body: &str, timeout_ms: u64) {
    let (phase, label, text) = map_notify(title, body);
    let msg = OverlayMessage {
        phase,
        label,
        text,
        tentative: String::new(),
        stats: String::new(),
        engine: match phase {
            crate::overlay::Phase::Recording => "Rust \u{b7} live: whisper snapshots".to_string(),
            _ => ctx.final_label(),
        },
        audio: ctx.paths.raw.display().to_string(),
        channels: ctx.channels(),
        channel: ctx.chan(),
        duration: timeout_ms as f64 / 1000.0,
        expires: now_unix() + timeout_ms as f64 / 1000.0,
    };
    let writer = JsonFileOverlay { state_path: ctx.overlay_path.clone() };
    if writer.show(&msg).is_err() {
        return;
    }
    if ctx.cfg.get("DICTATE_OSD_BACKEND").unwrap_or("gtk") == "feh" {
        return;
    }
    spawn_renderer(ctx);
}

fn renderer_binary() -> String {
    if let Ok(b) = std::env::var("DICTATE_OVERLAY_BIN") {
        if !b.is_empty() {
            return b;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Cargo layout: <root>/target/debug/dictate -> <root>/bin/dictate-overlay
            for cand in [
                dir.join("dictate-overlay"),
                dir.join("../../bin/dictate-overlay"),
            ] {
                if cand.is_file() {
                    return cand.display().to_string();
                }
            }
        }
    }
    "dictate-overlay".into()
}

fn spawn_renderer(ctx: &Ctx) {
    let logfile = std::fs::OpenOptions::new().create(true).append(true).open(&ctx.log).ok();
    let null = std::fs::OpenOptions::new().read(true).open("/dev/null").ok();
    let mut cmd = std::process::Command::new(renderer_binary());
    cmd.envs(ctx.cfg.osd_vars());
    if let Some(n) = null {
        cmd.stdin(n);
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    match logfile {
        // A fd that cannot be cloned is not worth failing (or panicking)
        // a notification over: fall back to a null output.
        Some(f) => match f.try_clone() {
            Ok(out) => {
                let _ = cmd.stdout(out).stderr(f).spawn();
            }
            Err(_) => {
                let _ = cmd
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        },
        None => {
            let _ = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
        }
    }
}

// ------------------------------------------------------------ release marks

fn release_mark(paths: &Paths, event: &str) {
    release_mark_at(paths, event, now_unix())
}

fn release_mark_at(paths: &Paths, event: &str, ts: f64) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&paths.release_timing) {
        let _ = writeln!(f, "{event}\t{ts:.6}");
    }
}

// ------------------------------------------------------------------ preview

pub fn stop_preview(paths: &Paths) {
    let content = std::fs::read_to_string(&paths.preview_pidfile).unwrap_or_default();
    if let Ok(pid) = content.trim().parse::<u32>() {
        state::terminate_pid(pid);
    }
    let _ = std::fs::remove_file(&paths.preview_pidfile);
}

// ------------------------------------------------------------------ reaping

/// Kill strays matching the raw-capture pattern (bash `reap_recorders`).
pub fn reap_recorders() {
    let uid = uid_string();
    let pids = state::stray_recorder_pids(&uid, state::RECORDER_PATTERN);
    if pids.is_empty() {
        return;
    }
    let _ = std::process::Command::new("pkill")
        .args(["-INT", "-u", &uid, "-f", state::RECORDER_PATTERN])
        .status();
    for _ in 0..20 {
        if state::stray_recorder_pids(&uid, state::RECORDER_PATTERN).is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::process::Command::new("pkill")
        .args(["-KILL", "-u", &uid, "-f", state::RECORDER_PATTERN])
        .status();
}

// ------------------------------------------------------------------- start

/// `dictate start [llm]`. Idempotent: second call while recording is a no-op.
pub fn cmd_start(ctx: &Ctx, llm_mode: bool) -> Result<()> {
    ctx.state.write_mode(if llm_mode { "llm" } else { "fast" })?;
    // A running daemon owns capture and the streaming preview; it publishes
    // to the overlay itself, so only the renderer needs starting here.
    match daemon::request(&ctx.paths, "start").as_deref() {
        Some("ok") => {
            std::fs::write(&ctx.paths.startfile, crate::history::local_iso_now())?;
            if ctx.cfg.get("DICTATE_OSD_BACKEND").unwrap_or("gtk") != "feh" {
                spawn_renderer(ctx);
            }
            return Ok(());
        }
        Some("busy") => return Ok(()),
        Some(err) => append_log(&ctx.log, &format!("dictate daemon: {err}; recording without it")),
        None => {}
    }
    match ctx.state.claim(std::process::id())? {
        Claim::AlreadyRecording => return Ok(()),
        Claim::Claimed => {}
    }
    if !Path::new(&ctx.model()).exists() && ctx.engine() != Engine::Parakeet {
        notify(ctx, "dictate: model missing",
            &format!("{} — run: dictate doctor", ctx.model()), 5000);
        let _ = std::fs::remove_file(&ctx.paths.pidfile);
        anyhow::bail!("dictate: model missing: {}", ctx.model());
    }
    stop_preview(&ctx.paths);
    // Reap BEFORE unlinking: an orphan holding $RAW keeps writing to the
    // unlinked inode invisibly. Recorder must stop before reclaim.
    reap_recorders();
    if recover_abandoned(ctx).is_err() {
        notify(ctx, "dictate: cannot preserve previous capture",
            &format!("see {}; recording not started", ctx.log.display()), 5000);
        let _ = std::fs::remove_file(&ctx.paths.pidfile);
        anyhow::bail!("dictate: cannot preserve previous capture");
    }
    let _ = std::fs::remove_file(&ctx.paths.raw);
    std::fs::write(&ctx.paths.startfile, crate::history::local_iso_now())?;
    let rec_pid = ctx.audio().start_capture(
        &ctx.paths.raw,
        RawFormat { channels: ctx.channels(), rate: 48000 },
        &ctx.mic(),
        ctx.max_secs(),
    )?;
    ctx.state.set_pid(rec_pid)?;
    notify(ctx, "\u{1f3a4} recording\u{2026}", "", ctx.max_secs() * 1000);

    // Preview needs the resident server; whisper-cli per tick would reload
    // the model every tick and fall behind.
    if ctx.c("DICTATE_STREAM", "1") == "1" && server_up(&ctx.server()) {
        spawn_preview(ctx);
    }

    // Catch instant failures (mic unplugged, bad source name).
    std::thread::sleep(std::time::Duration::from_millis(300));
    if state::read_live_pid(&ctx.paths.pidfile).is_none() {
        let tail = tail_file(&ctx.log, 2);
        let _ = std::fs::remove_file(&ctx.paths.pidfile);
        notify(ctx, "dictate: recorder failed to start", &tail, 5000);
        anyhow::bail!("dictate: recorder failed to start");
    }
    Ok(())
}

fn tail_file(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().rev().take(n).collect::<Vec<_>>().into_iter().rev()
        .collect::<Vec<_>>().join("\n")
}

/// Salvage a complete raw capture left by a killed stop (bash
/// `history_recover_abandoned`). Returns Err only when archival failed AND
/// history left us holding an unarchived recovery file.
fn recover_abandoned(ctx: &Ctx) -> Result<()> {
    if std::fs::metadata(&ctx.paths.raw).map(|m| m.len() == 0).unwrap_or(true) {
        return Ok(());
    }
    let recovered = ctx.paths.datadir.join(format!("dictate-recovered-{}.wav", std::process::id()));
    let converted = ctx.audio().mono16_with_channels(&ctx.paths.raw, &recovered, ctx.chan(), ctx.channels(), &ctx.log).is_ok()
        && std::fs::metadata(&recovered).map(|m| m.len() > 0).unwrap_or(false);
    // History I/O is best-effort (bash warns and continues with
    // `HISTORY_ENTRY=""`); a history-dir problem must not abort the capture.
    let warn = |e: anyhow::Error| append_log(&ctx.log, &format!("warning: could not archive dictation audio: {e:#}"));
    let entry = if converted {
        match ctx.history.archive_audio(&recovered, "recovered-after-crash",
            &[("engine", &ctx.c("DICTATE_ENGINE", "whisper")),
              ("mic", &ctx.mic()),
              ("channel", &format!("{}/{}", ctx.chan(), ctx.channels()))]) {
            Ok(e) => e,
            Err(e) => { warn(e); None }
        }
    } else {
        let mic = ctx.mic();
        let channel = format!("{}/{}", ctx.chan(), ctx.channels());
        let raw_format = format!("s16le,48000Hz,{}ch", ctx.channels());
        match ctx.history.archive_raw_audio(
            &ctx.paths.raw, "raw-recovered-after-crash",
            &[("mic", mic.as_str()), ("channel", channel.as_str()), ("raw_format", raw_format.as_str())]) {
            Ok(e) => e,
            Err(e) => { warn(e); None }
        }
    };
    if ctx.history.enabled && entry.is_none() {
        // Archival failed: move aside before the new capture takes the name.
        let stamp = file_stamp();
        if converted {
            let fb = ctx.paths.datadir.join(format!("dictate-recovered-{stamp}-{}", std::process::id())).with_extension("wav");
            std::fs::rename(&recovered, &fb)?;
            let _ = std::fs::remove_file(&ctx.paths.raw);
            let _ = std::fs::remove_file(&ctx.paths.startfile);
            append_log(&ctx.log, &format!("warning: history unavailable; recovered audio kept at {}", fb.display()));
        } else {
            let fb = ctx.paths.datadir.join(format!("dictate-recovered-{stamp}-{}.raw.pcm", std::process::id()));
            std::fs::rename(&ctx.paths.raw, &fb)?;
            let _ = std::fs::remove_file(&ctx.paths.startfile);
            append_log(&ctx.log, &format!("warning: history unavailable; recovered raw audio kept at {}", fb.display()));
        }
        return Ok(());
    }
    let _ = std::fs::remove_file(&recovered);
    let _ = std::fs::remove_file(&ctx.paths.raw);
    let _ = std::fs::remove_file(&ctx.paths.startfile);
    Ok(())
}

fn file_stamp() -> String {
    let t = now_unix() as i64;
    format!("{t}")
}

/// Liveness only: any HTTP response counts (bash `server_up` — no `-f`,
/// since bare GET on /inference legitimately 4xxs on a healthy server).
pub fn server_up(server: &str) -> bool {
    if server.is_empty() {
        return false;
    }
    std::process::Command::new("curl")
        .args(["-s", "--max-time", "1", "-o", "/dev/null", &format!("{server}/")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One timestamped line per `stop` in the log: each stage's time since the
/// previous mark, so slow dictations can be explained after the fact (the
/// TSV itself is rewritten on every release).
pub fn log_release_summary(ctx: &Ctx) {
    let marks = release_marks(ctx);
    let (Some(first), Some(last)) = (marks.first(), marks.last()) else { return };
    let stages: Vec<String> = marks.windows(2)
        .map(|w| format!("{} +{:.0}", w[1].0, (w[1].1 - w[0].1) * 1000.0))
        .collect();
    append_log(&ctx.log, &format!("{} release {:.0} ms: {}",
        crate::history::local_iso_now(), (last.1 - first.1) * 1000.0, stages.join(", ")));
}

/// Spawn the preview scheduler detached (phase 1: re-exec this binary's
/// `preview-worker` hidden subcommand so all logic stays in Rust).
fn spawn_preview(ctx: &Ctx) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dictate"));
    let child = std::process::Command::new(exe)
        .arg("preview-worker")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(child) = child {
        let _ = std::fs::write(&ctx.paths.preview_pidfile, format!("{}\n", child.id()));
    }
}

// -------------------------------------------------------------------- stop

/// Pure gates (unit-tested): sub-half-second blips and noise-floor captures
/// never reach the model.
pub fn is_too_short(duration_secs: f64) -> bool {
    duration_secs < 0.5
}

pub fn is_silence(peak_db: Option<f32>, min_peak_db: f64) -> bool {
    match peak_db {
        Some(p) => (p as f64) < min_peak_db,
        None => false,
    }
}

/// `dictate stop`: transcribe + deliver. Soft failures notify + Ok(());
/// hard transcription failure returns Err (bash `return 1`).
pub fn cmd_stop(ctx: &Ctx) -> Result<()> {
    let _ = std::fs::write(&ctx.paths.release_timing, "event\ttimestamp\n");
    ctx.timing_active.set(true);
    // Process-start timestamp, not stop entry (bash `command_started`).
    let started_ts = ctx.command_started;
    ctx.mark_at("command_started", started_ts);
    ctx.mark("stop_entered");
    match daemon::request(&ctx.paths, "stop").as_deref() {
        Some(r) if r.starts_with("ok") => {
            ctx.mark("recorder_stopped");
            notify(ctx, "\u{23f3} Transcribing", "", 30000);
            let early = r.ends_with(" early").then(|| {
                let path = early_pass_path(&ctx.paths);
                let early = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
                let _ = std::fs::remove_file(&path);
                early
            }).flatten();
            return finish_stop(ctx, early);
        }
        Some(r) if r.starts_with("error") => {
            let _ = std::fs::remove_file(&ctx.paths.startfile);
            append_log(&ctx.log, &format!("dictate daemon: {r}"));
            notify(ctx, "dictate: recording failed", r.trim_start_matches("error "), 4000);
            return Ok(());
        }
        // "idle" or no daemon: this recording, if any, is the per-process one.
        _ => {}
    }
    // No pidfile != not recording (orphan sweep, like bash).
    let recorded = if ctx.paths.pidfile.exists() {
        let pid = ctx.state.take_recorded_pid();
        stop_preview(&ctx.paths);
        notify(ctx, "⏳ Transcribing", "", 30000);
        if let Some(pid) = pid {
            state::send_int(pid);
            ctx.mark("recorder_stop_requested");
            state::wait_exit(pid, 50);
        }
        Some(())
    } else {
        stop_preview(&ctx.paths);
        reap_recorders();
        if std::fs::metadata(&ctx.paths.raw).map(|m| m.len() == 0).unwrap_or(true) {
            return Ok(());
        }
        None
    };
    let _ = recorded;
    ctx.mark("recorder_stopped");

    if std::fs::metadata(&ctx.paths.raw).map(|m| m.len() == 0).unwrap_or(true) {
        let _ = std::fs::remove_file(&ctx.paths.startfile);
        notify(ctx, "dictate: no audio captured", "check the mic — dictate doctor", 4000);
        return Ok(());
    }
    if ctx.audio().mono16_with_channels(&ctx.paths.raw, &ctx.paths.wav, ctx.chan(), ctx.channels(), &ctx.log).is_err()
        || std::fs::metadata(&ctx.paths.wav).map(|m| m.len() == 0).unwrap_or(true)
    {
        let mic = ctx.mic();
        let channel = format!("{}/{}", ctx.chan(), ctx.channels());
        let raw_format = format!("s16le,48000Hz,{}ch", ctx.channels());
        // Best-effort: a history failure keeps scratch audio and continues
        // to the user notice (bash `HISTORY_ENTRY=""` + branch below).
        let entry = ctx.history.archive_raw_audio(
            &ctx.paths.raw, "channel-extraction-failed",
            &[("mic", mic.as_str()), ("channel", channel.as_str()), ("raw_format", raw_format.as_str())])
            .unwrap_or_else(|e| {
                append_log(&ctx.log, &format!("warning: could not archive failed raw capture: {e:#}"));
                None
            });
        if !ctx.history.enabled || entry.is_some() {
            let _ = std::fs::remove_file(&ctx.paths.raw);
            let _ = std::fs::remove_file(&ctx.paths.startfile);
        }
        if entry.is_some() {
            notify(ctx, &format!("dictate: could not extract channel {}", ctx.chan()),
                &format!("raw audio retained; see {}", ctx.log.display()), 4000);
        } else {
            notify(ctx, &format!("dictate: could not extract channel {}", ctx.chan()),
                &format!("scratch audio kept; see {}", ctx.log.display()), 4000);
        }
        return Ok(());
    }

    finish_stop(ctx, None)
}

/// The rest of `stop` once `Paths::wav` holds the clip: archive, transcribe,
/// deliver.
fn finish_stop(ctx: &Ctx, early: Option<EarlyPass>) -> Result<()> {
    ctx.mark("audio_prepared");
    let prefetch = (ctx.c("DICTATE_NOTYPE", "0") != "1").then(|| prefetch_delivery(ctx));
    let started = std::fs::read_to_string(&ctx.paths.startfile)
        .unwrap_or_else(|_| crate::history::local_iso_now());
    let started = started.trim().to_string();
    // Best-effort like bash: transcription/delivery proceed even when the
    // archive fails; only the entry-gated status updates are skipped.
    let entry = ctx.history.archive_audio(&ctx.paths.wav, "captured",
        &[("started", started.as_str()),
          ("engine", ctx.c("DICTATE_ENGINE", "whisper").as_str()),
          ("mic", ctx.mic().as_str()),
          ("channel", &format!("{}/{}", ctx.chan(), ctx.channels())),
          ("mode", ctx.state.read_mode().as_str())])
        .unwrap_or_else(|e| {
            append_log(&ctx.log, &format!("warning: could not archive dictation audio: {e:#}"));
            None
        });
    ctx.mark("audio_archived");
    if !ctx.history.enabled || entry.is_some() {
        let _ = std::fs::remove_file(&ctx.paths.raw);
        let _ = std::fs::remove_file(&ctx.paths.startfile);
    }

    let dur = wav::read_mono16(&ctx.paths.wav)
        .map(|s| s.len() as f64 / wav::RATE as f64)
        .unwrap_or_else(|_| ffprobe_duration(&ctx.paths.wav));
    if is_too_short(dur) {
        if let Some(e) = &entry {
            ctx.history.set_status(e, "too-short");
        }
        notify(ctx, "dictate: too short", "", 1200);
        return Ok(());
    }
    notify(ctx, "\u{23f3} transcribing\u{2026}", "", 15000);

    let mode = ctx.state.take_mode();
    let text = match transcribe_to_text(ctx, &ctx.paths.wav, entry.as_deref(), &mode, early) {
        Err(e) => {
            notify(ctx, "dictate: transcription failed", "audio retained in history; see log", 4000);
            return Err(e);
        }
        Ok(TextOutcome::Silence(peak)) => {
            notify(ctx, "dictate: silence",
                &format!("peak {} dBFS", peak.map(|p| format!("{p:.1}")).unwrap_or_else(|| "?".into())), 1800);
            return Ok(());
        }
        Ok(TextOutcome::NoSpeech) => {
            notify(ctx, "dictate: no speech detected", "", 2500);
            return Ok(());
        }
        Ok(TextOutcome::Noise(text)) => {
            notify(ctx, "dictate: discarded noise", &text, 2000);
            return Ok(());
        }
        Ok(TextOutcome::Ready { text, .. }) => text,
    };

    if ctx.c("DICTATE_NOTYPE", "0") == "1" {
        ctx.clipboard().set(&text)?;
        if let Some(e) = &entry {
            ctx.history.set_status(e, "copied-to-clipboard");
        }
        notify_done(ctx, "Copied to clipboard", &text, entry.as_deref(), dur);
    } else {
        deliver_with(ctx, &text, prefetch);
        ctx.mark("delivery_returned");
        if let Some(e) = &entry {
            ctx.history.set_status(e, "delivery-attempted");
        }
        notify_done(ctx, "Pasted", &text, entry.as_deref(), dur);
    }
    if let Some(e) = &entry {
        spawn_shadow(ctx, e);
    }
    Ok(())
}

/// Shadow labelling (`DICTATE_SHADOW=1`, only while the final pass is local):
/// after the paste, a detached `dictate shadow <entry>` sends the audio to
/// ElevenLabs once, Qwen-corrected like a final pass, and saves it as
/// `transcript.elevenlabs.txt` next to the dictation. That gives fine-tuning
/// labels from a better model and, in `dictate stats`, how often it would
/// have pasted something else. Spending stops at `DICTATE_SHADOW_MAX_MIN`
/// minutes of audio in total.
fn spawn_shadow(ctx: &Ctx, entry: &Path) {
    if ctx.c("DICTATE_SHADOW", "0") != "1" || ctx.c("DICTATE_FINAL", "whisper") == "elevenlabs" {
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dictate"));
    let _ = std::process::Command::new(exe)
        .arg("shadow")
        .arg(entry)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn run_shadow(ctx: &Ctx, entry: &Path) -> Result<()> {
    let max_min: f64 = ctx.c("DICTATE_SHADOW_MAX_MIN", "360").parse().unwrap_or(360.0);
    let used = crate::usage::audio_minutes(&ctx.state_dir(), "shadow");
    if used >= max_min {
        append_log(&ctx.log, &format!("shadow: budget used ({used:.0} of {max_min:.0} min), skipping"));
        return Ok(());
    }
    let samples = wav::read_mono16(&entry.join("audio.wav"))?;
    let meter = crate::usage::Meter::new(&ctx.state_dir(), &ctx.cfg, "shadow");
    let mut scribe = crate::elevenlabs::Scribe::from_config(&ctx.cfg, &ctx.terms())?;
    scribe.meter = Some(meter.clone());
    let raw = match scribe.transcribe(&samples) {
        Ok(t) => t,
        Err(e) => {
            append_log(&ctx.log, &format!("shadow: elevenlabs failed: {e:#}"));
            return Ok(());
        }
    };
    let text = Corrector::from_config(&ctx.cfg, &ctx.terms())
        .map(|c| Corrector { meter: Some(meter), ..c })
        .and_then(|c| c.correct(&raw))
        .unwrap_or_else(|_| raw.clone());
    std::fs::write(entry.join("transcript.elevenlabs.raw.txt"), format!("{raw}\n"))?;
    std::fs::write(entry.join("transcript.elevenlabs.txt"), format!("{text}\n"))?;
    Ok(())
}

/// Stage times of the current `stop`, from the release-timing TSV.
fn release_marks(ctx: &Ctx) -> Vec<(String, f64)> {
    std::fs::read_to_string(&ctx.paths.release_timing)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(e, t)| Some((e.to_string(), t.parse().ok()?)))
        .collect()
}

/// The finished-dictation card: the text plus how long each stage took, the
/// models and the history id, so a slow or wrong dictation can be looked up.
fn notify_done(ctx: &Ctx, label: &str, text: &str, entry: Option<&Path>, audio_secs: f64) {
    let marks = release_marks(ctx);
    let at = |e: &str| marks.iter().find(|(n, _)| n == e).map(|(_, t)| *t);
    let span = |a: &str, b: &str| Some((at(b)? - at(a)?) * 1000.0);
    // Rows of "label\tvalue\tunit" for the card's table; a line without tabs is a note.
    let mut parts = vec![format!("total\t{:.2}\ts", now_unix() - ctx.command_started)];
    let engine = if ctx.c("DICTATE_FINAL", "whisper") == "elevenlabs" { "elevenlabs" } else { "whisper" };
    let tail = ctx.tail.get();
    if at("early_pass_used").is_some() {
        parts.push("final pass\tin pause\t".into());
    } else if let Some(ms) = span("transcription_started", "transcription_finished") {
        // Incremental: only the audio after the last pause was left to recognize.
        let label = if tail.is_some() { format!("{engine} tail") } else { engine.to_string() };
        parts.push(format!("{label}\t{:.2}\ts", ms / 1000.0));
    }
    if let Some((segments, secs)) = tail {
        parts.push(format!("pause segments\t{segments}\t"));
        if secs > 0.0 {
            parts.push(format!("tail audio\t{secs:.1}\ts"));
        }
    }
    if let Some(ms) = span("correction_started", "correction_finished") {
        parts.push(format!("correction\t{:.2}\ts", ms / 1000.0));
    }
    if let Some(ms) = span("text_ready", "delivery_returned") {
        parts.push(format!("paste\t{:.2}\ts", ms / 1000.0));
    }
    parts.push(format!("audio\t{audio_secs:.1}\ts"));
    crate::usage::record_dictation(&ctx.state_dir(), &crate::usage::Dictation {
        ts: now_unix(),
        id: entry.and_then(|e| e.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        audio_secs,
        setup: ctx.final_label(),
        early_used: at("early_pass_used").is_some(),
        total_ms: (now_unix() - ctx.command_started) * 1000.0,
        recognizer_ms: at("early_pass_used").is_none().then(|| span("transcription_started", "transcription_finished")).flatten(),
        correction_ms: at("early_pass_used").is_none().then(|| span("correction_started", "correction_finished")).flatten(),
        paste_ms: span("text_ready", "delivery_returned"),
        segments: tail.map_or(0, |t| t.0),
        tail_secs: tail.map(|t| t.1),
    });
    // One stage per line: the card shows only these, so they must be readable at a glance.
    let mut stats = parts.join("\n");
    if let Some(id) = entry.and_then(|e| e.file_name()) {
        stats += &format!("\n{}", id.to_string_lossy());
    }
    let secs = ctx.c("DICTATE_OSD_DONE_MS", "8000").parse::<f64>().unwrap_or(5000.0) / 1000.0;
    let msg = OverlayMessage {
        phase: crate::overlay::Phase::Success,
        label: label.into(),
        text: text.into(),
        tentative: String::new(),
        engine: ctx.final_label(),
        stats,
        audio: String::new(),
        channels: 1,
        channel: 0,
        duration: secs,
        expires: now_unix() + secs,
    };
    let _ = JsonFileOverlay { state_path: ctx.overlay_path.clone() }.show(&msg);
}

/// What the audio → text half of `stop` produced for one clip.
pub enum TextOutcome {
    Silence(Option<f32>),
    NoSpeech,
    Noise(String),
    Ready { raw: String, text: String },
}

/// Silence gate, transcription, cleanup, noise blocklist and style for one
/// mono 16 kHz wav — everything `stop` does between capture and delivery,
/// shared with `replay`. History statuses and transcripts are written only
/// when `entry` is given; overlay notices are left to the caller.
fn transcribe_to_text(ctx: &Ctx, wav: &Path, entry: Option<&Path>, mode: &str, early: Option<EarlyPass>)
    -> Result<TextOutcome> {
    let pass = FinalPass::new(ctx, "final");

    // Measured in-process for our own 16 kHz clips; ffmpeg cost ~100 ms here.
    let peak = match wav::read_mono16(wav) {
        Ok(s) => Some(wav::peak_db(&s)),
        Err(_) => ctx.audio().measure_peak_db(wav),
    };
    let feed: PathBuf = if ctx.c("DICTATE_FILTER", "").is_empty() {
        wav.to_path_buf()
    } else if apply_filter(wav, &ctx.paths.wav_proc, &ctx.c("DICTATE_FILTER", ""), &ctx.log) {
        ctx.paths.wav_proc.clone()
    } else {
        append_log(&ctx.log, "warning: filter chain failed, feeding raw audio to whisper");
        wav.to_path_buf()
    };

    if is_silence(peak, pass.min_peak) {
        if let Some(e) = entry {
            ctx.history.set_status(e, "silence");
        }
        return Ok(TextOutcome::Silence(peak));
    }

    ctx.mark("audio_checked");
    if let Some(e) = &entry {
        ctx.history.set_status(e, "transcribing");
    }
    ctx.mark("transcription_started");
    // Incremental: the daemon's segments end at `until`; only the rest of
    // the clip is recognized now, with their text as context.
    let tail = early.as_ref().and_then(|e| Some((e.until?, wav::read_mono16(&feed).ok()?)))
        .filter(|(until, samples)| until < &samples.len());
    let raw_text = match (&early, ctx.engine()) {
        (Some(e), _) if tail.is_some() => {
            let (until, samples) = tail.unwrap();
            ctx.mark("early_segments_used");
            ctx.tail.set(Some((e.segments, (samples.len() - until) as f64 / wav::RATE as f64)));
            let context = e.corrected.as_deref().unwrap_or(&e.clean);
            match pass.recognize(&samples[until..], Some(context)) {
                Ok(t) => Ok(crate::segment::join(&e.raw, &t)),
                Err(err) => {
                    append_log(&ctx.log, &format!("note: tail pass failed, recognizing the whole clip: {err:#}"));
                    ctx.tail.set(None);
                    pass.whisper(&samples)
                }
            }
        }
        (Some(e), _) => {
            ctx.mark("early_pass_used");
            ctx.tail.set((e.segments > 0).then_some((e.segments, 0.0)));
            Ok(e.raw.clone())
        }
        (None, Engine::Parakeet) => {
            let pc = ParakeetConfig {
                model: ctx.parakeet_model(),
                threads: ctx.c("DICTATE_THREADS", "8").parse().unwrap_or(8),
                bin: "parakeet-cli".into(),
            };
            transcribe::transcribe_parakeet(&feed, &pc, &ctx.log)
        }
        (None, Engine::Whisper) => match wav::read_mono16(&feed) {
            Ok(samples) => pass.whisper(&samples),
            Err(_) => transcribe::transcribe_whisper(&feed, &pass.whisper, &ctx.log),
        },
    };
    let Ok(raw_text) = raw_text else {
        if let Some(e) = entry {
            ctx.history.set_status(e, "transcription-failed");
        }
        anyhow::bail!("dictate: transcription failed");
    };
    ctx.mark("transcription_finished");
    let raw = text::clean_whisper_text(&raw_text);
    if raw.is_empty() {
        if let Some(e) = entry {
            ctx.history.set_status(e, "no-speech");
        }
        return Ok(TextOutcome::NoSpeech);
    }
    if let Some(e) = entry {
        ctx.history.save_text(e, &raw, "transcript.raw.txt");
    }

    let mut text = raw.clone();
    if mode == "llm"
        && ctx.c("DICTATE_LLM", "0") == "1"
        && !ctx.cfg.get("DICTATE_LLM_KEY").unwrap_or("").is_empty()
        && !ctx.cfg.get("DICTATE_LLM_URL").unwrap_or("").is_empty()
        && !ctx.cfg.get("DICTATE_LLM_MODEL").unwrap_or("").is_empty()
    {
        if let Some(e) = &entry {
            ctx.history.set_status(e, "llm-cleaning");
        }
        notify(ctx, "\u{23f3} Polishing wording", &text,
            (ctx.c("DICTATE_LLM_TIMEOUT", "15").parse::<u64>().unwrap_or(15) + 5) * 1000);
        let cleaned = llm_cleanup(ctx, &text);
        if !cleaned.is_empty() && cleaned != text {
            if let Some(e) = &entry {
                ctx.history.save_text(e, &cleaned, "transcript.cleaned.txt");
            }
            text = cleaned;
        }
    }

    let noise = ctx.c("DICTATE_NOISE", text::DEFAULT_NOISE_PHRASES);
    if text::is_noise(&text, &noise) {
        if let Some(e) = entry {
            ctx.history.set_status(e, "discarded-noise");
        }
        return Ok(TextOutcome::Noise(text));
    }

    ctx.mark("correction_started");
    let corrected = match early.and_then(|e| e.corrected.filter(|_| e.clean == text)) {
        Some(fixed) => Some(Ok(fixed)),
        None => pass.correct(&text),
    };
    if let Some(corrected) = corrected {
        match corrected {
            Ok(fixed) => {
                if fixed != text {
                    if let Some(e) = entry {
                        ctx.history.save_text(e, &fixed, "transcript.corrected.txt");
                    }
                    text = fixed;
                }
            }
            Err(e) => append_log(&ctx.log, &format!("note: correction skipped, keeping whisper text: {e:#}")),
        }
        ctx.mark("correction_finished");
    }

    // Style last: the blocklist above matches whisper's own wording.
    text = text::restyle(&text, ctx.c("DICTATE_STYLE", "0") == "1", &ctx.terms());
    if let Some(e) = entry {
        ctx.history.save_text(e, &text, "transcript.txt");
        ctx.history.set_status(e, "ready");
    }
    ctx.mark("text_ready");
    Ok(TextOutcome::Ready { raw, text })
}

/// The final pass after capture: whisper, then the optional correction. The
/// daemon also runs it while you pause, so a dictation that ends in a pause
/// is ready the moment the key comes up (`EarlyPass`).
pub struct FinalPass {
    whisper: WhisperConfig,
    /// `DICTATE_FINAL=elevenlabs`: recognize with ElevenLabs instead of
    /// whisper; whisper stays the fallback when the request fails.
    scribe: Option<std::result::Result<crate::elevenlabs::Scribe, String>>,
    /// `None`: correction is off; `Err`: on but unusable (missing key).
    corrector: Option<std::result::Result<Corrector, String>>,
    log: PathBuf,
    scratch: PathBuf,
    min_peak: f64,
}

/// What the daemon recognized during the pauses: the whole dictation when
/// the last pause was its end, or (`DICTATE_INCREMENTAL`) the segments
/// before the last cut, leaving `stop` only the audio after `until`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct EarlyPass {
    pub raw: String,
    /// The correction of `clean`, used only if the final text still matches
    /// `clean` when correction comes up (nothing rewrote it in between).
    pub clean: String,
    pub corrected: Option<String>,
    /// Samples covered: `None` (or the clip length) when the pass covers the
    /// whole clip, else `stop` recognizes `samples[until..]` and appends it.
    #[serde(default)]
    pub until: Option<usize>,
    /// Pause segments recognized (incremental passes only).
    #[serde(default)]
    pub segments: usize,
}

impl FinalPass {
    /// `purpose` labels its paid requests in the usage log: `early` for the
    /// daemon's pause passes, `final` after release.
    pub fn new(ctx: &Ctx, purpose: &'static str) -> Self {
        let meter = crate::usage::Meter::new(&ctx.state_dir(), &ctx.cfg, purpose);
        let mut whisper = WhisperConfig::new(
            &ctx.server(), &ctx.model(),
            &ctx.c("DICTATE_LANG", "auto"), &ctx.c("DICTATE_PROMPT", ""),
            ctx.c("DICTATE_THREADS", "8").parse().unwrap_or(8));
        if ctx.c("DICTATE_VAD", "0") == "1" {
            let vm = ctx.vad_model();
            if Path::new(&vm).exists() {
                whisper.vad_model = Some(vm);
                whisper.vad_threshold = ctx.c("DICTATE_VAD_THRESHOLD", "0.5");
            }
        }
        let corrector = (ctx.c("DICTATE_CORRECT", "0") == "1")
            .then(|| Corrector::from_config(&ctx.cfg, &ctx.terms())
                .map(|c| Corrector { meter: Some(meter.clone()), ..c })
                .map_err(|e| format!("{e:#}")));
        let scribe = (ctx.c("DICTATE_FINAL", "whisper") == "elevenlabs")
            .then(|| crate::elevenlabs::Scribe::from_config(&ctx.cfg, &ctx.terms())
                .map(|mut s| { s.meter = Some(meter.clone()); s })
                .map_err(|e| format!("{e:#}")));
        Self {
            whisper,
            scribe,
            corrector,
            log: ctx.log.clone(),
            scratch: ctx.paths.datadir.clone(),
            min_peak: ctx.c("DICTATE_MIN_PEAK", "-45").parse().unwrap_or(-45.0),
        }
    }

    /// Whisper's text for 16 kHz mono samples. Long clips go in pieces cut
    /// at pauses (see `wav::WHISPER_PIECE_SECS`), and silent pieces are
    /// skipped: whisper invents "Thank you." for silence.
    pub fn whisper(&self, samples: &[i16]) -> Result<String> {
        self.recognize(samples, None)
    }

    /// `whisper`, for a segment of a longer recording when `context` is
    /// given: the text recognized before it goes after whisper's prompt, and
    /// each piece of a long segment gets the pieces before it the same way,
    /// so sentences carry across the joins. ElevenLabs takes no such
    /// context, only the glossary as keyterms.
    pub fn recognize(&self, samples: &[i16], context: Option<&str>) -> Result<String> {
        match &self.scribe {
            Some(Ok(scribe)) => match scribe.transcribe(samples) {
                Ok(text) => return Ok(text),
                Err(e) => append_log(&self.log, &format!("note: elevenlabs failed, using whisper: {e:#}")),
            },
            Some(Err(e)) => append_log(&self.log, &format!("note: elevenlabs unavailable, using whisper: {e}")),
            None => {}
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static PIECE: AtomicU64 = AtomicU64::new(0);
        let pieces = wav::split_at_pauses(samples, wav::WHISPER_PIECE_SECS);
        let single = pieces.len() == 1;
        let mut texts: Vec<String> = Vec::new();
        for range in pieces {
            if !single && is_silence(Some(wav::peak_db(&samples[range.clone()])), self.min_peak) {
                continue;
            }
            let n = PIECE.fetch_add(1, Ordering::Relaxed);
            let piece = self.scratch.join(format!("dictate-piece-{}-{n}.wav", std::process::id()));
            wav::write_mono16(&piece, &samples[range])?;
            let text = match context {
                None => transcribe::transcribe_whisper(&piece, &self.whisper, &self.log),
                Some(before) => {
                    let before = texts.iter().fold(before.to_string(), |a, t| crate::segment::join(&a, &text::clean_whisper_text(t)));
                    let cfg = WhisperConfig {
                        prompt: crate::segment::prompt_with_context(&self.whisper.prompt, &before),
                        ..self.whisper.clone()
                    };
                    transcribe::transcribe_whisper(&piece, &cfg, &self.log)
                }
            };
            let _ = std::fs::remove_file(&piece);
            texts.push(text?);
        }
        Ok(texts.join(" "))
    }

    /// `None` when correction is off.
    pub fn correct(&self, text: &str) -> Option<Result<String>> {
        Some(match self.corrector.as_ref()? {
            Ok(c) => c.correct(text),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        })
    }
}

/// Where the daemon leaves an early pass for `stop`.
pub fn early_pass_path(paths: &Paths) -> PathBuf {
    paths.datadir.join("dictate-early.json")
}

/// Optional experimental ffmpeg filter chain (bash `$FILTER`); empty default.
fn apply_filter(wav: &Path, out: &Path, filter: &str, log: &Path) -> bool {
    let logfile = std::fs::OpenOptions::new().create(true).append(true).open(log).ok();
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(wav)
        .args(["-af", filter, "-c:a", "pcm_s16le", "-y"])
        .arg(out);
    if let Some(f) = logfile {
        cmd.stderr(f);
    } else {
        cmd.stderr(std::process::Stdio::null());
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn ffprobe_duration(wav: &Path) -> f64 {
    std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(wav)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0.0)
}

/// Legacy LLM cleanup: any failure returns the raw text unchanged.
fn llm_cleanup(ctx: &Ctx, raw: &str) -> String {
    let url = ctx.cfg.get("DICTATE_LLM_URL").unwrap_or("");
    let model = ctx.cfg.get("DICTATE_LLM_MODEL").unwrap_or("");
    let key = ctx.cfg.get("DICTATE_LLM_KEY").unwrap_or("");
    let prompt = ctx.c("DICTATE_PROMPT", "");
    let timeout = ctx.c("DICTATE_LLM_TIMEOUT", "15");
    let body = serde_json::json!({
        "model": model, "temperature": 0.0, "max_tokens": 2048,
        "thinking": {"type": "enabled", "effort": "low"},
        "messages": [
            {"role": "system", "content": format!("You fix speech-to-text transcripts of a software developer dictating. Correct misrecognized technical terms, fix spelling and punctuation. Only change a word when you are CERTAIN it is a misrecognition of known vocabulary or an obvious speech-to-text error; otherwise copy it exactly. Project-specific terms and names are usually correct - do not fix them. Never change the wording, never add or remove content, never answer or comment. Output ONLY the corrected text, nothing else. Known vocabulary: {prompt}")},
            {"role": "user", "content": raw}
        ]
    });
    // Payload via stdin (`-d @-`, like bash): long dictations must not
    // ride argv (ARG_MAX), and the body has no business in `ps`. (Bash
    // already exposes the key header on argv; we keep that parity.)
    let out = (|| {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new("curl")
            .args(["-sf", "--connect-timeout", "2", "--max-time", &timeout, url])
            .args(["-H", &format!("Authorization: Bearer {key}"), "-H", "Content-Type: application/json"])
            .args(["-d", "@-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.as_mut()?.write_all(body.to_string().as_bytes()).ok()?;
        drop(child.stdin.take());
        child.wait_with_output().ok()
    })();
    let Some(out) = out else {
        append_log(&ctx.log, "note: llm cleanup failed/timed out, keeping raw text");
        return raw.to_string();
    };
    if !out.status.success() {
        append_log(&ctx.log, "note: llm cleanup failed/timed out, keeping raw text");
        return raw.to_string();
    }
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => {
            append_log(&ctx.log, "note: llm cleanup failed/timed out, keeping raw text");
            return raw.to_string();
        }
    };
    match v.pointer("/choices/0/message/content").and_then(|c| c.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            append_log(&ctx.log, "note: llm cleanup failed/timed out, keeping raw text");
            raw.to_string()
        }
    }
}

// ----------------------------------------------------------------- deliver

/// Delivery work started while the text is still being recognized.
#[derive(Default)]
pub struct DeliveryPrefetch {
    /// Tools engine: the probed backend and the clipboard to restore.
    tools: Option<std::thread::JoinHandle<(ProcessClipboard, String)>>,
    /// Native engine: our X connection and a `clipboard-owner` worker that
    /// has already read the clipboard to restore.
    x: Option<std::thread::JoinHandle<Result<X>>>,
    owner: Option<OwnerWorker>,
}

fn paste_engine(ctx: &Ctx) -> PasteEngine {
    PasteEngine::parse(&ctx.c("DICTATE_PASTE_ENGINE", "native"))
}

fn restore_mode(ctx: &Ctx) -> bool {
    ctx.c("DICTATE_CLIPBOARD", "restore") == "restore"
}

fn settle_secs(ctx: &Ctx) -> f64 {
    ctx.c("DICTATE_PASTE_SETTLE", "0.4").parse().unwrap_or(0.4)
}

fn dictate_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dictate"))
}

/// Starts what `deliver_with` needs before the text exists, so it runs
/// while whisper does: process spawns, X connections, and reading the
/// clipboard to restore.
fn prefetch_delivery(ctx: &Ctx) -> DeliveryPrefetch {
    if InputMode::parse(&ctx.c("DICTATE_INPUT", "auto")) == InputMode::Type {
        return DeliveryPrefetch::default();
    }
    match paste_engine(ctx) {
        PasteEngine::Native => DeliveryPrefetch {
            x: Some(std::thread::spawn(X::connect)),
            owner: OwnerWorker::spawn(&dictate_exe()).ok(),
            ..Default::default()
        },
        PasteEngine::Tools => {
            // Save the clipboard for restoring while whisper runs, not after
            // it (copyq answers in ~60 ms). A copy made in between is lost.
            let restore = restore_mode(ctx);
            DeliveryPrefetch {
                tools: Some(std::thread::spawn(move || {
                    let clip = ProcessClipboard::new();
                    clip.backend();
                    let prev = if restore { clip.get() } else { String::new() };
                    (clip, prev)
                })),
                ..Default::default()
            }
        }
    }
}

/// Owns the clipboard from here: reads previous contents first (bash order),
/// verifies ownership, sends one chord, restores in background.
pub fn deliver(ctx: &Ctx, text: &str) {
    deliver_with(ctx, text, None)
}

/// `deliver` with the work `prefetch_delivery` started (`None`: do it now).
fn deliver_with(ctx: &Ctx, text: &str, prefetch: Option<DeliveryPrefetch>) {
    let mode = InputMode::parse(&ctx.c("DICTATE_INPUT", "auto"));
    if mode == InputMode::Type {
        let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
        return;
    }
    let p = prefetch.unwrap_or_default();
    if paste_engine(ctx) == PasteEngine::Native {
        let x = match p.x.map(|h| h.join()) {
            Some(Ok(Ok(x))) => Ok(x),
            _ => X::connect(),
        };
        let r = x.and_then(|x| deliver_native(ctx, text, mode, &x, p.owner));
        match r {
            Ok(()) => return,
            Err(e) => append_log(&ctx.log, &format!("note: native paste unavailable ({e:#}), using copyq/xclip")),
        }
    }
    deliver_tools(ctx, text, mode, p.tools.and_then(|h| h.join().ok()));
}

/// The native engine. `Err` only before anything reached the window, so
/// the caller can fall back to the tools engine.
fn deliver_native(ctx: &Ctx, text: &str, mode: InputMode, x: &X, owner: Option<OwnerWorker>) -> Result<()> {
    let cls = x.active_window_class();
    ctx.mark("window_class_read");
    let chord = match delivery::select_action(mode, &cls, &ctx.c("DICTATE_TERMINALS", delivery::DEFAULT_TERMINALS)) {
        DeliveryAction::Type => {
            let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
            return Ok(());
        }
        DeliveryAction::Paste { chord } => chord,
    };
    let mut owner = match owner {
        Some(o) => o,
        None => OwnerWorker::spawn(&dictate_exe())?,
    };
    ctx.mark("clipboard_backend_selected");
    // The worker re-reads the clipboard to restore, takes it, and answers
    // once the X server lists it as the owner.
    owner.offer(text, restore_mode(ctx), settle_secs(ctx), std::time::Duration::from_secs(1))?;
    ctx.mark("clipboard_set");
    // Never paste what was there before: read it back through the server.
    if x.read_clipboard(std::time::Duration::from_millis(200)).ok().flatten().as_deref() != Some(text) {
        append_log(&ctx.log, "note: clipboard not ready, typing instead");
        // The worker still restores the previous clipboard when it settles.
        owner.pasted();
        let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
        return Ok(());
    }
    ctx.mark("clipboard_verified");
    let _ = ctx.xdotool().send_paste(chord, &ctx.c("DICTATE_PASTE_KEY_DELAY", "0.012"), ctx.timing_tsv());
    owner.pasted();
    Ok(())
}

/// The tools engine (`DICTATE_PASTE_ENGINE=tools`, and the fallback):
/// copyq or xclip and xdotool processes for every step.
fn deliver_tools(ctx: &Ctx, text: &str, mode: InputMode, saved: Option<(ProcessClipboard, String)>) {
    let cls = ctx.xdotool().active_window_class();
    ctx.mark("window_class_read");
    match delivery::select_action(mode, &cls, &ctx.c("DICTATE_TERMINALS", delivery::DEFAULT_TERMINALS)) {
        DeliveryAction::Type => {
            let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
        }
        DeliveryAction::Paste { chord } => {
            let (clip, saved_clipboard) = match saved {
                Some((clip, prev)) => (clip, Some(prev)),
                None => (ctx.clipboard(), None),
            };
            clip.backend();
            ctx.mark("clipboard_backend_selected");
            let prev = if restore_mode(ctx) {
                saved_clipboard.unwrap_or_else(|| clip.get())
            } else {
                String::new()
            };
            ctx.mark("clipboard_saved");
            if clip.set(text).is_err() {
                let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
                return;
            }
            ctx.mark("clipboard_set");
            if !delivery::wait_clipboard(|| clip.get(), text, 10) {
                append_log(&ctx.log, "note: clipboard not ready, typing instead");
                let _ = ctx.xdotool().type_text(text, &ctx.c("DICTATE_TYPE_DELAY", "4"));
                return;
            }
            ctx.mark("clipboard_verified");
            let _ = ctx.xdotool().send_paste(chord, &ctx.c("DICTATE_PASTE_KEY_DELAY", "0.012"), ctx.timing_tsv());
            if restore_mode(ctx) && !prev.is_empty() {
                // Detached re-execution outlives `stop` (bash background
                // subshell); a thread here would die with the process before
                // the settle delay elapsed and never restore.
                if delivery::schedule_detached_restore(
                    &ctx.paths.datadir, &dictate_exe(), &clip, text, &prev, settle_secs(ctx),
                ).is_err() {
                    append_log(&ctx.log, "note: clipboard restore unavailable, keeping dictation text");
                }
            }
        }
    }
}

/// Hidden `dictate paste-bench`: `runs` deliveries of `text` (numbered) to
/// the focused window, timed like `stop` times them, one TSV row of
/// milliseconds per run. It changes the clipboard and sends keys: run it on
/// an isolated X server (`DISPLAY=:77`), never on the desktop in use.
pub fn cmd_paste_bench(ctx: &Ctx, text: &str, runs: usize, work_ms: u64, gap_ms: u64) -> Result<()> {
    const STEPS: [&str; 7] = ["window_class_read", "clipboard_backend_selected", "clipboard_saved",
        "clipboard_set", "clipboard_verified", "paste_shortcut_sent", "delivery_returned"];
    println!("run\t{}\ttotal", STEPS.join("\t"));
    ctx.timing_active.set(true);
    for run in 0..runs {
        let _ = std::fs::write(&ctx.paths.release_timing, "event\ttimestamp\n");
        let prefetch = prefetch_delivery(ctx);
        // Stands in for recognition, which the prefetch overlaps.
        std::thread::sleep(std::time::Duration::from_millis(work_ms));
        ctx.mark("text_ready");
        deliver_with(ctx, &format!("{text} {run}"), Some(prefetch));
        ctx.mark("delivery_returned");
        let marks = release_marks(ctx);
        let at = |e: &str| marks.iter().find(|(n, _)| n == e).map(|(_, t)| *t);
        let t0 = at("text_ready").unwrap_or(0.0);
        let mut prev = t0;
        let mut row = vec![run.to_string()];
        for step in STEPS {
            match at(step) {
                Some(t) => {
                    row.push(format!("{:.1}", (t - prev) * 1000.0));
                    prev = t;
                }
                None => row.push("-".into()),
            }
        }
        row.push(format!("{:.1}", (prev - t0) * 1000.0));
        println!("{}", row.join("\t"));
        std::thread::sleep(std::time::Duration::from_millis(gap_ms));
    }
    Ok(())
}

// ------------------------------------------------------------------- retry

/// `dictate retry [id]`: deliver a saved transcript (bash `history_retry`).
pub fn cmd_retry(ctx: &Ctx, id: Option<&str>) -> Result<()> {
    let entry = match id {
        Some(wanted) => ctx.history.resolve(wanted)
            .map_err(|_| anyhow::anyhow!("dictate: history entry not found or prefix is ambiguous: {wanted}"))?,
        None => ctx.history.latest_with_transcript()?,
    };
    let text = std::fs::read_to_string(entry.join("transcript.txt"))
        .context("dictate: no saved transcript found; use 'dictate history' and 'dictate show ID'")?;
    if text.trim().is_empty() {
        anyhow::bail!("dictate: no saved transcript found; use 'dictate history' and 'dictate show ID'");
    }
    if ctx.c("DICTATE_NOTYPE", "0") == "1" {
        ctx.clipboard().set(&text)?;
        notify(ctx, "\u{1f4cb} recovered dictation", &text,
            ctx.c("DICTATE_OSD_DONE_MS", "1000").parse().unwrap_or(1000));
    } else {
        deliver(ctx, &text);
    }
    println!("retried {}", entry.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
    Ok(())
}

// ------------------------------------------------------------------ replay

/// `replay [n]`: run the newest `n` saved dictations back through the same
/// audio → text path as `stop` and compare against the transcript saved at
/// the time. Read-only: no history writes, overlay, or delivery. Entries
/// cleaned by the LLM pass are skipped, since replay never calls it.
///
/// A difference is not automatically a regression — prompt, model, or
/// config may have changed since the recording — but on unchanged config it
/// should be rare. Returns the exit code: 0 identical, 1 any difference.
pub fn cmd_replay(ctx: &Ctx, n: usize) -> Result<i32> {
    let (mut total, mut same) = (0, 0);
    for entry in ctx.history.entries()? {
        if total == n {
            break;
        }
        let wav = entry.join("audio.wav");
        let Ok(saved) = std::fs::read_to_string(entry.join("transcript.txt")) else { continue };
        if !wav.is_file() || entry.join("transcript.cleaned.txt").exists() {
            continue;
        }
        total += 1;
        let id = entry.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let started = now_unix();
        let now = match transcribe_to_text(ctx, &wav, None, "fast", None) {
            Ok(TextOutcome::Ready { text, .. }) => text,
            Ok(TextOutcome::Silence(_)) => "<silence>".into(),
            Ok(TextOutcome::NoSpeech) => "<no speech>".into(),
            Ok(TextOutcome::Noise(t)) => format!("<noise: {t}>"),
            Err(e) => format!("<error: {e:#}>"),
        };
        let ms = (now_unix() - started) * 1000.0;
        if now.trim() == saved.trim() {
            same += 1;
            println!("same  {id}  {ms:5.0}ms");
        } else {
            println!("DIFF  {id}  {ms:5.0}ms");
            println!("      saved: {}", saved.trim());
            println!("      now:   {}", now.trim());
        }
    }
    println!("{same}/{total} identical");
    Ok(if same == total { 0 } else { 1 })
}

// ------------------------------------------------------------------ toggle

pub fn cmd_toggle(ctx: &Ctx) -> Result<()> {
    if state::read_live_pid(&ctx.paths.pidfile).is_some()
        || daemon::request(&ctx.paths, "status").as_deref() == Some("recording")
    {
        cmd_stop(ctx)
    } else {
        cmd_start(ctx, false)
    }
}

// -------------------------------------------------------------------- demo

/// Visual-only walkthrough: no mic, transcribe, clipboard, or typing.
pub fn cmd_demo(ctx: &Ctx) -> Result<()> {
    if state::read_live_pid(&ctx.paths.pidfile).is_some() {
        anyhow::bail!("Finish the current recording before running the demo.");
    }
    let done: u64 = ctx.c("DICTATE_OSD_DONE_MS", "1000").parse().unwrap_or(1000);
    notify(ctx, "\u{1f3a4} recording\u{2026}", "", 30000);
    std::thread::sleep(std::time::Duration::from_secs(1));
    notify(ctx, "\u{1f3a4} A quieter panel, with room to follow your thoughts.", "", 30000);
    std::thread::sleep(std::time::Duration::from_secs(2));
    notify(ctx, "\u{1f3a4} A quieter panel, with room to follow your thoughts. Live text stays readable as you keep speaking.", "", 30000);
    std::thread::sleep(std::time::Duration::from_secs(2));
    notify(ctx, "\u{23f3} Transcribing", "", 30000);
    std::thread::sleep(std::time::Duration::from_secs(2));
    notify(ctx, "\u{23f3} Polishing wording", "A quieter panel, with room to follow your thoughts.", 30000);
    std::thread::sleep(std::time::Duration::from_secs(2));
    notify(ctx, "A quieter panel, with room to follow your thoughts.", "", done);
    Ok(())
}

// ------------------------------------------------------------------- doctor

/// Full `dictate doctor` port. Returns exit code (0 ok, 1 problems).
/// Levels mirror bash: `ok`, non-fatal `warn`, fatal `MISS` (rc=1).
pub fn cmd_doctor(ctx: &Ctx) -> i32 {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Level {
        Ok,
        Warn,
        Miss,
    }
    let mut rc = 0;
    let mut show = |level: Level, msg: String| {
        if level == Level::Miss {
            rc = 1;
        }
        let tag = match level {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Miss => "MISS",
        };
        println!("  {tag}    {msg}");
    };
    for b in ["ffmpeg", "ffprobe", "xdotool", "pactl", "jq", "curl", "python3", "timeout", "flock", "pgrep", "pkill", "awk", "sha256sum"] {
        let found = crate::doctor::lookup_in_path(b);
        show(if found { Level::Ok } else { Level::Miss }, b.to_string());
    }
    if crate::doctor::lookup_in_path("pw-record") {
        show(Level::Ok, "pw-record (PipeWire capture)".into());
    } else {
        show(Level::Warn, "pw-record missing — ffmpeg PulseAudio capture will be used".into());
    }
    if crate::doctor::lookup_in_path("dunstify") || crate::doctor::lookup_in_path("notify-send") {
        show(Level::Ok, "desktop notifications (failure fallback)".into());
    } else {
        show(Level::Warn, "no desktop notification command — overlay failures only reach the log".into());
    }
    if ctx.c("DICTATE_OSD_BACKEND", "gtk") == "gtk" {
        show(
            if crate::doctor::lookup_in_path("nix-build") { Level::Ok } else { Level::Miss },
            "nix-build (GTK overlay runtime)".into(),
        );
    } else {
        // Non-fatal: the feh image backend is bash-only (see README port
        // notes); text delivery still works, only the overlay stays dark.
        show(Level::Warn, "DICTATE_OSD_BACKEND=feh is bash-only; this binary shows no overlay".into());
    }
    let (engine_bin, engine_label, model) = match ctx.engine() {
        Engine::Parakeet => ("parakeet-cli", "parakeet-cli (selected engine)", ctx.parakeet_model()),
        Engine::Whisper => ("whisper-cli", "whisper-cli (selected engine)", ctx.model()),
    };
    show(
        if crate::doctor::lookup_in_path(engine_bin) { Level::Ok } else { Level::Miss },
        engine_label.into(),
    );
    if Path::new(&model).exists() {
        show(Level::Ok, format!("model {model}"));
    } else {
        show(Level::Miss, format!("model {model}"));
        println!("        install whisper.cpp and download a compatible ggml model");
        println!("        see the project README for setup instructions");
    }
    if ctx.c("DICTATE_VAD", "0") == "1" {
        // Same default as stop: an unset key still resolves to the standard
        // Silero model instead of reporting a missing empty path.
        let vm = ctx.vad_model();
        show(
            if Path::new(&vm).exists() { Level::Ok } else { Level::Miss },
            format!("vad   enabled, {vm}"),
        );
    } else {
        show(Level::Ok, "vad   disabled (enable with DICTATE_VAD=1 if needed)".into());
    }
    if ctx.engine() == Engine::Parakeet {
        show(Level::Ok, "server not used by parakeet engine".into());
    } else if server_up(&ctx.server()) {
        show(Level::Ok, format!("server {} (resident)", ctx.server()));
    } else {
        show(Level::Warn, format!("server {} unreachable — falling back to whisper-cli", ctx.server()));
        println!("        start it with: dictate serve");
    }
    // Disk space (>1GiB free required).
    let df = std::process::Command::new("df")
        .args(["-Pk", &ctx.paths.datadir.display().to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    show(
        if crate::doctor::df_has_space(&df) { Level::Ok } else { Level::Miss },
        format!("space on {}", ctx.paths.datadir.display()),
    );
    // History.
    if ctx.history.enabled {
        show(
            if ctx.paths.history_dir.is_dir() { Level::Ok } else { Level::Miss },
            format!("history {}", ctx.paths.history_dir.display()),
        );
    } else {
        show(Level::Warn, "history disabled — failed pastes and audio will not be retained".into());
    }
    // Stray recorders.
    let strays = state::stray_recorder_pids(&uid_string(), state::RECORDER_PATTERN);
    if strays.is_empty() {
        show(Level::Ok, "no stray recorders".into());
    } else {
        show(Level::Miss, format!("stray recorder(s) still running: {strays:?}"));
        println!("        they hold disk even after deletion — fix: dictate stop");
    }
    // Mic present in pactl sources.
    let mic_ok = mic_present(&ctx.mic());
    show(if mic_ok { Level::Ok } else { Level::Miss }, format!("mic   {}", ctx.mic()));
    if !mic_ok {
        println!("        available sources:");
        if let Ok(o) = std::process::Command::new("pactl").args(["list", "short", "sources"]).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some(name) = line.split_whitespace().nth(1) {
                    println!("          {name}");
                }
            }
        }
    }
    // The example keyd/sxhkd integration uses F19 for a press/release
    // binding. Informational; start/stop/toggle do not require it.
    let f19 = std::process::Command::new("xmodmap").args(["-pke"]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("keycode 197 = F19"))
        .unwrap_or(false);
    show(
        if f19 { Level::Ok } else { Level::Warn },
        if f19 { "keysym F19 on keycode 197".into() }
        else { "keycode 197 is not F19 — configure your own press/release binding if needed".into() },
    );
    // A held key can autorepeat and re-fire the press binding; the pidfile
    // guard survives it, but repeat-off for the key is preferable (bash
    // parses the same xset 256-bit mask, LSB-first within each byte).
    match autorepeat_197() {
        None => show(Level::Warn, "could not parse the xset autorepeat mask".into()),
        Some(true) => {
            show(Level::Warn, "autorepeat is ON for keycode 197 — run: xset -r 197".into());
            println!("        (disable repeat for this key in your input configuration)");
        }
        Some(false) => show(Level::Ok, "autorepeat off for keycode 197".into()),
    }
    rc
}

/// `Some(true)` when autorepeat is ON for keycode 197, `Some(false)` when
/// off, `None` when the `xset` mask cannot be parsed.
fn autorepeat_197() -> Option<bool> {
    let out = std::process::Command::new("xset").arg("q").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mask = crate::doctor::extract_xset_mask(&text)?;
    crate::doctor::autorepeat_enabled_for_key(&mask, 197)
}

fn mic_present(mic: &str) -> bool {
    let probe = if mic == "auto" {
        std::process::Command::new("pactl").arg("get-default-source").output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    } else {
        mic.to_string()
    };
    if probe.is_empty() {
        return false;
    }
    std::process::Command::new("pactl").args(["list", "short", "sources"]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&probe))
        .unwrap_or(false)
}

// ------------------------------------------------------------- level/meter

/// `dictate level [secs]`: record, report peak/RMS with verdict.
pub fn cmd_level(ctx: &Ctx, secs: u64) -> Result<()> {
    println!("Recording {secs}s from:");
    println!("  {}", ctx.mic());
    println!("  channel {} of {}", ctx.chan(), ctx.channels());
    println!("Speak normally, at your usual distance and volume…");
    let raw = ctx.paths.datadir.join("dictate-level-raw.wav");
    let tmp = ctx.paths.datadir.join("dictate-level.wav");
    cap_secs(ctx, &raw, secs, ctx.channels())?;
    ctx.audio().wav_to_mono16(&raw, &tmp, ctx.chan(), &ctx.log)?;
    let (peak, rms) = audio::parse_astats_peak_rms(&astats_output(&tmp));
    println!("\n  peak {:>8} dBFS", peak.map(|p| format!("{p:.1}")).unwrap_or_else(|| "?".into()));
    println!("  rms  {:>8} dBFS\n", rms.map(|r| format!("{r:.1}")).unwrap_or_else(|| "?".into()));
    print!("{}", level_verdict(peak.unwrap_or(-99.0), rms.unwrap_or(-99.0)));
    if !ctx.c("DICTATE_FILTER", "").is_empty() {
        println!("\n  after the '{}' chain it would measure:", ctx.c("DICTATE_FILTER", ""));
        println!("{}", filtered_volumedetect(&tmp, &ctx.c("DICTATE_FILTER", "")));
    }
    println!("\n  kept at {} — play it back with: mpv {}", tmp.display(), tmp.display());
    Ok(())
}

/// Bounded capture for `level`/`scan` (WAV output regardless of backend:
/// `pw-record`'s default container, or ffmpeg `pcm_s16le` into `*.wav`).
/// `channels` is explicit so `scan` records the pactl-detected width, not
/// the configured single channel (bash `CHANNELS="$nch"`).
fn cap_secs(ctx: &Ctx, dest: &Path, secs: u64, channels: u32) -> Result<()> {
    let audio = ctx.audio();
    // Recorder output joins the state log (bash `>>"$LOG" 2>&1`).
    let logfile = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ctx.log)
            .ok()
    };
    if audio.use_pw_record() {
        // `timeout -s INT` exits 124 after a successful timed capture;
        // bash `cap_secs` ignores it (`|| true`). Spawn errors still fail.
        let mut cmd = std::process::Command::new("timeout");
        cmd.args(["-s", "INT", &secs.to_string(), "pw-record",
                "--target", &ctx.mic(), "--rate", "48000",
                "--channels", &channels.to_string(), "--format", "s16"])
            .arg(dest);
        if let Some(f) = logfile() {
            cmd.stdout(std::process::Stdio::from(f));
        }
        if let Some(f) = logfile() {
            cmd.stderr(std::process::Stdio::from(f));
        }
        cmd.status()?;
        Ok(())
    } else {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error",
                "-f", "pulse", "-channels", &channels.to_string(),
                "-i", &ctx.pulse_mic(), "-t", &secs.to_string(),
                "-c:a", "pcm_s16le", "-y"])
            .arg(dest);
        if let Some(f) = logfile() {
            cmd.stderr(std::process::Stdio::from(f));
        }
        let status = cmd.status()?;
        if !audio::bounded_capture_ok(status) {
            anyhow::bail!("dictate: capture failed");
        }
        Ok(())
    }
}

fn astats_output(wav: &Path) -> String {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-i"]).arg(wav)
        .args(["-af", "astats=metadata=1", "-f", "null", "-"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default()
}

/// Speech-capture verdict thresholds (bash `level` awk block).
pub fn level_verdict(peak_db: f32, rms_db: f32) -> String {
    let mut out = String::new();
    if peak_db >= -1.0 {
        out.push_str("  CLIPPING — the signal is hitting full scale and distorting.\n  Lower the input gain.\n");
    } else if peak_db >= -12.0 {
        out.push_str("  GOOD — healthy level with headroom. Nothing to fix.\n");
    } else if peak_db >= -24.0 {
        out.push_str("  QUIET but possibly usable. Consider moving closer or adjusting gain.\n");
    } else if peak_db >= -36.0 {
        out.push_str("  LOW — transcription may drop words. Move closer, adjust gain,\n  or try DICTATE_VAD_THRESHOLD=0.35.\n");
    } else {
        out.push_str("  TOO QUIET — below about -36 dBFS peak there is not enough\n  signal above the noise floor. Move closer, or use a mic with\n  a real preamp.\n");
    }
    if rms_db < -50.0 {
        out.push_str("\n  (RMS below -50 suggests almost nothing was captured — check the\n   mic is the right device and not physically muted.)\n");
    }
    out
}

fn filtered_volumedetect(wav: &Path, filter: &str) -> String {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-i"]).arg(wav)
        .args(["-af", &format!("{filter},volumedetect"), "-f", "null", "-"])
        .output()
        .map(|o| {
            let err = String::from_utf8_lossy(&o.stderr);
            let mut s = String::new();
            for line in err.lines() {
                if line.contains("max_volume") {
                    s.push_str(&format!("    peak{}\n", line[line.find("max_volume").unwrap() + 10..].trim_end()));
                } else if line.contains("mean_volume") {
                    s.push_str(&format!("    rms{}\n", line[line.find("mean_volume").unwrap() + 11..].trim_end()));
                }
            }
            s
        })
        .unwrap_or_default()
}

/// `dictate meter`: live bar meter until Ctrl-C (raw PCM + per-chunk flush,
/// never ffmpeg astats — AVIO buffering delays output ~9s, see bash notes).
pub fn cmd_meter(ctx: &Ctx) -> Result<()> {
    println!("Live level from {} (channel {} of {}) — Ctrl-C to stop",
        ctx.mic(), ctx.chan(), ctx.channels());
    println!("  -60       -48       -36       -24       -12        0");
    let mut child = if ctx.audio().use_pw_record() {
        std::process::Command::new("pw-record")
            .args(["--target", &ctx.mic(), "--rate", "48000",
                "--channels", &ctx.channels().to_string(),
                "--format", "s16", "--container", "raw", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?
    } else {
        std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error",
                "-f", "pulse", "-channels", &ctx.channels().to_string(),
                "-i", &ctx.pulse_mic(), "-ar", "48000", "-f", "s16le", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?
    };
    use std::io::Read;
    let mut stdout = child.stdout.take().context("dictate: no recorder output")?;
    let nch = ctx.channels() as usize;
    let ch = ctx.chan() as usize;
    let frame: usize = 4800 * nch * 2; // 0.1s at 48kHz
    let mut buf = vec![0u8; frame];
    let (mut peak_hold, mut hold) = (-99.0f64, 0i32);
    loop {
        let mut got = 0;
        while got < frame {
            match stdout.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => break,
            }
        }
        if got < frame {
            break;
        }
        let samples: Vec<i16> = buf[..got].chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let chan: Vec<i16> = samples.iter().skip(ch).step_by(nch.max(1)).copied().collect();
        if chan.is_empty() {
            continue;
        }
        let pk = chan.iter().map(|v| v.unsigned_abs() as f64).fold(0.0f64, f64::max).max(1.0);
        let rms = (chan.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / chan.len() as f64).sqrt().max(1e-9);
        let db = 20.0 * (rms / 32768.0).log10();
        let dbp = 20.0 * (pk / 32768.0).log10();
        hold -= 1;
        if dbp > peak_hold || hold <= 0 {
            peak_hold = dbp;
            hold = 20;
        }
        let d = db.clamp(-60.0, 0.0);
        let n = ((d + 60.0) / 60.0 * 50.0) as usize;
        let mark = if dbp > -1.0 { "CLIP" } else if dbp > -12.0 { "hot" } else if dbp > -30.0 { "ok" } else { "quiet" };
        print!("\r{:<50} {:6.1} dB  peak {:6.1}  {:<5}", "#".repeat(n), db, peak_hold, mark);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    println!();
    let _ = child.kill();
    Ok(())
}

// ------------------------------------------------------------------- serve

/// Foreground resident whisper-server with warmup (for a systemd user unit).
pub fn cmd_serve(ctx: &Ctx) -> Result<()> {
    let server = ctx.server();
    let port = server.rsplit(':').next().unwrap_or("8127").to_string();
    let warm = ctx.paths.datadir.join("dictate-warm.wav");
    let _ = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono",
            "-t", "1", "-c:a", "pcm_s16le", "-y"])
        .arg(&warm).status();
    // Warmup prober in background; exec server in foreground.
    let server = ctx.server();
    let lang = ctx.c("DICTATE_LANG", "auto");
    let prompt = ctx.c("DICTATE_PROMPT", "");
    let warm2 = warm.clone();
    std::thread::spawn(move || {
        for _ in 0..240 {
            let probe = std::process::Command::new("curl")
                .args(["-sf", "--max-time", "2", "-o", "/dev/null",
                    &format!("{server}/inference"),
                    "-F", &format!("file=@{}", warm2.display()),
                    "-F", "response_format=json",
                    "--form-string", &format!("language={lang}"),
                    "--form-string", &format!("prompt={prompt}")])
                .status().map(|s| s.success()).unwrap_or(false);
            if probe {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        eprintln!("server warm");
    });
    let status = std::process::Command::new("whisper-server")
        .args(["-m", &ctx.model(), "--host", "127.0.0.1", "--port", &port,
            "-t", &ctx.c("DICTATE_THREADS", "8"), "-nt", "-sns",
            "-l", &ctx.c("DICTATE_LANG", "auto"),
            "--prompt", &ctx.c("DICTATE_PROMPT", "")])
        .status()?;
    if !status.success() {
        anyhow::bail!("dictate: whisper-server exited");
    }
    Ok(())
}

// -------------------------------------------------------------------- scan

/// `dictate scan [secs]`: per-channel levels to find the mic input.
pub fn cmd_scan(ctx: &Ctx, secs: u64) -> Result<()> {
    let nch = pactl_source_channels(&ctx.mic()).unwrap_or_else(|| ctx.channels());
    println!("Source : {}", ctx.mic());
    println!("Channels: {nch}\n");
    println!("SPEAK NORMALLY for {secs}s — talk continuously so quiet channels stand out.");
    let tmp = ctx.paths.datadir.join("dictate-scan.wav");
    // Record the detected width, not the configured channel: a 10-input
    // interface must actually be captured at 10 channels (bash sets
    // `CHANNELS="$nch"` before `cap_secs`).
    cap_secs(ctx, &tmp, secs, nch)?;
    println!();
    for c in 0..nch {
        let (peak, rms) = channel_astats(&tmp, c);
        let (pk, rm) = (peak.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-inf".into()),
            rms.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-inf".into()));
        let note = match peak.unwrap_or(f32::NEG_INFINITY) {
            v if v <= -85.0 => "  (nothing — dead input or gain at zero)",
            v if v < -30.0 => "  (live preamp, no speech)",
            _ => "  <-- SIGNAL, use this one",
        };
        let note = if peak.is_none() { "  (digital zero — channel not active)" } else { note };
        println!("  ch{c:<2} peak {pk:>8}  rms {rm:>8}{note}");
    }
    println!("\n  Use the loudest channel with:  DICTATE_CHANNELS={nch} DICTATE_CHAN=<n> dictate level");
    println!("  kept at {}", tmp.display());
    Ok(())
}

fn pactl_source_channels(mic: &str) -> Option<u32> {
    let o = std::process::Command::new("pactl").args(["list", "sources"]).output().ok()?;
    pactl_channels_from(mic, &String::from_utf8_lossy(&o.stdout))
}

fn channel_astats(wav: &Path, chan: u32) -> (Option<f32>, Option<f32>) {
    let o = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-i"]).arg(wav)
        .args(["-af", &format!("pan=mono|c0=c{chan},astats=metadata=1"), "-f", "null", "-"])
        .output();
    match o {
        Ok(o) => audio::parse_astats_peak_rms(&String::from_utf8_lossy(&o.stderr)),
        Err(_) => (None, None),
    }
}

// -------------------------------------------------------------------- bench

/// `dictate bench [wav]`: every local whisper model + parakeet over one clip.
/// Warmed first (cold Vulkan shader compile would swamp timing).
pub fn cmd_bench(ctx: &Ctx, wav: Option<&str>) -> Result<()> {
    let wav = wav.map(PathBuf::from).unwrap_or_else(|| ctx.paths.wav.clone());
    if !wav.exists() {
        anyhow::bail!("no audio at {} — run 'dictate level' first, or pass a wav", wav.display());
    }
    println!("audio: {}", wav.display());
    let vols = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-i"]).arg(&wav)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output().map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();
    for line in vols.lines().filter(|l| l.contains("max_volume") || l.contains("mean_volume")) {
        println!("  {line}");
    }
    println!();
    let model_dir = Path::new(&ctx.model()).parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let lang = ctx.c("DICTATE_LANG", "auto");
    let prompt = ctx.c("DICTATE_PROMPT", "");
    if let Ok(dir) = std::fs::read_dir(&model_dir) {
        let mut models: Vec<PathBuf> = dir.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                n.starts_with("ggml-") && n.ends_with(".bin") && !n.contains("silero") && !n.contains("parakeet")
            }).collect();
        models.sort();
        for m in models {
            let name = m.file_stem().and_then(|n| n.to_str()).unwrap_or("?")
                .trim_start_matches("ggml-").to_string();
            // Warm.
            let _ = std::process::Command::new("whisper-cli")
                .args(["-m"]).arg(&m).args(["-f"]).arg(&wav)
                .args(["-l", &lang, "-nt", "-np", "-sns"]).output();
            let s = now_unix();
            let out = std::process::Command::new("whisper-cli")
                .args(["-m"]).arg(&m).args(["-f"]).arg(&wav)
                .args(["-l", &lang, "-nt", "-np", "-sns", "--prompt", &prompt])
                .output();
            if let Ok(o) = out {
                if o.status.success() {
                    let t: String = String::from_utf8_lossy(&o.stdout)
                        .chars().flat_map(|c| if c.is_whitespace() { Some(' ') } else { Some(c) }).collect();
                    println!("  {name:<20} {:5.2}s  {}", now_unix() - s, t.trim());
                }
            }
        }
    }
    // Same default model path as stop/doctor: an unset key still benches
    // the standard model instead of silently skipping parakeet.
    let pm = ctx.parakeet_model();
    if Path::new(&pm).exists() {
        let pc = ParakeetConfig { model: pm, threads: ctx.c("DICTATE_THREADS", "8").parse().unwrap_or(8), bin: "parakeet-cli".into() };
        let _ = transcribe::transcribe_parakeet(&wav, &pc, &ctx.log);
        let s = now_unix();
        let txt = transcribe::transcribe_parakeet(&wav, &pc, &ctx.log).unwrap_or_default();
        let t: String = txt.chars().flat_map(|c| if c.is_whitespace() { Some(' ') } else { Some(c) }).collect();
        println!("  {:<20} {:5.2}s  {}", "parakeet-tdt-v3", now_unix() - s, t.trim());
    }
    println!("\n  Note: these run through whisper-cli, but dictation runs through");
    println!("  whisper-cli and whisper-server can differ on the same audio.");
    Ok(())
}

/// `dictate segbench <wav>...`: whisper on each whole clip against whisper on
/// the segments the daemon would cut at its pauses (`segment::simulate_cuts`),
/// without and with the previous text as context. Word errors are counted
/// against the clip's reference: `transcript.elevenlabs.txt` beside it, else
/// `transcript.corrected.txt`, else `transcript.txt`, else `<clip>.txt`. Prints one TSV row per clip
/// and the totals; `-v` also prints the texts.
pub fn cmd_segbench(ctx: &Ctx, wavs: &[PathBuf], verbose: bool) -> Result<()> {
    use crate::correct::{edit_distance, words};
    let pass = FinalPass::new(ctx, "bench");
    let level: f32 = ctx.c("DICTATE_EARLY_LEVEL", "-30").parse().unwrap_or(-30.0);
    let pause = ctx.c("DICTATE_EARLY_PAUSE_MS", "500").parse::<usize>().unwrap_or(500) * wav::RATE as usize / 1000;
    let min = ctx.min_segment();
    let mut total = [0usize; 4];
    println!("clip\tsecs\tsegments\tref_words\twhole\tsegmented\tsegmented+context");
    for path in wavs {
        let samples = wav::read_mono16(path).with_context(|| format!("reading {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let reference = ["transcript.elevenlabs.txt", "transcript.corrected.txt", "transcript.txt"].iter()
            .map(|n| dir.join(n))
            .chain([path.with_extension("txt")])
            .find_map(|p| std::fs::read_to_string(p).ok());
        let Some(reference) = reference else {
            eprintln!("{}: no reference transcript, skipped", path.display());
            continue;
        };
        let mut bounds = vec![0];
        bounds.extend(crate::segment::simulate_cuts(&samples, level, pause, min));
        let tail_speech = wav::peak_db(&samples[*bounds.last().unwrap()..]) >= level;
        if tail_speech || bounds.len() == 1 {
            bounds.push(samples.len());
        }
        let whole = text::clean_whisper_text(&pass.whisper(&samples)?);
        let (mut plain, mut chained) = (String::new(), String::new());
        for w in bounds.windows(2) {
            let seg = &samples[w[0]..w[1]];
            plain = crate::segment::join(&plain, &text::clean_whisper_text(&pass.recognize(seg, None)?));
            let next = text::clean_whisper_text(&pass.recognize(seg, Some(&chained))?);
            chained = crate::segment::join(&chained, &next);
        }
        let r = words(&reference);
        let err = |t: &str| edit_distance(&r, &words(t));
        let (e0, e1, e2) = (err(&whole), err(&plain), err(&chained));
        let name = if path.file_name().is_some_and(|n| n == "audio.wav") { dir } else { path.as_path() };
        println!("{}\t{:.1}\t{}\t{}\t{e0}\t{e1}\t{e2}", name.file_name().unwrap_or_default().to_string_lossy(),
            samples.len() as f64 / wav::RATE as f64, bounds.len() - 1, r.len());
        if verbose {
            println!("  ref:     {}\n  whole:   {whole}\n  seg:     {plain}\n  seg+ctx: {chained}", reference.trim());
        }
        for (t, v) in total.iter_mut().zip([r.len(), e0, e1, e2]) {
            *t += v;
        }
    }
    let pct = |e: usize| 100.0 * e as f64 / total[0].max(1) as f64;
    println!("total\t\t\t{}\t{} ({:.2}%)\t{} ({:.2}%)\t{} ({:.2}%)", total[0],
        total[1], pct(total[1]), total[2], pct(total[2]), total[3], pct(total[3]));
    Ok(())
}

// ------------------------------------------------------- preview worker

/// Hidden `preview-worker` subprocess: live transcript while recording.
/// Ports `preview_loop` + gate wiring. Sequential requests only; stops when
/// the pidfile disappears or after 180s. Timing TSV rewritten at startup.
pub fn run_preview_worker(ctx: &Ctx) -> Result<()> {
    use crate::preview::{format_timing_row, EnergyGate, TIMING_HEADER};
    use std::io::Write;
    let _ = std::fs::write(&ctx.paths.preview_timing, TIMING_HEADER);
    let interval: f64 = ctx.c("DICTATE_STREAM_INTERVAL", "0").parse().unwrap_or(0.0);
    let pause: f64 = ctx.c("DICTATE_STREAM_PAUSE", "0.005").parse().unwrap_or(0.005);
    let gate_enabled = ctx.c("DICTATE_STREAM_GATE", "1") == "1";
    let gate_db: f32 = ctx.c("DICTATE_STREAM_GATE_DB", "-48").parse().unwrap_or(-48.0);
    let mut gate = EnergyGate::new(ctx.channels(), ctx.chan(), gate_db)
        .unwrap_or_else(|_| EnergyGate::new(1, 0, gate_db).unwrap());
    let mut file_offset: u64 = 0;
    let mut last = String::new();
    let mut previous = 0.0f64;
    let deadline = now_unix() + 180.0;
    let mut tick_started = now_unix();
    let server = ctx.server();
    let lang = ctx.c("DICTATE_LANG", "auto");
    let prompt = ctx.c("DICTATE_PROMPT", "");
    let style = ctx.c("DICTATE_STYLE", "0") == "1";
    let max_ms = ctx.max_secs() * 1000;

    let row = |started: f64, prep: f64, req: f64, fin: f64, gap: f64, updated: bool, status: &str| {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&ctx.paths.preview_timing) {
            let _ = f.write_all(format_timing_row(started, prep, req, fin, gap, updated, status).as_bytes());
        }
    };

    while now_unix() < deadline {
        // Cadence: optional start-to-start target, else pause-after-work.
        let delay = if interval != 0.0 {
            (interval - (now_unix() - tick_started)).max(pause).max(0.0)
        } else {
            pause.max(0.0)
        };
        if delay > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(delay));
        }
        if !ctx.paths.pidfile.exists() {
            break;
        }
        // Energy gate with its own 40ms quiet poll (preview_gate.py parity):
        // silence waits here without consuming the start-to-start interval.
        if gate_enabled && !wait_for_voice(ctx, &mut gate, &mut file_offset, deadline) {
            break;
        }
        if !ctx.paths.pidfile.exists() {
            break;
        }
        tick_started = now_unix();
        if std::fs::metadata(&ctx.paths.raw).map(|m| m.len() == 0).unwrap_or(true) {
            continue;
        }
        // Copy first: the recorder keeps appending; ffmpeg must not read a
        // torn final frame from the growing file.
        let snap = ctx.paths.datadir.join("dictate-preview.pcm");
        let wav = ctx.paths.datadir.join("dictate-preview.wav");
        let t0 = now_unix();
        let prepared_ok = std::fs::copy(&ctx.paths.raw, &snap).is_ok()
            && ctx.audio().mono16_with_channels(&snap, &wav, ctx.chan(), ctx.channels(), &ctx.log).is_ok();
        let t1 = now_unix();
        if !prepared_ok {
            let gap = if previous > 0.0 { (t0 - previous) * 1000.0 } else { 0.0 };
            row(t0, (t1 - t0) * 1000.0, 0.0, 0.0, gap, false, "prepare_error");
            previous = t0;
            continue;
        }
        let out = std::process::Command::new("curl")
            .args(["-sf", "--connect-timeout", "1", "--max-time", "5",
                &format!("{server}/inference"),
                "-F", &format!("file=@{}", wav.display()),
                "-F", "response_format=json",
                "--form-string", &format!("language={lang}"),
                "--form-string", &format!("prompt={prompt}")])
            .output();
        let t2 = now_unix();
        let mut txt = match out {
            Ok(o) if o.status.success() => {
                transcribe::parse_inference_text(&String::from_utf8_lossy(&o.stdout))
                    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default()
            }
            _ => {
                let gap = if previous > 0.0 { (t0 - previous) * 1000.0 } else { 0.0 };
                row(t0, (t1 - t0) * 1000.0, (t2 - t1) * 1000.0, 0.0, gap, false, "request_error");
                previous = t0;
                continue;
            }
        };
        txt = txt.trim().to_string();
        if !txt.is_empty() {
            txt = text::restyle(&txt, style, &prompt);
        }
        let t3 = now_unix();
        let mut updated = false;
        if !txt.is_empty() && txt != last {
            notify(ctx, &format!("\u{1f3a4} {txt}"), "", max_ms);
            last = txt;
            updated = true;
        }
        let gap = if previous > 0.0 { (t0 - previous) * 1000.0 } else { 0.0 };
        row(t0, (t1 - t0) * 1000.0, (t2 - t1) * 1000.0, (t3 - t2) * 1000.0, gap, updated, "ok");
        previous = t0;
    }
    Ok(())
}

/// Block (40ms polls) until the energy gate fires, the recording ends, or
/// the deadline passes. Truncation (recorder restarted under us) resets both
/// the file offset and the gate's voice/settle state, so a pending settle
/// can never fire on the new file. Mirrors `preview_gate.py`'s inner loop.
fn wait_for_voice(
    ctx: &Ctx,
    gate: &mut crate::preview::EnergyGate,
    file_offset: &mut u64,
    deadline: f64,
) -> bool {
    loop {
        if !ctx.paths.pidfile.exists() || now_unix() >= deadline {
            return false;
        }
        let end = std::fs::metadata(&ctx.paths.raw).map(|m| m.len()).unwrap_or(0);
        if end < *file_offset {
            *file_offset = 0;
            gate.reset();
        }
        let frame_bytes = (ctx.channels() as u64) * 2;
        // Bound catch-up work to the last 2s (96000 frames at 48kHz).
        let start = (*file_offset).max(end.saturating_sub(96000 * frame_bytes));
        let samples = bytes_to_i16(&read_range(&ctx.paths.raw, start, end));
        *file_offset = end;
        if gate.scan(&samples, now_unix()) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(0.04));
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    if end <= start {
        return vec![];
    }
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = vec![0u8; (end - start) as usize];
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => break,
        }
    }
    buf.truncate(got);
    buf
}

fn bytes_to_i16(data: &[u8]) -> Vec<i16> {
    data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

// Test seam for pactl parsing (same scan as `pactl_source_channels`).
fn pactl_channels_from(mic: &str, text: &str) -> Option<u32> {
    let mut cur = false;
    for line in text.lines() {
        if line.split_whitespace().next() == Some("Name:") {
            cur = line.contains(mic);
        }
        if cur && line.contains("Sample Specification:") {
            for tok in line.split_whitespace() {
                if let Some(n) = tok.strip_suffix("ch") {
                    if let Ok(v) = n.parse() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_silence_gates() {
        assert!(is_too_short(0.49));
        assert!(!is_too_short(0.5));
        assert!(is_silence(Some(-50.0), -45.0));
        assert!(!is_silence(Some(-40.0), -45.0));
        assert!(!is_silence(None, -45.0));
    }

    #[test]
    fn level_verdict_thresholds() {
        assert!(level_verdict(0.0, -20.0).contains("CLIPPING"));
        assert!(level_verdict(-6.0, -20.0).contains("GOOD"));
        assert!(level_verdict(-20.0, -30.0).contains("QUIET"));
        assert!(level_verdict(-30.0, -40.0).contains("LOW"));
        assert!(level_verdict(-50.0, -60.0).contains("TOO QUIET"));
        assert!(level_verdict(-6.0, -60.0).contains("RMS below -50"));
    }

    #[test]
    fn pactl_channel_parse() {
        let out = "Source #1\n\tName: my-mic\n\tSample Specification: s16le 10ch 48000Hz\n";
        // Name match required; fixture name differs from probe.
        assert_eq!(pactl_channels_from("my-mic", out), Some(10));
        assert_eq!(pactl_channels_from("other", out), None);
    }

    #[test]
    fn uid_and_overlay_path_sane() {
        assert!(!uid_string().is_empty());
        assert!(overlay_state_path(Path::new("/tmp")).ends_with("state.json"));
    }

    #[test]
    fn history_bounds_come_from_merged_config() {
        use crate::config::{Env, load_from_env};
        use crate::paths::Paths;
        let tmp = tempfile::tempdir().unwrap();
        // Config-file values (not process env) must drive prune bounds.
        let mut env = Env::with_vars([
            ("DICTATE_HISTORY_DAYS".into(), "7".into()),
            ("DICTATE_HISTORY_MAX".into(), "11".into()),
        ]);
        env.xdg_config_home = Some(tmp.path().to_string_lossy().into_owned());
        env.home = Some(tmp.path().to_string_lossy().into_owned());
        let cfg = load_from_env(&env).unwrap();
        let paths = Paths {
            rundir: tmp.path().into(),
            datadir: tmp.path().join("d"),
            state_dir: tmp.path().join("s"),
            history_dir: tmp.path().join("h"),
            history_enabled: true,
            pidfile: tmp.path().join("p"),
            startfile: tmp.path().join("st"),
            wav: tmp.path().join("w"),
            wav_proc: tmp.path().join("wp"),
            raw: tmp.path().join("r"),
            log: tmp.path().join("l"),
            release_timing: tmp.path().join("rt"),
            preview_timing: tmp.path().join("pt"),
            preview_pidfile: tmp.path().join("pp"),
            modefile: tmp.path().join("m"),
        };
        let ctx = Ctx::new(cfg, paths, true).unwrap();
        assert_eq!(ctx.history.max_days, 7);
        assert_eq!(ctx.history.max_entries, 11);
        // Unusable storage degrades to disabled, never fatal.
        let empty = crate::config::Env::default();
        let ctx2 = Ctx::new(
            load_from_env(&empty).unwrap(),
            Paths::resolve(&empty).unwrap(),
            false,
        )
        .unwrap();
        assert!(!ctx2.history.enabled);
    }
}
