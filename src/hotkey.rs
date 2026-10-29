//! `dictate hotkey`: push-to-talk and action keys without a key grab.
//!
//! A passive grab (sxhkd, xbindkeys, i3 bindsym) makes the X server send the
//! focused window `FocusOut` with mode `NotifyGrab` for as long as the key is
//! held. Focus never moves, but Alacritty and other terminals draw that as
//! lost focus (hollow cursor) for the whole dictation. This listener reads
//! XInput2 raw key events on the root window instead, which every client
//! that selects them receives with no grab and no focus events, and runs
//! `dictate start` on press and `dictate stop` on release.
//!
//! Without a grab the focused window would also get the keys, so the
//! listener clears their keysyms (apps ignore a key with no symbol) and
//! clears them again when the keymap is reloaded (`setxkbmap`). On a talk
//! key, Shift held at press or release runs `dictate stop --sentence` (end
//! the sentence, not the paragraph; see `target`); other modifiers are ignored.
//!
//! `DICTATE_HOTKEYS` binds keys to actions, `key=action` separated by
//! spaces. Keys are f13-f24 or X keycodes; actions:
//!   talk, talk-sentence  hold to talk; then a new paragraph / sentence
//!   cancel, retry      discard the recording / re-paste the last dictation
//!   learn              capture corrections from the current CLI draft
//!   window-prev, window-next  previous / next window of the focused tmux
//!   pane-prev, pane-next      previous / next pane in that window
//!   key:<chord>        send a chord with xdotool, e.g. key:alt+BackSpace
//!   layer              hold for the DICTATE_HOTKEYS_LAYER2 bindings
//!   edit               hold while dictating to say an edit instruction
//! While a talk key is held, `DICTATE_HOTKEYS_TALKING` applies instead
//! (chords: default f18=edit), and while the layer key is held,
//! `DICTATE_HOTKEYS_LAYER2`; keys bound in neither keep their layer 1
//! binding. Outside layer 1 the overlay shows the pad's bindings.
//! The config file is re-read at the next key press after it changes, so
//! bindings change without a restart. Every action is logged to
//! `keys.jsonl` in the state dir (`dictate keys` summarizes it). A key a binding no longer names keeps
//! its cleared keysyms until the X session restarts.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _, KeyEventFlags, XIEventMask};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, KeyButMask, Mapping, Window};
use x11rb::protocol::Event;

use crate::config::{self, Env};
use crate::target::Nav;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    /// `start` on press, `stop` on release.
    Talk { sentence: bool },
    /// `dictate <arg>` on press.
    Run(&'static str),
    /// Move in the focused terminal's tmux, in this process so a fast knob
    /// turn's steps stay in order.
    Nav(Nav),
    /// `xdotool key <chord>` on press.
    Key(String),
    /// While held, keys use their `DICTATE_HOTKEYS_LAYER2` binding, and the
    /// overlay shows them.
    Layer,
    /// Held during a dictation: what you say is an instruction for the text
    /// so far (`daemon` edit-start / edit-end).
    Edit,
}

impl Action {
    /// As written in `DICTATE_HOTKEYS`.
    fn name(&self) -> String {
        match self {
            Action::Talk { sentence: false } => "talk".into(),
            Action::Talk { sentence: true } => "talk-sentence".into(),
            Action::Run(a) => a.to_string(),
            Action::Nav(Nav::Window(s)) => if *s > 0 { "window-next" } else { "window-prev" }.into(),
            Action::Nav(Nav::Pane(s)) => if *s > 0 { "pane-next" } else { "pane-prev" }.into(),
            Action::Key(chord) => format!("key:{chord}"),
            Action::Layer => "layer".into(),
            Action::Edit => "edit".into(),
        }
    }

    /// For the layer grid.
    fn label(&self) -> String {
        match self {
            Action::Talk { sentence: false } => "talk ¶".into(),
            Action::Talk { sentence: true } => "talk .".into(),
            Action::Run("retry") => "re-paste".into(),
            Action::Run("learn") => "✓ learn".into(),
            Action::Nav(Nav::Window(s)) => if *s > 0 { "window →" } else { "← window" }.into(),
            Action::Nav(Nav::Pane(s)) => if *s > 0 { "pane →" } else { "← pane" }.into(),
            Action::Key(chord) => match chord.as_str() {
                "Return" => "⏎ enter".into(),
                "alt+BackSpace" => "⌫ word".into(),
                c => c.into(),
            },
            Action::Layer => "LAYER".into(),
            Action::Edit => "✎ LLM edit".into(),
            a => a.name(),
        }
    }
}

/// The macro pad, row by row from the top left (2 wide, 3 tall): what the
/// layer grid shows.
const PAD: [u8; 6] = [191, 192, 193, 194, 195, 196];

/// `f13`-`f24`, else the keycode.
fn key_name(code: u32) -> String {
    match code {
        191..=202 => format!("f{}", code - 191 + 13),
        _ => code.to_string(),
    }
}

/// Caps Lock (keyd sends F19) and the action keys a macro pad maps onto.
const DEFAULT_BINDINGS: &str =
    "f19=talk f17=talk f15=talk-sentence f16=cancel f13=layer f20=window-prev f21=window-next f22=pane-next";
const DEFAULT_LAYER2: &str = "f14=learn f18=retry";
/// While a talk key is held: chords with it.
const DEFAULT_TALKING: &str = "f18=edit";

/// Which bindings are in effect: layer 1, the layer key held, or a talk key
/// held (`DICTATE_HOTKEYS_TALKING`). The layer key wins over talking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Base = 1,
    Layer = 2,
    Talking = 3,
}

/// X keycode for a key name: f13-f24 (evdev KEY_F13 = 183, + 8) or a number.
fn keycode(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (13..=24).contains(&n).then(|| n - 13 + 191);
    }
    name.parse().ok()
}

fn parse_bindings(s: &str) -> Result<Vec<(u8, Action)>> {
    s.split_whitespace()
        .map(|b| {
            let (key, action) = b.split_once('=').with_context(|| format!("{b:?}: expected key=action"))?;
            let code = keycode(key).with_context(|| format!("{key:?}: expected f13-f24 or a keycode"))?;
            let action = match action {
                "talk" => Action::Talk { sentence: false },
                "talk-sentence" | "talk-space" => Action::Talk { sentence: true },
                "cancel" => Action::Run("cancel"),
                "retry" => Action::Run("retry"),
                "learn" => Action::Run("learn"),
                "window-prev" => Action::Nav(Nav::Window(-1)),
                "window-next" => Action::Nav(Nav::Window(1)),
                "pane-prev" => Action::Nav(Nav::Pane(-1)),
                "pane-next" => Action::Nav(Nav::Pane(1)),
                "layer" => Action::Layer,
                "edit" => Action::Edit,
                a => match a.strip_prefix("key:") {
                    Some(chord) if !chord.is_empty() => Action::Key(chord.to_string()),
                    _ => bail!("{a:?}: unknown action"),
                },
            };
            Ok((code, action))
        })
        .collect()
}

/// The bindings and the config file they came from, reloaded when it changes.
struct Bindings {
    path: Option<PathBuf>,
    stamp: Option<SystemTime>,
    keys: Vec<(u8, Action)>,
    layer2: Vec<(u8, Action)>,
    talking: Vec<(u8, Action)>,
}

impl Bindings {
    fn load() -> Self {
        let mut b = Bindings { path: config::config_path(&Env::from_process()), stamp: None, keys: Vec::new(), layer2: Vec::new(), talking: Vec::new() };
        b.reload();
        b
    }

    fn file_stamp(&self) -> Option<SystemTime> {
        self.path.as_ref().and_then(|p| std::fs::metadata(p).ok()?.modified().ok())
    }

    /// Re-reads the config if it changed; true when the keys were replaced.
    /// A bad binding keeps the previous ones.
    fn reload_if_changed(&mut self) -> bool {
        self.file_stamp() != self.stamp && self.reload()
    }

    fn reload(&mut self) -> bool {
        self.stamp = self.file_stamp();
        let cfg = config::load().ok();
        let spec = |key: &str, default: &str| {
            cfg.as_ref().and_then(|c| c.get(key)).unwrap_or(default).to_string()
        };
        let spec1 = spec("DICTATE_HOTKEYS", DEFAULT_BINDINGS);
        let spec2 = spec("DICTATE_HOTKEYS_LAYER2", DEFAULT_LAYER2);
        let spec3 = spec("DICTATE_HOTKEYS_TALKING", DEFAULT_TALKING);
        let parsed = parse_bindings(&spec1).and_then(|k| {
            Ok((k, parse_bindings(&spec2).context("layer 2")?, parse_bindings(&spec3).context("talking")?))
        });
        match parsed {
            Ok((keys, layer2, talking)) => {
                eprintln!("dictate hotkey: {spec1} | layer 2: {spec2} | talking: {spec3}");
                self.keys = keys;
                self.layer2 = layer2;
                self.talking = talking;
                true
            }
            Err(e) => {
                eprintln!("dictate hotkey: DICTATE_HOTKEYS: {e:#}; keeping the previous bindings");
                false
            }
        }
    }

    fn base(&self, detail: u32) -> Option<&Action> {
        self.keys.iter().find(|(k, _)| *k as u32 == detail).map(|(_, a)| a)
    }

    fn table(&self, mode: Mode) -> &[(u8, Action)] {
        match mode {
            Mode::Base => &self.keys,
            Mode::Layer => &self.layer2,
            Mode::Talking => &self.talking,
        }
    }

    /// The binding in effect, and its layer: a key without a binding in
    /// `mode` keeps its layer 1 one.
    fn action(&self, detail: u32, mode: Mode) -> Option<(&Action, Mode)> {
        match self.table(mode).iter().find(|(k, _)| *k as u32 == detail) {
            Some((_, a)) => Some((a, mode)),
            None => self.base(detail).map(|a| (a, Mode::Base)),
        }
    }

    /// `layer.json` for the overlay's grid: shown outside layer 1, with the
    /// held key (layer or talk) marked.
    fn grid(&self, mode: Mode, held: Option<u32>) -> serde_json::Value {
        let cells: Vec<_> = PAD.iter().map(|&k| {
            let base = self.base(k as u32);
            let (label, kind) = match (self.table(mode).iter().find(|(c, _)| *c == k), base) {
                (_, Some(a)) if held == Some(k as u32) => (a.label(), "layer"),
                (Some((_, a)), _) => (a.label(), "action"),
                (None, Some(a)) => (a.label(), "empty"),
                (None, None) => ("—".to_string(), "empty"),
            };
            let base = base.map(|a| format!("layer 1: {}", a.label())).filter(|_| kind != "layer");
            serde_json::json!({ "label": label, "kind": kind, "base": base.unwrap_or_default() })
        }).collect();
        let title = if mode == Mode::Talking { "WHILE DICTATING" } else { "LAYER 2" };
        serde_json::json!({ "active": mode != Mode::Base, "ts": crate::pipeline::now_unix(), "title": title, "cells": cells })
    }
}

pub fn run() -> Result<()> {
    let (conn, screen) = x11rb::connect(None).context("X display unavailable")?;
    let root = conn.setup().roots[screen].root;
    conn.extension_information(xinput::X11_EXTENSION_NAME)?
        .context("X server has no XInputExtension")?;
    // Raw events reach every selecting client regardless of grabs from 2.1 on.
    let v = conn.xinput_xi_query_version(2, 2)?.reply()?;
    anyhow::ensure!((v.major_version, v.minor_version) >= (2, 1), "XInput {}.{} is older than 2.1", v.major_version, v.minor_version);
    conn.xinput_xi_select_events(root, &[xinput::EventMask {
        deviceid: xinput::Device::ALL_MASTER.into(),
        mask: vec![XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE],
    }])?;
    let mut bindings = Bindings::load();
    let paths = config::load().ok().and_then(|c| crate::paths::Paths::resolve(&c.as_env(&Env::from_process())).ok());
    let grid_path = paths.as_ref().map(|p| crate::pipeline::overlay_state_path(&p.rundir).with_file_name("layer.json"));
    let show_grid = |b: &Bindings, mode: Mode, held: Option<u32>| {
        let active = mode != Mode::Base;
        let Some(path) = &grid_path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Atomic like state.json: the renderer never reads half a file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, b.grid(mode, held).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
        if active {
            // Idle, the renderer has quit; its lock makes a second one exit.
            let cfg = config::load().unwrap_or_default();
            let _ = Command::new(crate::pipeline::renderer_binary()).envs(cfg.osd_vars())
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
                .map(|mut c| std::thread::spawn(move || c.wait()));
        }
    };
    silence(&conn, &bindings)?;
    conn.flush()?;

    let exe = std::env::current_exe().unwrap_or_else(|_| "dictate".into());
    let state_dir = paths.as_ref().map(|p| p.state_dir.clone());
    // Talk rows are written on release but stamped with the press.
    let log = |key: u32, action: String, layer: u8, held: Option<f64>| {
        let Some(dir) = &state_dir else { return };
        let win = active_window(&conn, root);
        let pid = win.and_then(|w| window_pid(&conn, w));
        crate::usage::log_key(dir, &crate::usage::KeyPress {
            ts: crate::pipeline::now_unix() - held.unwrap_or(0.0),
            key: key_name(key),
            action,
            layer,
            app: win.map(|w| window_class(&conn, w)).unwrap_or_default(),
            pane: pid.and_then(crate::target::tmux_command).unwrap_or_default(),
            held,
        });
    };
    // The talk key being held: whether its stop ends the sentence, since
    // when, and its binding.
    let mut held: Option<(u32, bool, Instant, String)> = None;
    // The layer key being held, and the edit key.
    let mut layer: Option<u32> = None;
    let mut editing: Option<u32> = None;
    let daemon = |req: &str| paths.as_ref().and_then(|p| crate::daemon::request(p, req));
    let mut shown = Mode::Base;
    loop {
        let mode = |layer: Option<u32>, held: &Option<(u32, bool, Instant, String)>| match (layer, held) {
            (Some(_), _) => Mode::Layer,
            (None, Some(_)) => Mode::Talking,
            (None, None) => Mode::Base,
        };
        match conn.wait_for_event().context("X connection lost")? {
            Event::XinputRawKeyPress(e) if !e.flags.contains(KeyEventFlags::KEY_REPEAT) => {
                // Every key press on the desktop arrives here, so an edited
                // config applies from the next one.
                if bindings.reload_if_changed() {
                    silence(&conn, &bindings)?;
                    conn.flush()?;
                }
                let Some((action, level)) = bindings.action(e.detail, mode(layer, &held)).map(|(a, l)| (a.clone(), l)) else { continue };
                match &action {
                    Action::Layer => layer = Some(e.detail),
                    Action::Edit => {
                        if editing.is_none() && daemon("edit-start").as_deref() == Some("ok") {
                            editing = Some(e.detail);
                        }
                    }
                    &Action::Talk { sentence } => {
                        if held.is_none() {
                            let sentence = sentence || shift_held(&conn, root);
                            held = Some((e.detail, sentence, Instant::now(), action.name()));
                            spawn(&exe, &["start"]);
                        }
                        // Logged on release, with the time held.
                    }
                    Action::Run(arg) => spawn(&exe, &[arg]),
                    &Action::Nav(nav) => {
                        if let Some(pid) = active_window(&conn, root).and_then(|w| window_pid(&conn, w)) {
                            crate::target::navigate(pid, nav);
                        }
                    }
                    Action::Key(chord) => spawn("xdotool".as_ref(), &["key", "--clearmodifiers", chord]),
                }
                if !matches!(action, Action::Talk { .. }) {
                    log(e.detail, action.name(), level as u8, None);
                }
            }
            Event::XinputRawKeyRelease(e) => {
                if layer == Some(e.detail) {
                    layer = None;
                }
                if editing == Some(e.detail) {
                    editing = None;
                    daemon("edit-end");
                }
                if let Some((key, sentence, since, name)) = held.take_if(|h| h.0 == e.detail) {
                    let sentence = sentence || shift_held(&conn, root);
                    if sentence {
                        spawn(&exe, &["stop", "--sentence"]);
                    } else {
                        spawn(&exe, &["stop"]);
                    }
                    let name = if sentence && name == "talk" { "talk-sentence".into() } else { name };
                    log(key, name, Mode::Base as u8, Some(since.elapsed().as_secs_f64()));
                }
            }
            Event::MappingNotify(e) if e.request == Mapping::KEYBOARD => {
                silence(&conn, &bindings)?;
                conn.flush()?;
            }
            _ => {}
        }
        let now = mode(layer, &held);
        if now != shown {
            shown = now;
            let marked = if now == Mode::Layer { layer } else { held.as_ref().map(|h| h.0) };
            show_grid(&bindings, now, marked);
        }
    }
}

/// Clears the bound keys' keysyms unless already clear. Our own change
/// comes back as a MappingNotify, which then finds nothing to do.
fn silence(conn: &impl Connection, bindings: &Bindings) -> Result<()> {
    for &(key, _) in bindings.keys.iter().chain(&bindings.layer2).chain(&bindings.talking) {
        let map = conn.get_keyboard_mapping(key, 1)?.reply()?;
        if map.keysyms.iter().any(|&s| s != 0) {
            let none = vec![0; map.keysyms_per_keycode as usize];
            conn.change_keyboard_mapping(1, key, map.keysyms_per_keycode, &none)?.check()?;
        }
    }
    Ok(())
}

fn atom(conn: &impl Connection, name: &[u8]) -> Option<u32> {
    conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom)
}

/// The EWMH active window.
fn active_window(conn: &impl Connection, root: Window) -> Option<Window> {
    conn.get_property(false, root, atom(conn, b"_NET_ACTIVE_WINDOW")?, AtomEnum::WINDOW, 0, 1).ok()?.reply().ok()?
        .value32()?.next()
        .filter(|&w| w != 0)
}

fn window_pid(conn: &impl Connection, win: Window) -> Option<u32> {
    conn.get_property(false, win, atom(conn, b"_NET_WM_PID")?, AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?
        .value32()?.next()
}

fn window_class(conn: &impl Connection, win: Window) -> String {
    conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| crate::x11::wm_class_name(&r.value))
        .unwrap_or_default()
}

/// Raw events carry no modifier state; the pointer query does.
fn shift_held(conn: &impl Connection, root: Window) -> bool {
    conn.query_pointer(root)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.mask.contains(KeyButMask::SHIFT))
}

/// Runs `exe <args>` like a hotkey daemon would, reaped off-thread so the
/// event loop never waits on a slow stop.
fn spawn(exe: &std::path::Path, args: &[&str]) {
    let cmd = args.join(" ");
    match Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::null()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("dictate hotkey: {cmd}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_parse() {
        let keys = parse_bindings(DEFAULT_BINDINGS).unwrap();
        assert!(keys.contains(&(197, Action::Talk { sentence: false })));
        assert!(keys.contains(&(193, Action::Talk { sentence: true })));
        assert!(keys.contains(&(199, Action::Nav(Nav::Window(1)))));
        assert!(keys.contains(&(200, Action::Nav(Nav::Pane(1)))));
        assert!(parse_bindings(DEFAULT_LAYER2)
            .unwrap()
            .contains(&(192, Action::Run("learn"))));
    }

    #[test]
    fn key_names_and_chords() {
        assert_eq!(keycode("F13"), Some(191));
        assert_eq!(keycode("f24"), Some(202));
        assert_eq!(keycode("f12"), None);
        assert_eq!(keycode("200"), Some(200));
        assert_eq!(parse_bindings("f14=key:alt+BackSpace").unwrap(), vec![(192, Action::Key("alt+BackSpace".into()))]);
    }

    #[test]
    fn layer_two_falls_back_to_layer_one() {
        let b = Bindings {
            path: None,
            stamp: None,
            keys: parse_bindings("f13=layer f17=talk f14=key:alt+BackSpace").unwrap(),
            layer2: parse_bindings("f14=retry").unwrap(),
            talking: parse_bindings("f18=edit").unwrap(),
        };
        assert_eq!(b.action(192, Mode::Layer), Some((&Action::Run("retry"), Mode::Layer)));
        assert_eq!(b.action(192, Mode::Base), Some((&Action::Key("alt+BackSpace".into()), Mode::Base)));
        assert_eq!(b.action(195, Mode::Layer), Some((&Action::Talk { sentence: false }, Mode::Base)));
        assert_eq!(b.action(196, Mode::Talking), Some((&Action::Edit, Mode::Talking)));
        assert_eq!(b.action(196, Mode::Base), None);
        let grid = b.grid(Mode::Layer, Some(191));
        assert_eq!(grid["cells"][0]["kind"], "layer");
        assert_eq!(grid["cells"][1]["label"], "re-paste");
        assert_eq!(grid["cells"][1]["base"], "layer 1: ⌫ word");
        assert_eq!(grid["cells"][4]["kind"], "empty");
    }

    #[test]
    fn bad_bindings_are_errors() {
        assert!(parse_bindings("f14").is_err());
        assert!(parse_bindings("f14=jump").is_err());
        assert!(parse_bindings("f9=talk").is_err());
        assert!(parse_bindings("f14=key:").is_err());
    }
}
