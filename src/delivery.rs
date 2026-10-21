//! Text delivery: clipboard + paste key + typing fallback.
//!
//! Ports `deliver()`, `clip_get/set`, `send_paste_key`, `type_text`.
//! Rules preserved: terminal class -> ctrl+shift+v (bracketed paste safety
//! in nvim/readline), unknown app defaults to typing unless mode=paste,
//! clipboard verified before shortcut, restore is best-effort background
//! with still-holds-our-text guard. Backend probed once per delivery
//! (`have_copyq`), never per clipboard op.
//!
//! Two engines (`DICTATE_PASTE_ENGINE`): `native` (default) owns the
//! CLIPBOARD from a `clipboard-owner` worker over its own X connection and
//! hands it back to copyq/xclip after the settle delay; `tools` runs
//! copyq/xclip for every step. Both keep the guards above; the chord goes
//! through `xdotool key` in both.

use anyhow::{Context, Result};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Auto,
    Paste,
    Type,
}

impl InputMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "paste" => Self::Paste,
            "type" => Self::Type,
            _ => Self::Auto,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryAction {
    /// Synthesize keystrokes (universal, slow, safe fallback).
    Type,
    /// Paste via clipboard with the given chord.
    Paste { chord: &'static str },
}

/// Default terminal class list (bash `TERMINAL_CLASSES`).
pub const DEFAULT_TERMINALS: &str = "Alacritty|kitty|Ghostty|com.mitchellh.ghostty|URxvt|XTerm|st-256color|foot|WezTerm|Konsole|Gnome-terminal";

/// Case-insensitive full-match against `|`-separated class list
/// (bash `grep -qiE "^(...)$"`).
pub fn class_is_terminal(class: &str, terminals_pipe: &str) -> bool {
    let class = class.trim();
    terminals_pipe
        .split('|')
        .any(|t| t.eq_ignore_ascii_case(class))
}

/// Decide how `text` reaches the focused window.
pub fn select_action(mode: InputMode, window_class: &str, terminals_pipe: &str) -> DeliveryAction {
    if mode == InputMode::Type {
        return DeliveryAction::Type;
    }
    let cls = window_class.trim();
    if cls.is_empty() || cls == "(null)" {
        if mode == InputMode::Paste {
            return DeliveryAction::Paste { chord: "ctrl+v" };
        }
        return DeliveryAction::Type;
    }
    if class_is_terminal(cls, terminals_pipe) {
        DeliveryAction::Paste { chord: "ctrl+shift+v" }
    } else {
        DeliveryAction::Paste { chord: "ctrl+v" }
    }
}

/// `PASTE_KEY_DELAY` seconds -> integer ms for `xdotool --delay`
/// (bash `awk seconds*1000`, comma-decimal tolerant).
pub fn paste_delay_ms(delay_secs: &str) -> i64 {
    delay_secs.replace(',', ".").parse::<f64>().map(|s| (s * 1000.0) as i64).unwrap_or(12)
}

/// Poll `get` until it returns `want` (bash 10 x 20ms ownership wait).
/// Returns true when the clipboard holds our text in time.
pub fn wait_clipboard<F: Fn() -> String>(get: F, want: &str, tries: u32) -> bool {
    for _ in 0..tries.max(1) {
        if get() == want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    get() == want
}

/// Restore guard: only restore `prev` while the clipboard still holds our
/// dictation (a user copy in between must win).
pub fn should_restore_clipboard(current: &str, our_text: &str) -> bool {
    current == our_text
}

#[derive(Clone, Debug)]
pub struct ProcessClipboard {
    pub xclip_bin: String,
    pub copyq_bin: String,
    /// Forced backend for tests (`Some("copyq"|"xclip")`), else auto-probe.
    pub force_backend: Option<String>,
    /// The probe's answer: one `copyq size` per value, not one per
    /// clipboard op (bash probes once per `deliver`). With copyq installed
    /// but not running, each probe waits ~1 s for the server.
    probed: std::sync::OnceLock<&'static str>,
}

impl ProcessClipboard {
    pub fn new() -> Self {
        Self::with_bins("xclip", "copyq", None)
    }

    pub fn with_bins(xclip_bin: &str, copyq_bin: &str, force_backend: Option<&str>) -> Self {
        Self {
            xclip_bin: xclip_bin.into(),
            copyq_bin: copyq_bin.into(),
            force_backend: force_backend.map(str::to_string),
            probed: std::sync::OnceLock::new(),
        }
    }

    /// Probed once per value (bash `have_copyq`): copyq usable?
    pub fn backend(&self) -> &'static str {
        if let Some(f) = &self.force_backend {
            return if f == "copyq" { "copyq" } else { "xclip" };
        }
        self.probed.get_or_init(|| if have_copyq(&self.copyq_bin) { "copyq" } else { "xclip" })
    }

    pub fn get(&self) -> String {
        if self.backend() == "copyq" {
            std::process::Command::new(&self.copyq_bin)
                .args(["clipboard"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        } else {
            std::process::Command::new(&self.xclip_bin)
                .args(["-selection", "clipboard", "-o"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        }
    }

    pub fn set(&self, text: &str) -> Result<()> {
        if self.backend() == "copyq" {
            // `copy` prints "true"; keep it off our stdout.
            let status = std::process::Command::new(&self.copyq_bin)
                .args(["copy", "--", text])
                .stdout(std::process::Stdio::null())
                .status()
                .context("dictate: clipboard set failed")?;
            if !status.success() {
                anyhow::bail!("dictate: clipboard set failed");
            }
        } else {
            use std::io::Write;
            let mut child = std::process::Command::new(&self.xclip_bin)
                .args(["-selection", "clipboard"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .context("dictate: clipboard set failed")?;
            match child.stdin.as_mut() {
                Some(stdin) => {
                    stdin.write_all(text.as_bytes())?;
                }
                None => anyhow::bail!("dictate: clipboard set failed"),
            }
            child.wait()?;
        }
        Ok(())
    }
}

impl Default for ProcessClipboard {
    fn default() -> Self {
        Self::new()
    }
}

pub fn have_copyq(copyq_bin: &str) -> bool {
    std::process::Command::new(copyq_bin)
        .args(["size"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct Xdotool {
    pub bin: String,
}

impl Xdotool {
    pub fn active_window_class(&self) -> String {
        std::process::Command::new(&self.bin)
            .args(["getactivewindow", "getwindowclassname"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// One X client owns the whole chord incl. clearmodifiers (bash
    /// `send_paste_key` — never split modifier down/key/up across clients).
    /// On success appends `paste_shortcut_sent` to the release-timing TSV
    /// (bash `release_mark`); `timing` is `None` outside `stop` (e.g. retry),
    /// where no TSV is active.
    pub fn send_paste(&self, chord: &str, delay_secs: &str, timing: Option<&Path>) -> Result<()> {
        let ms = paste_delay_ms(delay_secs).to_string();
        let status = std::process::Command::new(&self.bin)
            .args(["key", "--clearmodifiers", "--delay", &ms, chord])
            .status()
            .context("dictate: paste shortcut failed")?;
        if !status.success() {
            anyhow::bail!("dictate: paste shortcut failed");
        }
        if let Some(tsv) = timing {
            append_timing(tsv, "paste_shortcut_sent");
        }
        Ok(())
    }

    pub fn type_text(&self, text: &str, char_delay_ms: &str) -> Result<()> {
        let status = std::process::Command::new(&self.bin)
            .args(["type", "--clearmodifiers", "--delay", char_delay_ms, "--", text])
            .status()
            .context("dictate: typing failed")?;
        if !status.success() {
            anyhow::bail!("dictate: typing failed");
        }
        Ok(())
    }
}

fn append_timing(tsv: &Path, event: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(tsv) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "{event}\t{now:.6}");
    }
}

/// Schedule clipboard restoration in a detached re-execution of this binary
/// (`restore-clipboard` subcommand).
///
/// Bash restores from a background subshell that outlives the script; a
/// `std::thread::spawn` here would die with `stop` before the settle delay
/// elapsed, so the default `DICTATE_CLIPBOARD=restore` never restored.
/// The child is spawned detached (stdio null, handle dropped) and survives
/// our exit via reparenting. Payloads travel in 0600 files, never argv.
pub fn schedule_detached_restore(
    datadir: &Path,
    exe: &Path,
    clip: &ProcessClipboard,
    our_text: &str,
    prev: &str,
    settle_secs: f64,
) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let base = format!("dictate-restore-{}-{n}", std::process::id());
    let expected_path = datadir.join(format!(".{base}.expected"));
    let prev_path = datadir.join(format!(".{base}.prev"));
    for (path, text) in [(&expected_path, our_text), (&prev_path, prev)] {
        std::fs::write(path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let spawn = std::process::Command::new(exe)
        .args([
            "restore-clipboard",
            clip.backend(),
            &settle_secs.max(0.0).to_string(),
            &expected_path.display().to_string(),
            &prev_path.display().to_string(),
            &clip.xclip_bin,
            &clip.copyq_bin,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if spawn.is_err() {
        let _ = std::fs::remove_file(&expected_path);
        let _ = std::fs::remove_file(&prev_path);
        anyhow::bail!("dictate: clipboard restore unavailable");
    }
    Ok(())
}

/// Hidden `restore-clipboard` worker: sleep, then restore `prev` only while
/// the clipboard still holds our dictation (a newer user copy must win).
/// Always removes its payload files.
pub fn run_restore_worker(
    backend: &str,
    settle_secs: f64,
    expected_path: &Path,
    prev_path: &Path,
    xclip_bin: &str,
    copyq_bin: &str,
) {
    std::thread::sleep(std::time::Duration::from_secs_f64(settle_secs.max(0.0)));
    let expected = std::fs::read_to_string(expected_path).unwrap_or_default();
    let prev = std::fs::read_to_string(prev_path).unwrap_or_default();
    let clip = ProcessClipboard::with_bins(xclip_bin, copyq_bin, Some(backend));
    if !expected.is_empty() && should_restore_clipboard(&clip.get(), &expected) {
        let _ = clip.set(&prev);
    }
    let _ = std::fs::remove_file(expected_path);
    let _ = std::fs::remove_file(prev_path);
}

// ------------------------------------------------------- clipboard owner

/// How `deliver` sets the clipboard (`DICTATE_PASTE_ENGINE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteEngine {
    /// A `clipboard-owner` worker serves the text from an X connection of
    /// its own; window class and the ready check go over our own one.
    Native,
    /// copyq/xclip/xdotool processes for every step (the old path).
    Tools,
}

impl PasteEngine {
    pub fn parse(s: &str) -> Self {
        if s == "tools" { Self::Tools } else { Self::Native }
    }
}

/// What the worker restores once the paste settled.
///
/// `prev` only while the clipboard still holds our dictation (we still own
/// it, or whoever owns it now serves the same text): a newer user copy must
/// win. Otherwise, while we still own it, our own text, so it outlives the
/// worker the way `copyq copy`/`xclip` kept it. `None`: leave it alone.
pub fn settled_clipboard<'a>(
    restore: bool,
    prev: &'a str,
    ours: &'a str,
    owned: bool,
    current: impl FnOnce() -> Option<String>,
) -> Option<&'a str> {
    if restore && !prev.is_empty() && (owned || should_restore_clipboard(&current().unwrap_or_default(), ours)) {
        Some(prev)
    } else if owned {
        Some(ours)
    } else {
        None
    }
}

enum OwnerMsg {
    Offer { restore: bool, settle: f64, text: String },
    Pasted,
    Eof,
}

/// Parses the worker's stdin: `offer <restore|keep> <settle> <len>\n<text>`,
/// then `pasted\n`. Anything else, or EOF, ends the conversation.
fn read_owner_msgs(tx: std::sync::mpsc::Sender<OwnerMsg>) {
    use std::io::{BufRead, Read};
    let stdin = std::io::stdin();
    let mut r = stdin.lock();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = tx.send(OwnerMsg::Eof);
            return;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let msg = match parts.as_slice() {
            ["offer", mode, settle, len] => {
                let Ok(len) = len.parse::<usize>() else { break };
                let mut buf = vec![0u8; len];
                if r.read_exact(&mut buf).is_err() {
                    break;
                }
                OwnerMsg::Offer {
                    restore: *mode == "restore",
                    settle: settle.parse().unwrap_or(0.4),
                    text: String::from_utf8_lossy(&buf).into_owned(),
                }
            }
            ["pasted"] => OwnerMsg::Pasted,
            _ => break,
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
    let _ = tx.send(OwnerMsg::Eof);
}

/// Hidden `clipboard-owner` worker, spawned by `stop` while recognition
/// runs. Connects to X and reads the clipboard to restore; on `offer` it
/// re-reads it (a copy made meanwhile wins), takes the CLIPBOARD with our
/// text and answers `ready` once the server lists it as owner. It serves
/// pastes until `pasted` (or EOF) plus the settle delay, then hands the
/// clipboard back through copyq/xclip (`settled_clipboard`) and exits once
/// the new owner took over. Outlives `stop` like the restore worker.
pub fn run_clipboard_owner(xclip_bin: &str, copyq_bin: &str) {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let say = |line: &str| {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    let x = match crate::x11::X::connect() {
        Ok(x) => x,
        Err(e) => return say(&format!("fail {e:#}")),
    };
    // Probe copyq now, not at handover: with copyq installed but not
    // running the probe takes ~1 s, which would delay the restore.
    let (xclip, copyq) = (xclip_bin.to_string(), copyq_bin.to_string());
    let probe = std::thread::spawn(move || {
        let clip = ProcessClipboard::with_bins(&xclip, &copyq, None);
        clip.backend();
        clip
    });
    let early_prev = x.read_restorable(Duration::from_millis(300));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || read_owner_msgs(tx));
    let (restore, settle, text) = match rx.recv() {
        Ok(OwnerMsg::Offer { restore, settle, text }) => (restore, settle, text),
        _ => return,
    };
    let prev = if restore {
        match x.read_restorable(Duration::from_millis(50)) {
            Ok(v) => v.unwrap_or_default(),
            Err(_) => early_prev.ok().flatten().unwrap_or_default(),
        }
    } else {
        String::new()
    };
    let offered_prev = (restore && !prev.is_empty()).then(|| prev.clone());
    let mut owner = match crate::x11::ClipboardOwner::take(x, text.clone(), offered_prev) {
        Ok(o) => o,
        Err(e) => return say(&format!("fail {e:#}")),
    };
    say("ready");
    // Serve the paste. A parent that never says `pasted` (killed, hung) is
    // treated like one that did after 30 s.
    let mut done = false;
    let _ = owner.serve_until(Instant::now() + Duration::from_secs(30), |_| {
        done = done || !matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        done
    });
    let _ = owner.serve_until(Instant::now() + Duration::from_secs_f64(settle.max(0.0)), |_| false);
    let owned = owner.owned();
    let fin = settled_clipboard(restore, &prev, &text, owned, || {
        owner.read_clipboard(Duration::from_millis(200)).ok().flatten()
    })
    .map(str::to_string);
    let Some(fin) = fin else {
        owner.drain();
        return;
    };
    let clip = probe.join().unwrap_or_else(|_| ProcessClipboard::with_bins(xclip_bin, copyq_bin, None));
    // Keep serving while the tool takes over, so neither a gap nor a paste
    // in flight goes unanswered. If the tool failed, keep serving like
    // xclip would until someone copies.
    let setter = std::thread::spawn(move || clip.set(&fin).is_ok());
    let mut setter = Some(setter);
    let mut give_up = Instant::now() + Duration::from_secs(30 * 86400);
    while owner.owned() && Instant::now() < give_up {
        let _ = owner.serve_until(Instant::now() + Duration::from_millis(50), |o| !o.owned());
        // Normally SelectionClear arrives first; once the tool succeeded,
        // don't wait forever for one that got lost.
        if setter.as_ref().is_some_and(|h| h.is_finished())
            && setter.take().and_then(|h| h.join().ok()) == Some(true)
        {
            give_up = Instant::now() + Duration::from_secs(2);
        }
    }
    owner.drain();
}

/// `stop`'s end of a `clipboard-owner` worker.
pub struct OwnerWorker {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl OwnerWorker {
    pub fn spawn(exe: &Path) -> Result<Self> {
        use std::io::BufRead;
        use std::process::Stdio;
        let mut child = std::process::Command::new(exe)
            .args(["clipboard-owner", "xclip", "copyq"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("dictate: clipboard worker unavailable")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("dictate: clipboard worker unavailable")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, stdin, lines })
    }

    /// Hands over the text; returns once the worker owns the clipboard.
    /// On failure the worker is killed, so it cannot take the clipboard
    /// later behind a fallback.
    pub fn offer(&mut self, text: &str, restore: bool, settle_secs: f64, timeout: std::time::Duration) -> Result<()> {
        use std::io::Write;
        let r = (|| {
            let stdin = self.stdin.as_mut().context("worker stdin closed")?;
            writeln!(stdin, "offer {} {} {}", if restore { "restore" } else { "keep" }, settle_secs.max(0.0), text.len())?;
            stdin.write_all(text.as_bytes())?;
            stdin.flush()?;
            match self.lines.recv_timeout(timeout) {
                Ok(l) if l == "ready" => Ok(()),
                Ok(l) => anyhow::bail!("{l}"),
                Err(_) => anyhow::bail!("no answer"),
            }
        })();
        if r.is_err() {
            self.abandon();
        }
        r
    }

    /// The paste key went out: the worker starts its settle delay.
    pub fn pasted(mut self) {
        use std::io::Write;
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"pasted\n");
        }
    }

    /// Kills the worker (no paste through it).
    pub fn abandon(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn explicit_type_always_types() {
        assert_eq!(select_action(InputMode::Type, "Firefox", DEFAULT_TERMINALS), DeliveryAction::Type);
        assert_eq!(select_action(InputMode::Type, "", DEFAULT_TERMINALS), DeliveryAction::Type);
    }

    #[test]
    fn terminals_paste_with_shift() {
        for cls in ["Alacritty", "kitty", "XTerm", "xterm", "GHOSTTY", "foot"] {
            assert_eq!(
                select_action(InputMode::Auto, cls, DEFAULT_TERMINALS),
                DeliveryAction::Paste { chord: "ctrl+shift+v" },
                "{cls}"
            );
        }
    }

    #[test]
    fn normal_apps_paste_plain() {
        assert_eq!(
            select_action(InputMode::Auto, "firefox", "Alacritty|xterm"),
            DeliveryAction::Paste { chord: "ctrl+v" }
        );
    }

    #[test]
    fn unknown_app_types_unless_forced_paste() {
        assert_eq!(select_action(InputMode::Auto, "", DEFAULT_TERMINALS), DeliveryAction::Type);
        assert_eq!(select_action(InputMode::Auto, "(null)", DEFAULT_TERMINALS), DeliveryAction::Type);
        assert_eq!(
            select_action(InputMode::Paste, "", DEFAULT_TERMINALS),
            DeliveryAction::Paste { chord: "ctrl+v" }
        );
    }

    #[test]
    fn terminal_match_is_case_insensitive_full_match() {
        assert!(class_is_terminal("xterm", "XTerm|foot"));
        assert!(!class_is_terminal("myxterm", "XTerm|foot"));
        assert!(!class_is_terminal("xterm2", "XTerm|foot"));
    }

    #[test]
    fn delay_conversion_matches_awk() {
        assert_eq!(paste_delay_ms("0.012"), 12);
        assert_eq!(paste_delay_ms("0"), 0);
        assert_eq!(paste_delay_ms("0,012"), 12);
    }

    #[test]
    fn clipboard_wait_succeeds_when_settles() {
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let ready = wait_clipboard(
            move || {
                c.set(c.get() + 1);
                if c.get() >= 3 { "want".to_string() } else { "old".to_string() }
            },
            "want",
            10,
        );
        assert!(ready);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn restore_guard_preserves_newer_user_copy() {
        assert!(should_restore_clipboard("our text", "our text"));
        assert!(!should_restore_clipboard("user copied this", "our text"));
    }

    #[test]
    fn detached_restore_spawn_failure_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let clip = ProcessClipboard::new();
        let err = schedule_detached_restore(
            tmp.path(),
            std::path::Path::new("/nonexistent/dictate"),
            &clip,
            "our text", "prev text", 0.4,
        );
        assert!(err.is_err());
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn restore_worker_conditional_restore() {
        let _exec_guard = crate::FAKE_EXEC_LOCK.lock().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("clip");
        std::fs::write(&state, "our text").unwrap();
        // Fake xclip: `-o` prints state, otherwise stdin becomes state.
        let fake = tmp.path().join("xclip");
        std::fs::write(&fake, format!(
            "#!/bin/sh\nif [ \"$3\" = \"-o\" ]; then cat \"{}\"; else cat > \"{}\"; fi\n",
            state.display(), state.display())).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = fake.to_string_lossy().into_owned();
        let expected = tmp.path().join("exp");
        let prev = tmp.path().join("prv");
        std::fs::write(&expected, "our text").unwrap();
        std::fs::write(&prev, "prev text").unwrap();
        run_restore_worker("xclip", 0.0, &expected, &prev, &fake, "copyq");
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "prev text");
        assert!(!expected.exists() && !prev.exists(), "payload files removed");
        // Newer user copy wins.
        std::fs::write(&state, "user copied this").unwrap();
        std::fs::write(&expected, "our text").unwrap();
        std::fs::write(&prev, "prev text").unwrap();
        run_restore_worker("xclip", 0.0, &expected, &prev, &fake, "copyq");
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "user copied this");
    }

    #[test]
    fn settled_clipboard_guards() {
        let none = || -> Option<String> { panic!("owner known: no read") };
        // Still ours: restore the previous clipboard.
        assert_eq!(settled_clipboard(true, "prev", "ours", true, none), Some("prev"));
        // Someone else serves the same text (e.g. a manager took it over).
        assert_eq!(settled_clipboard(true, "prev", "ours", false, || Some("ours".into())), Some("prev"));
        // A newer user copy wins.
        assert_eq!(settled_clipboard(true, "prev", "ours", false, || Some("user".into())), None);
        assert_eq!(settled_clipboard(true, "prev", "ours", false, || None), None);
        // Nothing to restore, keep mode: persist our text while we own it.
        assert_eq!(settled_clipboard(true, "", "ours", true, none), Some("ours"));
        assert_eq!(settled_clipboard(false, "prev", "ours", true, none), Some("ours"));
        assert_eq!(settled_clipboard(false, "prev", "ours", false, none), None);
    }

    #[test]
    fn paste_engine_defaults_native() {
        assert_eq!(PasteEngine::parse("tools"), PasteEngine::Tools);
        assert_eq!(PasteEngine::parse("native"), PasteEngine::Native);
        assert_eq!(PasteEngine::parse(""), PasteEngine::Native);
    }
}
