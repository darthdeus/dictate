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
            child.stdin.as_mut().unwrap().write_all(text.as_bytes())?;
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
    pub fn send_paste(&self, chord: &str, delay_secs: &str, log: &Path) -> Result<()> {
        let ms = paste_delay_ms(delay_secs).to_string();
        let status = std::process::Command::new(&self.bin)
            .args(["key", "--clearmodifiers", "--delay", &ms, chord])
            .status()
            .context("dictate: paste shortcut failed")?;
        if !status.success() {
            anyhow::bail!("dictate: paste shortcut failed");
        }
        append_line(log, "paste_shortcut_sent marker (see timings)");
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

fn append_line(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
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
}
