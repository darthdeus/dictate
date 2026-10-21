//! Resident streaming engine. `dictate daemon` keeps the Nemotron streaming
//! model loaded and warm (the first load compiles Vulkan shaders, which takes
//! seconds) and owns the mic while recording, publishing the committed text
//! and the still-revisable tentative tail to the overlay as you speak.
//! Committed text only ever grows, so words on screen never flip back.
//!
//! `start`/`stop` reach it over a unix socket and fall back to the
//! per-process pipeline (whisper snapshot preview) when it is not running.
//! On `stop` it writes the clip to `Paths::wav`, and the `stop` process runs
//! the final pass (whisper, optional correction) and delivers, since it has
//! the X session that typing and the clipboard need.
//!
//! Protocol, one request line and one reply line:
//!   start  -> ok | busy | error <why>
//!   stop   -> ok <seconds> | idle | error <why>
//!   status -> recording | idle

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use transcribe_cpp::{Model, ParakeetStreamOptions, RunOptions, Session, StreamExtension, StreamOptions};

use crate::overlay::{JsonFileOverlay, OverlayBackend, OverlayMessage, Phase};
use crate::paths::Paths;
use crate::pipeline::{append_log, now_unix, Ctx};
use crate::wav::{self, RATE};

/// Audio per `feed`: 160 ms, the model's native step at right context 1.
const CHUNK_MS: usize = 160;

pub fn socket_path(paths: &Paths) -> PathBuf {
    paths.rundir.join("dictate-daemon.sock")
}

/// Sends one request; `None` when no daemon is listening.
pub fn request(paths: &Paths, line: &str) -> Option<String> {
    let mut s = UnixStream::connect(socket_path(paths)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    writeln!(s, "{line}").ok()?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).ok()?;
    Some(reply.trim().to_string())
}

fn stream_model(ctx: &Ctx) -> String {
    ctx.cfg.get("DICTATE_STREAM_MODEL").map(str::to_string).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share/transcribe/nemotron-speech-streaming-en-0.6b-Q8_0.gguf")
    })
}

/// Lookahead of the streaming model: 0, 1, 6 or 13 frames (80, 160, 560,
/// 1120 ms). More lookahead is a little more accurate and that much slower.
fn right_context(ctx: &Ctx) -> i32 {
    ctx.cfg.get_or("DICTATE_STREAM_RIGHT", "1").parse().unwrap_or(1)
}

fn stream_options(right: i32) -> StreamOptions {
    StreamOptions {
        family: Some(StreamExtension::ParakeetStream(ParakeetStreamOptions { att_context_right: Some(right) })),
        ..Default::default()
    }
}

struct Recording {
    recorder: u32,
    stop: Arc<AtomicBool>,
    wav: PathBuf,
    thread: JoinHandle<(Session, Result<Vec<i16>>)>,
}

/// `dictate daemon`: serve until killed. `load_ctx` is called per recording,
/// so config edits apply to the next dictation without a restart.
pub fn run(load_ctx: &dyn Fn() -> Result<Ctx>) -> Result<()> {
    let ctx = load_ctx()?;
    let t = Instant::now();
    let model = Model::load(stream_model(&ctx)).with_context(|| format!("loading {}", stream_model(&ctx)))?;
    let mut session = Some(model.session()?);
    warm_up(session.as_mut().unwrap(), right_context(&ctx))?;
    eprintln!("dictate daemon: {} on {} ready in {:.1}s", model.arch(), model.backend(), t.elapsed().as_secs_f64());

    let sock = socket_path(&ctx.paths);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    let mut recording: Option<Recording> = None;
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let mut line = String::new();
        if BufReader::new(&conn).read_line(&mut line).is_err() {
            continue;
        }
        let reply = match line.trim() {
            "start" if recording.is_some() => "busy".to_string(),
            "start" => {
                let s = session.take().expect("the session is home while idle");
                match load_ctx() {
                    Err(e) => {
                        session = Some(s);
                        format!("error {e:#}")
                    }
                    Ok(c) => match start(c, s) {
                        Ok(r) => {
                            recording = Some(r);
                            "ok".to_string()
                        }
                        Err((s, e)) => {
                            session = Some(s);
                            format!("error {e:#}")
                        }
                    },
                }
            }
            "stop" => match recording.take() {
                None => "idle".to_string(),
                Some(r) => {
                    let (s, reply) = stop(r);
                    session = Some(s);
                    reply
                }
            },
            "status" => if recording.is_some() { "recording" } else { "idle" }.to_string(),
            other => format!("error unknown request {other:?}"),
        };
        let _ = writeln!(conn, "{reply}");
    }
    Ok(())
}

/// Runs a short silent stream so the GPU pipelines for streaming and for
/// `finalize` compile now rather than during the first dictation.
fn warm_up(session: &mut Session, right: i32) -> Result<()> {
    let mut stream = session.stream(&RunOptions::default(), &stream_options(right))?;
    let chunk = vec![0f32; RATE as usize * CHUNK_MS / 1000];
    for _ in 0..12 {
        stream.feed(&chunk)?;
    }
    stream.finalize()?;
    Ok(())
}

/// Spawns the recorder and the capture thread. On failure the session comes
/// back with the error, so the daemon keeps serving.
fn start(ctx: Ctx, session: Session) -> std::result::Result<Recording, (Session, anyhow::Error)> {
    let channels = ctx.channels() as usize;
    let chan = ctx.chan() as usize;
    if chan >= channels {
        return Err((session, anyhow::anyhow!("DICTATE_CHAN {chan} is not below DICTATE_CHANNELS {channels}")));
    }
    let mut cmd = Command::new("pw-record");
    let mic = ctx.mic();
    if mic != "auto" {
        cmd.args(["--target", &mic]);
    }
    cmd.args(["--rate", &RATE.to_string(), "--channels", &channels.to_string()])
        .args(["--format", "s16", "--container", "raw", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log_stdio(&ctx.log));
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err((session, anyhow::anyhow!("cannot run pw-record: {e}"))),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let recorder = child.id();
    let wav = ctx.paths.wav.clone();
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut session = session;
        let res = capture(&ctx, &mut session, child, channels, chan, &flag);
        if let Err(e) = &res {
            append_log(&ctx.log, &format!("dictate daemon: capture failed: {e:#}"));
        }
        (session, res)
    });
    Ok(Recording { recorder, stop, wav, thread })
}

fn stop(r: Recording) -> (Session, String) {
    // No more preview updates: the stop process takes over the overlay.
    r.stop.store(true, Ordering::SeqCst);
    // SIGINT makes pw-record flush and exit; the reader then sees EOF.
    crate::state::send_int(r.recorder);
    let (session, res) = r.thread.join().expect("dictate daemon: capture thread panicked");
    let reply = match res.and_then(|audio| {
        wav::write_mono16(&r.wav, &audio)?;
        Ok(audio.len() as f64 / RATE as f64)
    }) {
        Ok(secs) => format!("ok {secs:.2}"),
        Err(e) => format!("error {e:#}"),
    };
    (session, reply)
}

/// Reads the recorder until EOF (stop) or `DICTATE_MAX_SECS`, feeding the
/// model 160 ms at a time and publishing each change of the preview.
fn capture(ctx: &Ctx, session: &mut Session, mut child: Child, channels: usize, chan: usize, stop: &AtomicBool)
    -> Result<Vec<i16>> {
    let mut out = child.stdout.take().context("recorder has no stdout")?;
    let frame = channels * 2;
    let chunk_bytes = RATE as usize * CHUNK_MS / 1000 * frame;
    let max_samples = ctx.max_secs() as usize * RATE as usize;
    // Selected channel as mono PCM, tailed by the overlay's level meter.
    let level_path = ctx.paths.rundir.join("dictate-stream.pcm");
    let mut level = std::fs::File::create(&level_path).ok();
    let overlay = Publisher { ctx, level_path: &level_path, stop };
    overlay.show("", "");

    let mut stream = session.stream(&RunOptions::default(), &stream_options(right_context(ctx)))?;
    let mut audio: Vec<i16> = Vec::with_capacity(RATE as usize * 60);
    let mut buf = vec![0u8; chunk_bytes];
    let mut eof = false;
    while !eof && audio.len() < max_samples {
        let mut filled = 0;
        while filled < buf.len() {
            match out.read(&mut buf[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mono: Vec<i16> = buf[..filled - filled % frame]
            .chunks_exact(frame)
            .map(|f| i16::from_le_bytes([f[2 * chan], f[2 * chan + 1]]))
            .collect();
        if mono.is_empty() {
            continue;
        }
        if let Some(f) = level.as_mut() {
            let bytes: Vec<u8> = mono.iter().flat_map(|s| s.to_le_bytes()).collect();
            let _ = f.write_all(&bytes);
        }
        audio.extend_from_slice(&mono);
        let pcm: Vec<f32> = mono.iter().map(|&s| s as f32 / 32768.0).collect();
        let update = stream.feed(&pcm)?;
        if update.committed_changed || update.tentative_changed {
            let t = stream.text();
            overlay.show(&t.committed, &t.tentative);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let t = Instant::now();
    stream.finalize()?;
    append_log(&ctx.log, &format!("{} daemon: {:.1}s captured, finalize {:.0} ms",
        crate::history::local_iso_now(), audio.len() as f64 / RATE as f64, t.elapsed().as_secs_f64() * 1000.0));
    drop(level);
    let _ = std::fs::remove_file(&level_path);
    Ok(audio)
}

struct Publisher<'a> {
    ctx: &'a Ctx,
    level_path: &'a Path,
    stop: &'a AtomicBool,
}

impl Publisher<'_> {
    fn show(&self, committed: &str, tentative: &str) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let max = self.ctx.max_secs() as f64;
        let msg = OverlayMessage {
            phase: Phase::Recording,
            label: "Listening".into(),
            text: committed.trim_start().to_string(),
            engine: live_label(self.ctx),
            tentative: if committed.trim().is_empty() { tentative.trim_start() } else { tentative }.to_string(),
            audio: self.level_path.display().to_string(),
            channels: 1,
            channel: 0,
            duration: max,
            expires: now_unix() + max,
        };
        let _ = JsonFileOverlay { state_path: self.ctx.overlay_path.clone() }.show(&msg);
    }
}

fn live_label(ctx: &Ctx) -> String {
    let model = stream_model(ctx);
    let stem = Path::new(&model).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let lookahead = (right_context(ctx) + 1) * 80;
    format!("Rust \u{b7} live: {} ({lookahead} ms lookahead)", stem.replace("-speech-streaming", ""))
}

fn log_stdio(log: &Path) -> Stdio {
    std::fs::OpenOptions::new().create(true).append(true).open(log).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}
