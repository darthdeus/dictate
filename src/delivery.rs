//! Text delivery: clipboard + paste key + typing fallback.
//!
//! Ports `deliver()`, `clip_get/set`, `send_paste_key`, `type_text`.
//! Rules preserved: terminal class -> ctrl+shift+v (bracketed paste safety
//! in nvim/readline), unknown app defaults to typing unless mode=paste,
//! clipboard verified before shortcut, restore is best-effort background
//! with still-holds-our-text guard. Backend probed once per delivery
//! (`have_copyq`), never per clipboard op.

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
}

impl ProcessClipboard {
    pub fn new() -> Self {
        Self { xclip_bin: "xclip".into(), copyq_bin: "copyq".into(), force_backend: None }
    }

    /// Probe once per delivery (bash `have_copyq`): copyq usable?
    pub fn backend(&self) -> &'static str {
        if let Some(f) = &self.force_backend {
            return if f == "copyq" { "copyq" } else { "xclip" };
        }
        if have_copyq(&self.copyq_bin) { "copyq" } else { "xclip" }
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
            std::process::Command::new(&self.copyq_bin)
                .args(["copy", "--", text])
                .status()
                .context("dictate: clipboard set failed")?;
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
    let clip = ProcessClipboard {
        xclip_bin: xclip_bin.to_string(),
        copyq_bin: copyq_bin.to_string(),
        force_backend: Some(backend.to_string()),
    };
    if !expected.is_empty() && should_restore_clipboard(&clip.get(), &expected) {
        let _ = clip.set(&prev);
    }
    let _ = std::fs::remove_file(expected_path);
    let _ = std::fs::remove_file(prev_path);
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
}
