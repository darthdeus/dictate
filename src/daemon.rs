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
//! While you pause mid-recording it already runs that final pass on the
//! audio so far (`EarlyWorker`). If the pause was the end, `stop` replies
//! `ok <seconds> early` and leaves the result at `early_pass_path`, so the
//! stop process only has to paste.
//!
//! Protocol, one request line and one reply line:
//!   start  -> ok | busy | error <why>
//!   stop   -> ok <seconds> [early] | idle | error <why>
//!   status -> recording | idle

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use transcribe_cpp::{Model, ParakeetStreamOptions, RunOptions, Session, StreamExtension, StreamOptions};

use crate::overlay::{JsonFileOverlay, OverlayBackend, OverlayMessage, Phase};
use crate::paths::Paths;
use crate::pipeline::{append_log, early_pass_path, now_unix, Ctx, EarlyPass, FinalPass};
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
    /// `Paths::pidfile`, holding the recorder's pid while recording: the
    /// polybar clock reads its age to show (and flag stuck) recordings.
    pidfile: PathBuf,
    stop: Arc<AtomicBool>,
    wav: PathBuf,
    early_path: PathBuf,
    thread: JoinHandle<(Session, Result<Captured>)>,
}

/// The clip, and the pause pass that covers it if one finished.
type Captured = (Vec<i16>, Option<EarlyPass>);

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
    let pidfile = ctx.paths.pidfile.clone();
    let _ = std::fs::write(&pidfile, recorder.to_string());
    let wav = ctx.paths.wav.clone();
    let early_path = early_pass_path(&ctx.paths);
    let _ = std::fs::remove_file(&early_path);
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut session = session;
        let res = capture(&ctx, &mut session, child, channels, chan, &flag);
        if let Err(e) = &res {
            append_log(&ctx.log, &format!("dictate daemon: capture failed: {e:#}"));
        }
        (session, res)
    });
    Ok(Recording { recorder, pidfile, stop, wav, early_path, thread })
}

fn stop(r: Recording) -> (Session, String) {
    // No more preview updates: the stop process takes over the overlay.
    r.stop.store(true, Ordering::SeqCst);
    // SIGINT makes pw-record flush and exit; the reader then sees EOF.
    crate::state::send_int(r.recorder);
    let (session, res) = r.thread.join().expect("dictate daemon: capture thread panicked");
    let _ = std::fs::remove_file(&r.pidfile);
    let reply = match res.and_then(|(audio, early)| {
        wav::write_mono16(&r.wav, &audio)?;
        let early = match early {
            Some(e) => std::fs::write(&r.early_path, serde_json::to_vec(&e)?).is_ok(),
            None => false,
        };
        Ok((audio.len() as f64 / RATE as f64, early))
    }) {
        Ok((secs, true)) => format!("ok {secs:.2} early"),
        Ok((secs, false)) => format!("ok {secs:.2}"),
        Err(e) => format!("error {e:#}"),
    };
    (session, reply)
}

/// Reads the recorder until EOF (stop) or `DICTATE_MAX_SECS`, feeding the
/// model 160 ms at a time and publishing each change of the preview.
fn capture(ctx: &Ctx, session: &mut Session, mut child: Child, channels: usize, chan: usize, stop: &AtomicBool)
    -> Result<Captured> {
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
    let mut early = EarlyWorker::new(ctx);
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
        let t = stream.text();
        let early_changed = early.heard(&audio, &t.committed, &t.tentative);
        let pcm: Vec<f32> = mono.iter().map(|&s| s as f32 / 32768.0).collect();
        let update = stream.feed(&pcm)?;
        if update.committed_changed || update.tentative_changed || early_changed {
            let t = stream.text();
            let (committed, tentative) = early.preview(&t.committed, &t.tentative);
            overlay.show(&committed, &tentative);
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
    let early = early.finish(&ctx.log, &audio);
    Ok((audio, early))
}

/// Runs the final pass in the background whenever the speaker pauses, on
/// the audio so far. A pass whose snapshot covers all the speech is used at
/// stop; one overtaken by new speech is dropped, and the next pause starts
/// another. Correction is skipped when speech resumed during whisper, which
/// keeps Groq requests (rate-limited per minute) to about one per pause.
struct EarlyWorker {
    pass: Option<Arc<FinalPass>>,
    /// A chunk whose peak is at or above this (dBFS) is speech.
    level: f32,
    pause_samples: usize,
    /// End of the last speech chunk, shared with the running pass.
    speech_end: Arc<AtomicUsize>,
    running: Option<(usize, usize, JoinHandle<Option<EarlyPass>>)>,
    /// A finished pass, the audio length it covered and how many words of
    /// the live text it replaces in the preview.
    done: Option<(usize, usize, EarlyPass)>,
    spoke: bool,
}

impl EarlyWorker {
    fn new(ctx: &Ctx) -> Self {
        let on = ctx.c("DICTATE_EARLY", "1") == "1";
        Self {
            pass: on.then(|| Arc::new(FinalPass::new(ctx))),
            level: ctx.c("DICTATE_EARLY_LEVEL", "-30").parse().unwrap_or(-30.0),
            pause_samples: ctx.c("DICTATE_EARLY_PAUSE_MS", "500").parse::<usize>().unwrap_or(500) * RATE as usize / 1000,
            speech_end: Arc::new(AtomicUsize::new(0)),
            running: None,
            done: None,
            spoke: false,
        }
    }

    /// Called after each chunk is appended to `audio`, with the live text so
    /// far; true when a pass just finished (the preview changes).
    fn heard(&mut self, audio: &[i16], committed: &str, tentative: &str) -> bool {
        let Some(pass) = self.pass.clone() else { return false };
        let chunk = RATE as usize * CHUNK_MS / 1000;
        let tail = &audio[audio.len().saturating_sub(chunk)..];
        if wav::peak_db(tail) >= self.level {
            self.spoke = true;
            self.speech_end.store(audio.len(), Ordering::SeqCst);
            return self.collect();
        }
        let finished = self.collect();
        let speech_end = self.speech_end.load(Ordering::SeqCst);
        let covered = self.done.as_ref().map_or(0, |d| d.0).max(self.running.as_ref().map_or(0, |r| r.0));
        if !self.spoke || self.running.is_some() || covered >= speech_end || audio.len() - speech_end < self.pause_samples {
            return finished;
        }
        let words = committed.split_whitespace().count() + tentative.split_whitespace().count();
        let (samples, speech) = (audio.to_vec(), self.speech_end.clone());
        let len = samples.len();
        self.running = Some((len, words, std::thread::spawn(move || {
            let raw = pass.whisper(&samples).ok()?;
            let clean = crate::text::clean_whisper_text(&raw);
            let corrected = if clean.is_empty() || speech.load(Ordering::SeqCst) > len {
                None
            } else {
                pass.correct(&clean).and_then(Result::ok)
            };
            Some(EarlyPass { raw, clean, corrected })
        })));
        finished
    }

    /// Takes a finished pass; true if it produced a result.
    fn collect(&mut self) -> bool {
        if !self.running.as_ref().is_some_and(|r| r.2.is_finished()) {
            return false;
        }
        let (len, words, h) = self.running.take().unwrap();
        match h.join() {
            Ok(Some(e)) => {
                self.done = Some((len, words, e));
                true
            }
            _ => false,
        }
    }

    /// The preview: the latest pass's corrected text in place of the live
    /// words it covered, then the live words spoken since.
    fn preview(&self, committed: &str, tentative: &str) -> (String, String) {
        let Some((_, words, e)) = &self.done else { return (committed.into(), tentative.into()) };
        let fixed = e.corrected.as_deref().unwrap_or(&e.clean);
        let c: Vec<&str> = committed.split_whitespace().collect();
        let t: Vec<&str> = tentative.split_whitespace().collect();
        let mut out = fixed.to_string();
        for w in c.iter().skip(*words) {
            out.push(' ');
            out.push_str(w);
        }
        let tail = t.iter().skip(words.saturating_sub(c.len())).copied().collect::<Vec<_>>().join(" ");
        (out, if tail.is_empty() { tail } else { format!(" {tail}") })
    }

    /// The pass to use at stop: one covering all the speech, waiting for it
    /// if it is still running.
    fn finish(mut self, log: &Path, audio: &[i16]) -> Option<EarlyPass> {
        let speech_end = self.speech_end.load(Ordering::SeqCst);
        let secs = |n: usize| n as f64 / RATE as f64;
        let t = Instant::now();
        if let Some((len, words, h)) = self.running.take() {
            if len >= speech_end {
                if let Ok(Some(e)) = h.join() {
                    self.done = Some((len, words, e));
                }
            }
        }
        let used = self.done.filter(|(len, _, _)| *len >= speech_end && self.spoke).map(|(len, _, e)| (len, e));
        if self.pass.is_some() && self.spoke {
            let what = match &used {
                Some((len, e)) => format!("early pass used ({:.1} of {:.1} s, waited {:.0} ms, corrected {})",
                    secs(*len), secs(audio.len()), t.elapsed().as_secs_f64() * 1000.0, e.corrected.is_some()),
                None => format!("no early pass (speech until {:.1} of {:.1} s)", secs(speech_end), secs(audio.len())),
            };
            append_log(log, &format!("{} daemon: {what}", crate::history::local_iso_now()));
        }
        used.map(|(_, e)| e)
    }
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
            stats: String::new(),
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
