//! In-process X11: active window class, reading the CLIPBOARD selection, and
//! owning it (the `clipboard-owner` worker).
//!
//! Replaces per-delivery `xdotool getactivewindow getwindowclassname`,
//! `copyq clipboard` / `xclip -o` reads and `copyq copy` / `xclip` writes,
//! each a process spawn (copyq: ~40 ms, a Qt client start plus a server
//! round trip). The paste chord itself still goes through `xdotool key`.

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::connection::RequestConnection as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

/// Clipboard managers (CopyQ, Klipper) skip data offering this target with
/// the value `secret`, the same as `copyq copy` never entering CopyQ's
/// history: the dictation stays out of clipboard history while it is on the
/// clipboard. What it restores goes through copyq/xclip as before.
pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// CopyQ treats data carrying its owner format as its own: not added to
/// history and, with `copy_clipboard` on, not copied into PRIMARY. That is
/// how `copyq copy` data behaved; without it the dictation would replace
/// the PRIMARY selection.
pub const COPYQ_OWNER: &str = "application/x-copyq-owner";

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    utf8: Atom,
    text: Atom,
    mime_utf8: Atom,
    mime_plain: Atom,
    incr: Atom,
    hint: Atom,
    copyq_owner: Atom,
    net_active_window: Atom,
    /// Property our own window receives conversions into.
    transfer: Atom,
    /// Marks a `clipboard-owner` worker's window.
    dictate_owner: Atom,
    /// Private target: the clipboard that worker will restore.
    dictate_prev: Atom,
}

pub struct X {
    conn: RustConnection,
    root: Window,
    win: Window,
    atoms: Atoms,
}

impl X {
    pub fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("X display unavailable")?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT, win, root, 0, 0, 1, 1, 0,
            WindowClass::INPUT_ONLY, COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        // Pipeline all interns, then collect.
        let names = [
            "CLIPBOARD", "TARGETS", "TIMESTAMP", "UTF8_STRING", "TEXT",
            "text/plain;charset=utf-8", "text/plain", "INCR", PASSWORD_HINT,
            "_NET_ACTIVE_WINDOW", "DICTATE_SELECTION", "_DICTATE_CLIPBOARD_OWNER", "_DICTATE_PREV",
            COPYQ_OWNER,
        ];
        let cookies = names
            .iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut a = Vec::with_capacity(names.len());
        for c in cookies {
            a.push(c.reply()?.atom);
        }
        let atoms = Atoms {
            clipboard: a[0], targets: a[1], timestamp: a[2], utf8: a[3], text: a[4],
            mime_utf8: a[5], mime_plain: a[6], incr: a[7], hint: a[8],
            net_active_window: a[9], transfer: a[10], dictate_owner: a[11], dictate_prev: a[12], copyq_owner: a[13],
        };
        conn.flush()?;
        Ok(Self { conn, root, win, atoms })
    }

    /// `xdotool getactivewindow getwindowclassname`: the class half of the
    /// focused window's WM_CLASS, or "" when there is no EWMH active window.
    pub fn active_window_class(&self) -> String {
        let active = self
            .conn
            .get_property(false, self.root, self.atoms.net_active_window, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        let Some(w) = active.filter(|&w| w != NONE) else { return String::new() };
        let class = self
            .conn
            .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok());
        class
            .map(|r| wm_class_name(&r.value))
            .unwrap_or_default()
    }

    /// Next event, waiting at most until `deadline`.
    fn next_event(&self, deadline: Instant) -> Result<Option<Event>> {
        loop {
            if let Some(e) = self.conn.poll_for_event()? {
                return Ok(Some(e));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }

    /// The CLIPBOARD as UTF-8 text: `Ok(None)` when nobody owns it or the
    /// owner has no text. Events that are not ours go to `other` (the owner
    /// worker must keep serving while it reads).
    pub fn read_clipboard_with(
        &self,
        timeout: Duration,
        other: &mut dyn FnMut(&X, Event),
    ) -> Result<Option<String>> {
        let owner = self.conn.get_selection_owner(self.atoms.clipboard)?.reply()?.owner;
        if owner == NONE {
            return Ok(None);
        }
        self.read_target(self.atoms.utf8, timeout, other)
    }

    /// The clipboard to restore after our paste. While another dictation's
    /// worker still holds it (back-to-back dictations), that is the
    /// clipboard *it* will restore, not its dictation.
    pub fn read_restorable(&self, timeout: Duration) -> Result<Option<String>> {
        let owner = self.conn.get_selection_owner(self.atoms.clipboard)?.reply()?.owner;
        if owner == NONE {
            return Ok(None);
        }
        let is_worker = self
            .conn
            .get_property(false, owner, self.atoms.dictate_owner, AtomEnum::ANY, 0, 1)?
            .reply()
            .is_ok_and(|r| r.type_ != NONE);
        if is_worker {
            if let Ok(Some(prev)) = self.read_target(self.atoms.dictate_prev, timeout, &mut |_, _| {}) {
                return Ok(Some(prev));
            }
        }
        self.read_target(self.atoms.utf8, timeout, &mut |_, _| {})
    }

    fn read_target(&self, target: Atom, timeout: Duration, other: &mut dyn FnMut(&X, Event)) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        self.conn.delete_property(self.win, self.atoms.transfer)?;
        self.conn.convert_selection(self.win, self.atoms.clipboard, target, self.atoms.transfer, CURRENT_TIME)?;
        self.conn.flush()?;
        loop {
            match self.next_event(deadline)? {
                None => bail!("clipboard owner did not answer"),
                Some(Event::SelectionNotify(e)) if e.requestor == self.win && e.selection == self.atoms.clipboard => {
                    if e.property == NONE {
                        return Ok(None);
                    }
                    return self.take_transfer(deadline, other).map(Some);
                }
                Some(e) => other(self, e),
            }
        }
    }

    pub fn read_clipboard(&self, timeout: Duration) -> Result<Option<String>> {
        self.read_clipboard_with(timeout, &mut |_, _| {})
    }

    /// Reads (and deletes) the transfer property, following INCR.
    fn take_transfer(&self, deadline: Instant, other: &mut dyn FnMut(&X, Event)) -> Result<String> {
        let r = self.conn.get_property(true, self.win, self.atoms.transfer, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
        self.conn.flush()?;
        if r.type_ != self.atoms.incr {
            return Ok(String::from_utf8_lossy(&r.value).into_owned());
        }
        let mut data = Vec::new();
        loop {
            match self.next_event(deadline)? {
                None => bail!("clipboard transfer timed out"),
                Some(Event::PropertyNotify(e))
                    if e.window == self.win
                        && e.atom == self.atoms.transfer
                        && e.state == x11rb::protocol::xproto::Property::NEW_VALUE =>
                {
                    let r = self.conn.get_property(true, self.win, self.atoms.transfer, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
                    self.conn.flush()?;
                    if r.value.is_empty() {
                        return Ok(String::from_utf8_lossy(&data).into_owned());
                    }
                    data.extend_from_slice(&r.value);
                }
                Some(Event::PropertyNotify(_)) => {}
                Some(e) => other(self, e),
            }
        }
    }

    /// A server timestamp (ICCCM: never own a selection at CurrentTime).
    fn server_time(&self) -> Result<u32> {
        self.conn.change_property8(PropMode::APPEND, self.win, self.atoms.transfer, AtomEnum::STRING, &[])?;
        self.conn.flush()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.next_event(deadline)? {
                None => bail!("no PropertyNotify for the timestamp"),
                Some(Event::PropertyNotify(e)) if e.window == self.win => return Ok(e.time),
                Some(_) => {}
            }
        }
    }

    /// Largest text `ClipboardOwner` serves in one property (no INCR on the
    /// sending side); larger dictations go through copyq/xclip.
    pub fn max_serve_bytes(&self) -> usize {
        self.conn.maximum_request_bytes().saturating_sub(1024)
    }
}

/// Class half of a WM_CLASS value (`instance\0class\0`).
pub fn wm_class_name(value: &[u8]) -> String {
    let mut parts = value.split(|&b| b == 0);
    let _instance = parts.next();
    parts
        .next()
        .map(|c| String::from_utf8_lossy(c).trim().to_string())
        .unwrap_or_default()
}

/// Our text as the CLIPBOARD selection: answers conversions for as long as
/// we own it.
pub struct ClipboardOwner {
    pub x: X,
    data: Served,
    owned: bool,
}

/// What a `ClipboardOwner` answers with.
struct Served {
    text: String,
    /// What this worker will restore, for a newer worker (`read_restorable`).
    prev: Option<String>,
    time: u32,
}

/// Latin-1 for the legacy STRING target; what doesn't fit becomes '?'.
fn latin1(text: &str) -> Vec<u8> {
    text.chars().map(|c| if (c as u32) < 256 { c as u32 as u8 } else { b'?' }).collect()
}

impl ClipboardOwner {
    /// Takes the CLIPBOARD and confirms the server lists us as its owner,
    /// so a paste sent after this returns cannot get the previous contents.
    pub fn take(x: X, text: String, prev: Option<String>) -> Result<Self> {
        if text.len() > x.max_serve_bytes() {
            bail!("text too large to serve without INCR");
        }
        x.conn.change_property8(PropMode::REPLACE, x.win, x.atoms.dictate_owner, AtomEnum::STRING, b"1")?;
        let time = x.server_time()?;
        x.conn.set_selection_owner(x.win, x.atoms.clipboard, time)?;
        let owner = x.conn.get_selection_owner(x.atoms.clipboard)?.reply()?.owner;
        if owner != x.win {
            bail!("could not take the clipboard");
        }
        Ok(Self { x, data: Served { text, prev, time }, owned: true })
    }

    /// Whether we still own the CLIPBOARD (no SelectionClear seen yet).
    pub fn owned(&self) -> bool {
        self.owned
    }

    /// Handles one event: answers conversion requests, notes losing the
    /// selection.
    pub fn handle(&mut self, e: Event) {
        handle_event(&self.x, &self.data, &mut self.owned, e);
    }

    /// Serves until `deadline` or until `stop()` says so; `stop` is asked
    /// at least every few milliseconds, not only when an event arrives.
    pub fn serve_until(&mut self, deadline: Instant, mut stop: impl FnMut(&Self) -> bool) -> Result<()> {
        while !stop(self) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(e) = self.x.next_event(deadline.min(now + Duration::from_millis(2)))? {
                self.handle(e);
            }
        }
        Ok(())
    }

    /// Answers whatever is already queued (refusing once we lost the
    /// selection), so no requestor is left waiting when we exit.
    pub fn drain(&mut self) {
        while let Ok(Some(e)) = self.x.conn.poll_for_event() {
            self.handle(e);
        }
    }

    /// The clipboard's current text, serving our own while we wait (we may
    /// still be the owner).
    pub fn read_clipboard(&mut self, timeout: Duration) -> Result<Option<String>> {
        let data = &self.data;
        let mut owned = self.owned;
        let r = self.x.read_clipboard_with(timeout, &mut |x, e| handle_event(x, data, &mut owned, e));
        self.owned = owned;
        r
    }
}

fn handle_event(x: &X, data: &Served, owned: &mut bool, e: Event) {
    match e {
        Event::SelectionClear(c) if c.selection == x.atoms.clipboard && c.owner == x.win => *owned = false,
        Event::SelectionRequest(r) => {
            let _ = answer(x, data, *owned, &r);
        }
        _ => {}
    }
}

fn answer(x: &X, data: &Served, owned: bool, r: &SelectionRequestEvent) -> Result<()> {
    let a = &x.atoms;
    let (text, time) = (data.text.as_str(), data.time);
    // Obsolete clients pass no property: use the target (ICCCM 2.2).
    let prop = if r.property == NONE { r.target } else { r.property };
    let ok = owned && r.selection == a.clipboard && {
        let t = r.target;
        if t == a.targets {
            let list = [a.targets, a.timestamp, a.utf8, a.mime_utf8, AtomEnum::STRING.into(), a.text, a.mime_plain, a.hint, a.copyq_owner];
            x.conn.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &list)?;
            true
        } else if t == a.timestamp {
            x.conn.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::INTEGER, &[time])?;
            true
        } else if t == a.utf8 || t == a.text {
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.utf8, text.as_bytes())?;
            true
        } else if t == a.mime_utf8 {
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.mime_utf8, text.as_bytes())?;
            true
        } else if t == Atom::from(AtomEnum::STRING) || t == a.mime_plain {
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, t, &latin1(text))?;
            true
        } else if t == a.dictate_prev && data.prev.is_some() {
            let prev = data.prev.as_deref().unwrap_or_default();
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.utf8, prev.as_bytes())?;
            true
        } else if t == a.copyq_owner {
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.copyq_owner, b"dictate")?;
            true
        } else if t == a.hint {
            x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.hint, b"secret")?;
            true
        } else {
            false
        }
    };
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: r.time,
        requestor: r.requestor,
        selection: r.selection,
        target: r.target,
        property: if ok { prop } else { NONE },
    };
    x.conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
    x.conn.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wm_class_takes_class_half() {
        assert_eq!(wm_class_name(b"xterm\0XTerm\0"), "XTerm");
        assert_eq!(wm_class_name(b"Alacritty\0Alacritty\0"), "Alacritty");
        assert_eq!(wm_class_name(b"only-instance\0"), "");
        assert_eq!(wm_class_name(b""), "");
    }

    #[test]
    fn latin1_replaces_wide_chars() {
        assert_eq!(latin1("caf\u{e9} \u{17e}"), b"caf\xe9 ?".to_vec());
    }
}
