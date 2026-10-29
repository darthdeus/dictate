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
//! While you pause mid-recording it already runs that final pass
//! (`EarlyWorker`): by default only on the audio since the previous pause
//! (`segment`), else on all the audio so far. `stop` then replies
//! `ok <seconds> early` and leaves the result at `early_pass_path`: if the
//! pause was the end the stop process only has to paste, otherwise it
//! recognizes just the audio after the last cut.
//!
//! Protocol, one request line and one reply line:
//!   start  -> ok | busy | error <why>
//!   stop   -> ok <seconds> [early] | idle | error <why>
//!   cancel -> ok | idle    (the recording is discarded)
//!   edit-start, edit-end -> ok | idle
//!   status -> recording | idle
//!
//! Mid-dictation edit: between `edit-start` and `edit-end` you speak an
//! instruction about what you dictated so far ("no, make that Tuesday").
//! At `edit-end` the audio before it and the instruction are cut off the
//! recording; in the background whisper recognizes both and the correction
//! model applies the instruction (`Corrector::edit`), and the result is the
//! fixed start of the dictation (the "base"), shown on the overlay. The live
//! stream and pause passes start over on the audio after the cut. `stop`
//! leaves the base at `edit_base_path` for the stop process to put in front
//! of what it recognizes.

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
    stream_options_boosted(right, None)
}

/// Stream setup for a dictation: the glossary phrase boost (`boost.rs`)
/// goes in as the run vocabulary plus its weight.
fn boosted_stream(ctx: &Ctx) -> (RunOptions, StreamOptions) {
    let boost = crate::boost::preview_boost(&ctx.cfg);
    let weight = boost.as_ref().map(|(_, w)| *w);
    let run = RunOptions { vocabulary: boost.map(|(t, _)| t).unwrap_or_default(), ..Default::default() };
    (run, stream_options_boosted(right_context(ctx), weight))
}

fn stream_options_boosted(right: i32, boost: Option<f32>) -> StreamOptions {
    StreamOptions {
        family: Some(StreamExtension::ParakeetStream(ParakeetStreamOptions {
            att_context_right: Some(right),
            // Negative = off: no vocabulary, no boost (the library default
            // would otherwise apply to a non-empty vocabulary).
            vocabulary_boost: Some(boost.unwrap_or(-1.0)),
        })),
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
    edit_base: PathBuf,
    /// `edit-start` (true) and `edit-end` (false) requests, for the capture
    /// thread to act on between chunks.
    edits: Arc<std::sync::Mutex<Vec<bool>>>,
    thread: JoinHandle<(Session, Result<Captured>)>,
}

/// The clip after the last edit, the pause pass that covers it if one
/// finished, and the edited text before it.
type Captured = (Vec<i16>, Option<EarlyPass>, Option<String>);

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
            "cancel" => match recording.take() {
                None => "idle".to_string(),
                Some(r) => {
                    session = Some(cancel(r));
                    "ok".to_string()
                }
            },
            "edit-start" | "edit-end" => match &recording {
                None => "idle".to_string(),
                Some(r) => {
                    r.edits.lock().unwrap().push(line.trim() == "edit-start");
                    "ok".to_string()
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
    let edit_base = edit_base_path(&ctx.paths);
    let _ = std::fs::remove_file(&edit_base);
    let flag = stop.clone();
    let edits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let marks = edits.clone();
    let thread = std::thread::spawn(move || {
        let mut session = session;
        let res = capture(&ctx, &mut session, child, channels, chan, &flag, &marks);
        if let Err(e) = &res {
            append_log(&ctx.log, &format!("dictate daemon: capture failed: {e:#}"));
        }
        (session, res)
    });
    Ok(Recording { recorder, pidfile, stop, wav, early_path, edit_base, edits, thread })
}

fn stop(r: Recording) -> (Session, String) {
    // No more preview updates: the stop process takes over the overlay.
    r.stop.store(true, Ordering::SeqCst);
    // SIGINT makes pw-record flush and exit; the reader then sees EOF.
    crate::state::send_int(r.recorder);
    let (session, res) = r.thread.join().expect("dictate daemon: capture thread panicked");
    let _ = std::fs::remove_file(&r.pidfile);
    let reply = match res.and_then(|(audio, early, base)| {
        wav::write_mono16(&r.wav, &audio)?;
        if let Some(base) = base {
            std::fs::write(&r.edit_base, base)?;
        }
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

/// Ends a recording like `stop` but keeps nothing: no wav, no early pass.
fn cancel(r: Recording) -> Session {
    r.stop.store(true, Ordering::SeqCst);
    crate::state::send_int(r.recorder);
    let (session, _) = r.thread.join().expect("dictate daemon: capture thread panicked");
    let _ = std::fs::remove_file(&r.pidfile);
    session
}

/// Reads the recorder until EOF (stop) or `DICTATE_MAX_SECS`, feeding the
/// model 160 ms at a time and publishing each change of the preview.
fn capture(ctx: &Ctx, session: &mut Session, mut child: Child, channels: usize, chan: usize, stop: &AtomicBool,
    marks: &std::sync::Mutex<Vec<bool>>) -> Result<Captured> {
    let mut out = child.stdout.take().context("recorder has no stdout")?;
    let frame = channels * 2;
    let chunk_bytes = RATE as usize * CHUNK_MS / 1000 * frame;
    let max_samples = ctx.max_secs() as usize * RATE as usize;
    // Selected channel as mono PCM, tailed by the overlay's level meter.
    let level_path = ctx.paths.rundir.join("dictate-stream.pcm");
    let mut level = std::fs::File::create(&level_path).ok();
    let overlay = Publisher { ctx, level_path: &level_path, stop };
    overlay.show("", "");

    let (run, opts) = boosted_stream(ctx);
    let mut stream = session.stream(&run, &opts)?;
    let mut audio: Vec<i16> = Vec::with_capacity(RATE as usize * 60);
    let mut early = EarlyWorker::new(ctx);
    let fixes = crate::boost::PreviewFixes::load();
    let mut buf = vec![0u8; chunk_bytes];
    let mut eof = false;
    // Stop latency breakdown: when the stop flag was first seen, and how
    // much buffered audio was still fed to the model after it.
    let mut stop_seen: Option<Instant> = None;
    let mut fed_after_stop = 0usize;
    let mut editor = Editor::default();
    // While an edit instruction is spoken: where it starts in `audio`, and
    // how much of the stream's committed text came before it.
    let mut instruction: Option<(usize, usize)> = None;
    while !eof && audio.len() < max_samples {
        if stop_seen.is_none() && stop.load(Ordering::SeqCst) {
            stop_seen = Some(Instant::now());
        }
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
        // After stop the preview is gone and the text comes from the final
        // pass: buffered audio only needs keeping, not recognizing.
        if stop_seen.is_some() {
            fed_after_stop += mono.len();
            continue;
        }
        let mut redraw = false;
        for start in std::mem::take(&mut *marks.lock().unwrap()) {
            match (start, instruction) {
                (true, None) => instruction = Some((audio.len(), stream.text().committed.len())),
                (false, Some((from, _))) => {
                    instruction = None;
                    editor.apply(ctx, &mut audio, from);
                    // What follows the cut is a fresh dictation.
                    early = EarlyWorker::new(ctx);
                    drop(stream);
                    stream = session.stream(&run, &opts)?;
                }
                _ => {}
            }
            redraw = true;
        }
        redraw |= editor.poll();
        let t = stream.text();
        // The instruction is not dictation: pause passes skip it.
        let early_changed = instruction.is_none() && early.heard(&audio, &t.committed, &t.tentative);
        let pcm: Vec<f32> = mono.iter().map(|&s| s as f32 / 32768.0).collect();
        let update = stream.feed(&pcm)?;
        if update.committed_changed || update.tentative_changed || early_changed || redraw {
            let t = stream.text();
            match instruction {
                Some((_, before)) => {
                    let said = format!("{}{}", t.committed.get(before..).unwrap_or(""), t.tentative);
                    overlay.show_as("Edit instruction", &editor.text(), &fixes.apply(&said));
                }
                None => {
                    let (committed, tentative) = early.preview(&t.committed, &t.tentative);
                    let committed = crate::segment::join(&editor.text(), &fixes.apply(&committed));
                    let label = if editor.pending() { "Applying edit" } else { "Listening" };
                    overlay.show_as(label, &committed, &fixes.apply(&tentative));
                }
            }
        }
    }
    // Released while still speaking the instruction: it ends here.
    if let Some((from, _)) = instruction {
        editor.apply(ctx, &mut audio, from);
        early = EarlyWorker::new(ctx);
    }
    let eof_at = Instant::now();
    let _ = child.kill();
    let _ = child.wait();
    // No `stream.finalize()`: its text would be unused, and it took up to
    // 640 ms of the release (2026-10-22). Dropping the stream resets it.
    drop(stream);
    drop(level);
    let _ = std::fs::remove_file(&level_path);
    let t = Instant::now();
    let early = early.finish(&ctx.log, &audio);
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    append_log(&ctx.log, &format!("{} daemon: {:.1}s captured; stop: eof +{:.0} ms ({:.0} ms audio kept after stop), early finish {:.0} ms",
        crate::history::local_iso_now(), audio.len() as f64 / RATE as f64,
        stop_seen.map(|s| ms(eof_at - s)).unwrap_or(-1.0), fed_after_stop as f64 * 1000.0 / RATE as f64, ms(t.elapsed())));
    Ok((audio, early, editor.finish(&ctx.log)))
}

/// The mid-dictation edits of one recording: the edited text so far, and
/// the edit being made in the background.
#[derive(Default)]
struct Editor {
    base: Option<String>,
    running: Option<JoinHandle<String>>,
}

impl Editor {
    /// Cuts the dictation before `from` and the instruction after it off
    /// `audio`, and starts applying the instruction in the background.
    fn apply(&mut self, ctx: &Ctx, audio: &mut Vec<i16>, from: usize) {
        let dictation = audio[..from].to_vec();
        let instruction: Vec<i16> = audio.drain(..).skip(from).collect();
        // Edits apply in order: this one starts from the previous result.
        let previous = self.wait();
        let pass = FinalPass::new(ctx, "edit");
        // In the background while you keep talking: waiting beats losing it.
        let corrector = crate::correct::Corrector::from_config(&ctx.cfg, &ctx.terms()).map(|c| crate::correct::Corrector {
            timeout: c.timeout.max(Duration::from_secs(8)),
            meter: Some(crate::usage::Meter::new(&ctx.state_dir(), &ctx.cfg, "edit")),
            ..c
        });
        let log = ctx.log.clone();
        self.running = Some(std::thread::spawn(move || {
            // Whisper hears "..." in silence: no letters is nothing said.
            let recognize = |samples: &[i16], context: Option<&str>| {
                pass.recognize(samples, context).map(|t| crate::text::clean_whisper_text(&t))
                    .map(|t| if t.chars().any(char::is_alphanumeric) { t } else { String::new() })
            };
            let spoken = recognize(&dictation, previous.as_deref()).unwrap_or_else(|e| {
                append_log(&log, &format!("dictate daemon: edit: recognizing the dictation failed: {e:#}"));
                String::new()
            });
            let text = crate::segment::join(previous.as_deref().unwrap_or(""), &spoken);
            let edited = recognize(&instruction, None).and_then(|said| {
                append_log(&log, &format!("dictate daemon: edit instruction: {said}"));
                if said.trim().is_empty() || text.trim().is_empty() {
                    return Ok(text.clone());
                }
                corrector?.edit(&text, &said)
            });
            edited.unwrap_or_else(|e| {
                append_log(&log, &format!("dictate daemon: edit failed, keeping the text: {e:#}"));
                text
            })
        }));
    }

    /// Takes a finished edit's result; true when the text changed.
    fn poll(&mut self) -> bool {
        if self.running.as_ref().is_some_and(|t| t.is_finished()) {
            self.base = self.running.take().and_then(|t| t.join().ok());
            return true;
        }
        false
    }

    fn wait(&mut self) -> Option<String> {
        if let Some(t) = self.running.take() {
            self.base = t.join().ok();
        }
        self.base.clone()
    }

    fn pending(&self) -> bool {
        self.running.is_some()
    }

    /// What the overlay shows before the live text.
    fn text(&self) -> String {
        self.base.clone().unwrap_or_default()
    }

    fn finish(mut self, log: &Path) -> Option<String> {
        let t = Instant::now();
        let base = self.wait();
        if base.is_some() {
            append_log(log, &format!("dictate daemon: edited base ready {:.0} ms after stop", t.elapsed().as_secs_f64() * 1000.0));
        }
        base
    }
}

/// Where `stop` leaves the edited start of the dictation.
pub fn edit_base_path(paths: &Paths) -> PathBuf {
    paths.datadir.join("dictate-edit-base.txt")
}

/// Runs the final pass in the background whenever the speaker pauses.
///
/// Whole mode (`DICTATE_INCREMENTAL=0`): each pass covers the audio so far.
/// A pass whose snapshot covers all the speech is used at stop; one
/// overtaken by new speech is dropped, and the next pause starts another.
///
/// Incremental mode (the default, see `segment`): each pass recognizes only
/// the audio since the last cut, up to a new cut inside the pause, and
/// appends it to the text so far. The cut is kept once the segment holds
/// `DICTATE_SEGMENT_MIN_MS` of audio up to its last speech: whisper is less
/// accurate on short pieces. A pass over less is speculative: it is used if
/// the pause turns out to be the end, and otherwise dropped, its audio going
/// again into the next segment. Kept segments tile the recording; speech
/// resuming during a pass only lands in the next one.
///
/// Either way correction (on the whole text) is skipped when speech resumed
/// during the pass, which keeps Groq requests (rate-limited per minute) to
/// about one per pause.
struct EarlyWorker {
    pass: Option<Arc<FinalPass>>,
    incremental: bool,
    /// A chunk whose peak is at or above this (dBFS) is speech.
    level: f32,
    pause_samples: usize,
    min_segment: usize,
    /// End of the last speech chunk, shared with the running pass.
    speech_end: Arc<AtomicUsize>,
    running: Option<Running>,
    /// The latest pass that covers the speech so far (whole mode), or the
    /// kept segments (incremental).
    done: Option<Done>,
    /// Incremental: the kept segments plus a speculative one after them.
    speculative: Option<Done>,
    /// `speech_end` when the last incremental pass started: one pass per
    /// pause, and a failed one is retried only after new speech (or at stop).
    tried_at: usize,
    spoke: bool,
}

struct Running {
    /// Audio the pass covers: `[0, len)`.
    len: usize,
    /// Incremental: whether its cut is kept.
    keep: bool,
    thread: JoinHandle<Option<Done>>,
}

/// A finished pass.
struct Done {
    /// The audio length it covers.
    len: usize,
    /// How many words of the live text it replaces in the preview.
    words: usize,
    pass: EarlyPass,
    /// What the preview shows for it: the correction, or when that was
    /// skipped, the previous segments' shown text plus the new segment.
    shown: String,
}

impl EarlyWorker {
    fn new(ctx: &Ctx) -> Self {
        let on = ctx.c("DICTATE_EARLY", "1") == "1";
        Self {
            pass: on.then(|| Arc::new(FinalPass::new(ctx, "early"))),
            incremental: ctx.c("DICTATE_INCREMENTAL", "1") == "1",
            level: ctx.c("DICTATE_EARLY_LEVEL", "-30").parse().unwrap_or(-30.0),
            pause_samples: ctx.c("DICTATE_EARLY_PAUSE_MS", "500").parse::<usize>().unwrap_or(500) * RATE as usize / 1000,
            min_segment: ctx.min_segment(),
            speech_end: Arc::new(AtomicUsize::new(0)),
            running: None,
            done: None,
            speculative: None,
            tried_at: 0,
            spoke: false,
        }
    }

    /// Where the kept segments end (the last cut).
    fn cut(&self) -> usize {
        self.done.as_ref().map_or(0, |d| d.len)
    }

    /// Called after each chunk is appended to `audio`, with the live text so
    /// far; true when a pass just finished (the preview changes).
    fn heard(&mut self, audio: &[i16], committed: &str, tentative: &str) -> bool {
        let Some(pass) = self.pass.clone() else { return false };
        let tail = &audio[audio.len().saturating_sub(crate::segment::CHUNK)..];
        if wav::peak_db(tail) >= self.level {
            self.spoke = true;
            self.speech_end.store(audio.len(), Ordering::SeqCst);
            return self.collect();
        }
        let finished = self.collect();
        let speech_end = self.speech_end.load(Ordering::SeqCst);
        let covered = if self.incremental {
            self.cut().max(self.tried_at)
        } else {
            self.cut().max(self.running.as_ref().map_or(0, |r| r.len))
        };
        if !self.spoke || self.running.is_some() || covered >= speech_end || audio.len() - speech_end < self.pause_samples {
            return finished;
        }
        let words = committed.split_whitespace().count() + tentative.split_whitespace().count();
        let speech = self.speech_end.clone();
        let len = if self.incremental { crate::segment::cut_in_pause(audio, speech_end) } else { audio.len() };
        let thread = if self.incremental {
            self.tried_at = speech_end;
            let (prev, shown) = self.done.as_ref().map(|d| (d.pass.clone(), d.shown.clone())).unwrap_or_default();
            let samples = audio[self.cut()..len].to_vec();
            std::thread::spawn(move || {
                let context = prev.corrected.as_deref().unwrap_or(&prev.clean);
                let segment = pass.recognize(&samples, Some(context)).ok()?;
                let raw = crate::segment::join(&prev.raw, &segment);
                let clean = crate::text::clean_whisper_text(&raw);
                let corrected = if clean.is_empty() || speech.load(Ordering::SeqCst) > len {
                    None
                } else {
                    pass.correct(&clean).and_then(Result::ok)
                };
                let shown = corrected.clone()
                    .unwrap_or_else(|| crate::segment::join(&shown, &crate::text::clean_whisper_text(&segment)));
                let pass = EarlyPass { raw, clean, corrected, until: Some(len), segments: prev.segments + 1 };
                Some(Done { len, words, pass, shown })
            })
        } else {
            let samples = audio.to_vec();
            std::thread::spawn(move || {
                let raw = pass.whisper(&samples).ok()?;
                let clean = crate::text::clean_whisper_text(&raw);
                let corrected = if clean.is_empty() || speech.load(Ordering::SeqCst) > len {
                    None
                } else {
                    pass.correct(&clean).and_then(Result::ok)
                };
                let shown = corrected.clone().unwrap_or_else(|| clean.clone());
                Some(Done { len, words, pass: EarlyPass { raw, clean, corrected, ..Default::default() }, shown })
            })
        };
        let keep = !self.incremental || speech_end - self.cut() >= self.min_segment;
        self.running = Some(Running { len, keep, thread });
        finished
    }

    /// Takes a finished pass; true if it produced a result.
    fn collect(&mut self) -> bool {
        if !self.running.as_ref().is_some_and(|r| r.thread.is_finished()) {
            return false;
        }
        self.join()
    }

    /// Waits for the running pass, if any; true if it produced a result.
    fn join(&mut self) -> bool {
        let Some(r) = self.running.take() else { return false };
        let Ok(Some(d)) = r.thread.join() else { return false };
        if r.keep {
            self.speculative = None;
            self.done = Some(d);
        } else {
            self.speculative = Some(d);
        }
        true
    }

    /// The pass the preview shows: the speculative one when it is the latest.
    fn latest(&self) -> Option<&Done> {
        match (&self.done, &self.speculative) {
            (Some(d), Some(s)) if d.len >= s.len => Some(d),
            (_, Some(s)) => Some(s),
            (d, None) => d.as_ref(),
        }
    }

    /// The preview: the latest pass's corrected text in place of the live
    /// words it covered, then the live words spoken since.
    fn preview(&self, committed: &str, tentative: &str) -> (String, String) {
        let Some(Done { words, shown, .. }) = self.latest() else { return (committed.into(), tentative.into()) };
        let c: Vec<&str> = committed.split_whitespace().collect();
        let t: Vec<&str> = tentative.split_whitespace().collect();
        let mut out = shown.to_string();
        for w in c.iter().skip(*words) {
            out.push(' ');
            out.push_str(w);
        }
        let tail = t.iter().skip(words.saturating_sub(c.len())).copied().collect::<Vec<_>>().join(" ");
        (out, if tail.is_empty() { tail } else { format!(" {tail}") })
    }

    /// The pass to use at stop. Whole mode: one covering all the speech,
    /// waiting for it if it is still running. Incremental: the latest pass
    /// (waiting for a running one, which started earlier than any pass
    /// `stop` could start), speculative or kept: `until` is the clip's end
    /// when no speech followed it, else `stop` recognizes the rest after the
    /// kept segments.
    fn finish(mut self, log: &Path, audio: &[i16]) -> Option<EarlyPass> {
        let speech_end = self.speech_end.load(Ordering::SeqCst);
        let secs = |n: usize| n as f64 / RATE as f64;
        let t = Instant::now();
        if self.running.as_ref().is_some_and(|r| self.incremental || r.len >= speech_end) {
            self.join();
        }
        let waited = t.elapsed().as_secs_f64() * 1000.0;
        let used = if !self.spoke {
            None
        } else if self.incremental {
            let ends = |d: &Done| speech_end <= d.len;
            match (self.done.take(), self.speculative.take()) {
                (_, Some(s)) if ends(&s) => Some((s.len, EarlyPass { until: Some(audio.len()), ..s.pass })),
                (Some(d), _) => Some((d.len, EarlyPass { until: Some(if ends(&d) { audio.len() } else { d.len }), ..d.pass })),
                (None, _) => None,
            }
        } else {
            self.done.take().filter(|d| d.len >= speech_end).map(|d| (d.len, d.pass))
        };
        if self.pass.is_some() && self.spoke {
            let what = match &used {
                Some((len, e)) if self.incremental => format!(
                    "early segments used ({} covering {:.1} of {:.1} s, tail {}, waited {waited:.0} ms, corrected {})",
                    e.segments, secs(*len), secs(audio.len()),
                    if e.until == Some(audio.len()) { "silent".to_string() } else { format!("{:.1} s", secs(audio.len() - len)) },
                    e.corrected.is_some()),
                Some((len, e)) => format!("early pass used ({:.1} of {:.1} s, waited {waited:.0} ms, corrected {})",
                    secs(*len), secs(audio.len()), e.corrected.is_some()),
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
        self.show_as("Listening", committed, tentative)
    }

    fn show_as(&self, label: &str, committed: &str, tentative: &str) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let max = self.ctx.max_secs() as f64;
        let msg = OverlayMessage {
            phase: Phase::Recording,
            label: label.into(),
            text: committed.trim_start().to_string(),
            engine: live_label(self.ctx),
            stats: String::new(),
            raw: String::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Per request: samples received and the prompt.
    type Seen = Arc<Mutex<Vec<(usize, String)>>>;

    /// A whisper-server stand-in answering `s0`, `s1`, ... after `delay_ms`.
    fn fake_server(delay_ms: u64) -> (String, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let head_end = loop {
                    let n = c.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                if head.contains("expect: 100-continue") {
                    let _ = c.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                }
                while buf.len() < head_end + len {
                    let n = c.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = &buf[head_end..];
                let riff = body.windows(4).position(|w| w == b"RIFF").unwrap();
                let data_len = u32::from_le_bytes(body[riff + 40..riff + 44].try_into().unwrap()) as usize;
                let text = String::from_utf8_lossy(body);
                let prompt = text.split("name=\"prompt\"\r\n\r\n").nth(1)
                    .and_then(|p| p.split("\r\n--").next()).unwrap_or("").to_string();
                std::thread::sleep(Duration::from_millis(delay_ms));
                let mut seen = log.lock().unwrap();
                let reply = format!("{{\"text\":\"s{}\"}}", seen.len());
                seen.push((data_len / 2, prompt));
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        (url, seen)
    }

    fn ctx(dir: &Path, server: &str, min_ms: u32) -> Ctx {
        let mut env = crate::config::Env::with_vars([
            ("DICTATE_SERVER".into(), server.into()),
            ("DICTATE_PROMPT".into(), "Glossary.".into()),
            ("DICTATE_SEGMENT_MIN_MS".into(), min_ms.to_string()),
            ("DICTATE_MODEL".into(), dir.join("no-model.bin").to_string_lossy().into_owned()),
        ]);
        env.xdg_config_home = Some(dir.to_string_lossy().into_owned());
        env.home = Some(dir.to_string_lossy().into_owned());
        let cfg = crate::config::load_from_env(&env).unwrap();
        let p = |n: &str| dir.join(n);
        let paths = Paths {
            rundir: dir.into(), datadir: dir.into(), state_dir: dir.into(), history_dir: p("h"), history_enabled: false,
            pidfile: p("p"), startfile: p("st"), wav: p("w"), wav_proc: p("wp"), raw: p("r"), log: p("l"),
            release_timing: p("rt"), preview_timing: p("pt"), preview_pidfile: p("pp"), modefile: p("m"),
        };
        Ctx::new(cfg, paths, false).unwrap()
    }

    fn tone(secs: f64) -> Vec<i16> {
        (0..(secs * RATE as f64) as usize).map(|i| if i % 2 == 0 { 8000 } else { -8000 }).collect()
    }

    fn quiet(secs: f64) -> Vec<i16> {
        vec![0; (secs * RATE as f64) as usize]
    }

    /// Feeds `audio[from..]` 160 ms at a time; `wait` joins each pass as soon
    /// as it starts, as if recognition were instant.
    fn feed(w: &mut EarlyWorker, audio: &[i16], from: usize, wait: bool) {
        let mut len = from;
        while len < audio.len() {
            len = (len + crate::segment::CHUNK).min(audio.len());
            w.heard(&audio[..len], "", "");
            if wait {
                w.join();
            }
        }
    }

    #[test]
    fn segments_tile_the_recording() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (url, seen) = fake_server(0);
        let ctx = ctx(tmp.path(), &url, 0);
        let mut audio = tone(1.5);
        audio.extend(quiet(1.0));
        audio.extend(tone(2.0));
        audio.extend(quiet(1.0));
        audio.extend(tone(1.0));
        audio.extend(quiet(0.2));
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio, 0, true);
        let e = w.finish(&ctx.log, &audio).unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(e.segments, 2);
        assert_eq!(e.raw, "s0 s1");
        let until = e.until.unwrap();
        assert!(until < audio.len(), "speech after the last cut leaves a tail");
        assert_eq!(seen.iter().map(|s| s.0).sum::<usize>(), until, "segments cover [0, until) once");
        let at = |n: usize| n as f64 / RATE as f64;
        assert!((1.5..2.5).contains(&at(seen[0].0)), "first cut in the first pause: {}", at(seen[0].0));
        assert_eq!(seen[0].1, "Glossary.");
        assert_eq!(seen[1].1, "Glossary. s0", "the previous text follows the prompt");
    }

    #[test]
    fn silent_tail_needs_nothing_more() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (url, seen) = fake_server(0);
        let ctx = ctx(tmp.path(), &url, 0);
        let mut audio = tone(1.0);
        audio.extend(quiet(0.8));
        audio.extend(tone(1.0));
        audio.extend(quiet(1.2));
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio, 0, true);
        let e = w.finish(&ctx.log, &audio).unwrap();
        assert_eq!(e.until, Some(audio.len()), "no speech after the last cut: nothing left for stop");
        assert_eq!((e.segments, e.raw.as_str()), (2, "s0 s1"));
        let sent: usize = seen.lock().unwrap().iter().map(|s| s.0).sum();
        assert!(sent < audio.len() && sent > audio.len() - RATE as usize, "{sent}");
    }

    #[test]
    fn speech_during_a_pass_goes_to_the_next() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (url, seen) = fake_server(300);
        let ctx = ctx(tmp.path(), &url, 0);
        let mut audio = tone(1.0);
        audio.extend(quiet(0.8));
        audio.extend(tone(1.0));
        audio.extend(quiet(1.2));
        audio.extend(tone(0.5));
        // All of it arrives while the first pass runs: that pass keeps its
        // cut, nothing else starts, and stop gets the rest as the tail.
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio, 0, false);
        let e = w.finish(&ctx.log, &audio).unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!((e.segments, e.raw.as_str()), (1, "s0"));
        assert_eq!(e.until, Some(seen[0].0));
        assert!(e.corrected.is_none());
    }

    #[test]
    fn short_segments_are_speculative() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (url, seen) = fake_server(0);
        let ctx = ctx(tmp.path(), &url, 2000);
        let mut audio = tone(1.0);
        audio.extend(quiet(0.8)); // 1 s of speech: speculative pass, dropped
        audio.extend(tone(1.5));
        audio.extend(quiet(0.8)); // 2.5 s since the start: kept
        let kept = audio.len();
        audio.extend(tone(1.0));
        audio.extend(quiet(0.8)); // speculative again, and the end
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio[..kept], 0, true);
        assert_eq!(w.done.as_ref().map(|d| d.pass.raw.as_str()), Some("s1"), "the kept segment replaces the speculative one");
        let resumed = audio.len() - (0.8 * RATE as f64) as usize;
        feed(&mut w, &audio[..resumed], kept, true);
        // Stopping during speech: the kept segment, the rest is the tail.
        let e = w.finish(&ctx.log, &audio[..resumed]).unwrap();
        assert_eq!((e.raw.as_str(), e.until), ("s1", Some(seen.lock().unwrap()[1].0)));
        let seen = seen.lock().unwrap().clone();
        assert!(seen[0].0 < seen[1].0, "the kept segment resends the speculative audio");
    }

    #[test]
    fn speculative_pass_ends_the_dictation() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (url, _) = fake_server(0);
        let ctx = ctx(tmp.path(), &url, 6000);
        let mut audio = tone(1.0);
        audio.extend(quiet(1.0));
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio, 0, true);
        let e = w.finish(&ctx.log, &audio).unwrap();
        assert_eq!((e.raw.as_str(), e.until, e.segments), ("s0", Some(audio.len()), 1));
    }

    #[test]
    fn failed_segment_leaves_its_audio_to_stop() {
        let _guard = crate::FAKE_EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // Nothing listens here and whisper-cli has no model: every pass fails.
        let ctx = ctx(tmp.path(), "http://127.0.0.1:9", 0);
        let mut audio = tone(1.0);
        audio.extend(quiet(1.0));
        let mut w = EarlyWorker::new(&ctx);
        feed(&mut w, &audio, 0, true);
        assert!(w.finish(&ctx.log, &audio).is_none(), "no segment: stop recognizes the whole clip");
    }
}
