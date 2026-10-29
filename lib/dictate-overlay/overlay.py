"""Compositor-free, click-through dictation HUD. GTK owns one double-buffered window."""
import array
from difflib import SequenceMatcher
import json
import math
import os
import re
from pathlib import Path
import sys
import time

import cairo
import gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Gdk, GLib, Gtk, Pango, PangoCairo


def changed_words(previous, current):
    """UTF-8 byte ranges of inserted/replaced words, for Pango attributes."""
    before = list(re.finditer(r"\S+", previous))
    after = list(re.finditer(r"\S+", current))
    offsets = [0]
    for char in current:
        offsets.append(offsets[-1] + len(char.encode("utf-8")))
    matcher = SequenceMatcher(None, [w.group() for w in before],
                              [w.group() for w in after], autojunk=False)
    return [(offsets[after[j].start()], offsets[after[k-1].end()])
            for op, _, _, j, k in matcher.get_opcodes()
            if op in ("insert", "replace") and j < k]


def diff_snippets(raw, text, context=3, limit=4):
    """What the correction changed, as short snippets of ("ctx"|"old"|"new", words)
    parts with `context` words around each change; changes closer than that
    share a snippet. Returns (snippets, how many more were left out)."""
    before, after = raw.split(), text.split()
    changes = [(i, j, k, l) for op, i, j, k, l in
               SequenceMatcher(None, before, after, autojunk=False).get_opcodes() if op != "equal"]
    groups = []
    for change in changes:
        if groups and change[2]-groups[-1][-1][3] <= 2*context:
            groups[-1].append(change)
        else:
            groups.append([change])
    snippets = []
    for group in groups[:limit]:
        start, end = max(0, group[0][2]-context), min(len(after), group[-1][3]+context)
        parts = [("ctx", "\u2026")] if start > 0 else []
        pos = start
        for i, j, k, l in group:
            if k > pos:
                parts.append(("ctx", " ".join(after[pos:k])))
            if j > i:
                parts.append(("old", " ".join(before[i:j])))
            if l > k:
                parts.append(("new", " ".join(after[k:l])))
            pos = l
        if end > pos:
            parts.append(("ctx", " ".join(after[pos:end])))
        if end < len(after):
            parts.append(("ctx", "\u2026"))
        snippets.append(parts)
    return snippets, len(groups)-len(snippets)


def diff_text(snippets, more):
    """One line per snippet, plus UTF-8 byte ranges of the old/new parts."""
    text, ranges = "", []
    for n, parts in enumerate(snippets):
        for m, (kind, words) in enumerate(parts):
            text += ("\n" if n and not m else " " if m else "")
            start = len(text.encode("utf-8"))
            text += words
            if kind != "ctx":
                ranges.append((start, len(text.encode("utf-8")), kind))
    if more:
        text += f"\n+{more} more"
    return text, ranges


def next_state(previous, incoming):
    state = dict(incoming)
    if (state.get("phase") == "processing" and not state.get("text")
            and previous.get("phase") in ("recording", "processing")):
        state["text"] = previous.get("text", "") + previous.get("tentative", "")
    return state


def compact(state):
    """Finished and failed dictations show as a small card at the top right
    that a click dismisses."""
    return state.get("phase") in ("success", "captured", "error")


COMPACT_W = 480
# The success card shows only how the dictation went (the text is already
# pasted), large enough to read at a glance.
STATS_SIZE = 22
LABEL_SIZE = 17
FOOT_SIZE = 14
# What the correction changed, above the stats.
DIFF_SIZE = 15
ROW_H = 34
DIM = (0.48, 0.54, 0.62)
BRIGHT = (0.89, 0.92, 0.96)
# A row's optional fourth field colours its value (the mic level).
ROW_COLORS = {"ok": (0.40, 0.88, 0.73), "warn": (1.0, 0.75, 0.30), "bad": (1.0, 0.42, 0.42)}


def card_rows(state):
    """Stats lines "label<TAB>value<TAB>unit[<TAB>color]" become table rows;
    others are notes."""
    lines = state.get("stats", "").splitlines()
    rows = [line.split("\t")+["", "", ""] for line in lines if "\t" in line]
    notes = [line for line in lines if line and "\t" not in line]
    return [r[:4] for r in rows], notes


def card_text(state):
    """Errors show their message; a finished dictation only what the
    correction changed (the text itself is already pasted)."""
    if state.get("phase") != "success":
        return state.get("text", ""), []
    return diff_text(*diff_snippets(state.get("raw") or state.get("text", ""), state.get("text", "")))


def rounded(cr, x, y, w, h, r):
    cr.new_sub_path()
    for cx, cy, start in ((x+w-r, y+r, -90), (x+w-r, y+h-r, 0),
                          (x+r, y+h-r, 90), (x+r, y+r, 180)):
        cr.arc(cx, cy, r, math.radians(start), math.radians(start+90))
    cr.close_path()


def silhouette(w, h, radius=24):
    """Rounded-rectangle window shape: without a compositor the corners
    outside the drawing would show."""
    region = cairo.Region()
    for y in range(h):
        dy = max(radius-y-0.5, y-(h-radius)+0.5, 0)
        inset = math.ceil(radius-math.sqrt(max(0, radius*radius-dy*dy)))
        region.union(cairo.RectangleInt(inset, y, w-2*inset, 1))
    return region


def popup(win):
    """A window that never takes focus or clicks (see Overlay.__init__)."""
    win.set_wmclass("dictate-osd", "dictate-osd")
    win.set_accept_focus(False)
    win.set_focus_on_map(False)
    win.set_keep_above(True)
    win.set_skip_taskbar_hint(True)
    win.set_skip_pager_hint(True)
    win.set_decorated(False)


def monitor_rect(win):
    """Geometry of the monitor with the active window (else the pointer)."""
    display = win.get_display()
    active = win.get_screen().get_active_window()
    if active:
        monitor = display.get_monitor_at_window(active)
    else:
        _, x, y = display.get_default_seat().get_pointer().get_position()
        monitor = display.get_monitor_at_point(x, y)
    return monitor.get_geometry()


class Overlay(Gtk.Window):
    def __init__(self, path):
        # POPUP is override-redirect: i3 never manages/focuses this window.
        super().__init__(type=Gtk.WindowType.POPUP)
        popup(self)
        self.set_title("Dictation")
        self.path = Path(path)
        self.stamp = None
        self.state = {}
        self.changed_ranges = []
        self.born = self.changed = self.last_seen = time.monotonic()
        self.levels = [0.0] * 36
        self.last_audio = None
        self.frame = 0
        self.full_w = max(560, min(1600, int(os.getenv("DICTATE_OSD_WIDTH", "960"))))
        self.w = self.full_w
        self.h = 252
        self.target_h = 252
        self.monitor_rect = None
        self.accent = BRIGHT
        self.size = max(18, min(40, int(os.getenv("DICTATE_OSD_SIZE", "28"))))
        self.motion = os.getenv("DICTATE_OSD_MOTION", "1") != "0"
        self.set_default_size(self.w, self.h)
        self.area = Gtk.DrawingArea()
        self.area.connect("draw", self.draw)
        self.add(self.area)
        self.connect("realize", self.realized)
        self.connect("size-allocate", self.allocated)
        self.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
        self.connect("button-press-event", self.dismiss)
        GLib.timeout_add(33, self.tick)

    def realized(self, _):
        win = self.get_window()
        # Empty input shape lets clicks pass through, including with i3's
        # focus-follows-mouse. Set before the first map, not after showing.
        # Use Gtk.Widget's API so GTK preserves the region when mapping and
        # applying style; setting only Gdk.Window's region gets overwritten.
        self.input_shape_combine_region(cairo.Region())
        win.set_pass_through(True)
        self.reshape()

    def clickable(self, on):
        """Only the finished card takes clicks (to dismiss it); the live
        panel stays click-through."""
        self.input_shape_combine_region(None if on else cairo.Region())
        if self.get_realized():
            self.get_window().set_pass_through(not on)

    def dismiss(self, *_):
        self.hide()
        self.last_seen = time.monotonic()
        return True

    def allocated(self, _, allocation):
        if allocation.height != self.h:
            self.h = allocation.height
            if self.get_realized():
                self.reshape()

    def reshape(self):
        # Rebuild the silhouette after resizing; preserve the empty input shape.
        self.get_window().shape_combine_region(silhouette(self.w, self.h), 0, 0)

    def position(self):
        rect = monitor_rect(self)
        self.monitor_rect = rect
        self.w = COMPACT_W if compact(self.state) else self.full_w
        self.clickable(compact(self.state))
        if compact(self.state):
            self.x = rect.x + rect.width - self.w - 24
            self.base_y = rect.y + 24
        else:
            self.x = rect.x + (rect.width-self.w)//2
            # Top center, growing down: the middle of the screen is where you work.
            self.base_y = rect.y + 24
        if self.get_realized():
            self.reshape()
        self.y = min(self.base_y, rect.y+rect.height-24-self.target_h)
        self.move(self.x, self.y - (18 if self.motion else 0))

    def measure(self, text, size):
        layout = self.area.create_pango_layout(text)
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size(size*Pango.SCALE)
        layout.set_font_description(font)
        layout.set_width((self.w-64)*Pango.SCALE)
        layout.set_wrap(Pango.WrapMode.WORD_CHAR)
        return layout.get_pixel_size()[1]

    def card_note(self, notes):
        return "\n".join(notes+[self.state.get("engine", "")]).strip()

    def span(self, text, size, bold=False):
        layout = self.area.create_pango_layout(text)
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size(size*Pango.SCALE)
        font.set_weight(Pango.Weight.BOLD if bold else Pango.Weight.NORMAL)
        layout.set_font_description(font)
        return layout

    def draw_rows(self, cr, rows, y, accent):
        """Labels left, numbers right-aligned in one column, units after them."""
        if not rows:
            return y
        labels = [self.span(r[0], LABEL_SIZE) for r in rows]
        values = [self.span(r[1], STATS_SIZE, bold=True) for r in rows]
        units = [self.span(r[2], LABEL_SIZE) for r in rows]
        label_w = max(l.get_pixel_size()[0] for l in labels)
        value_w = max(v.get_pixel_size()[0] for v in values)
        right = 32 + label_w + 28 + value_w
        for i, (label, value, unit) in enumerate(zip(labels, values, units)):
            row_y = y + i*ROW_H
            tint = ROW_COLORS.get(rows[i][3], accent if i == 0 else BRIGHT)
            vw, vh = value.get_pixel_size()
            lh = label.get_pixel_size()[1]
            base = row_y + vh  # align label/unit baseline-ish with the number
            for layout, x, top, color in ((label, 32, base-lh-1, DIM),
                                          (value, right-vw, row_y, tint),
                                          (unit, right+7, base-lh-1, DIM)):
                cr.set_source_rgb(*color)
                cr.move_to(x, top)
                PangoCairo.show_layout(cr, layout)
            if i == 0 and len(rows) > 1:
                cr.set_source_rgba(*DIM, 0.35)
                cr.set_line_width(1)
                cr.move_to(32, row_y + ROW_H - 3)
                cr.line_to(self.w-32, row_y + ROW_H - 3)
                cr.stroke()
        return y + len(rows)*ROW_H

    def fit_text(self, reset=False):
        small = compact(self.state)
        body = card_text(self.state)[0]
        layout = self.area.create_pango_layout(body if small else self.state.get("text", ""))
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size(((DIFF_SIZE if self.state.get("phase") == "success" else 18) if small else self.size)*Pango.SCALE)
        layout.set_font_description(font)
        layout.set_width((self.w-64)*Pango.SCALE)
        layout.set_wrap(Pango.WrapMode.WORD_CHAR)
        _, text_height = layout.get_pixel_size()
        if small:
            rows, notes = card_rows(self.state)
            foot = len(rows)*ROW_H
            foot += self.measure(self.card_note(notes), FOOT_SIZE)+14 if self.card_note(notes) else 0
            if body:
                text_height += 10  # gap above the stats
            else:
                text_height = 0
            desired = min(int(self.monitor_rect.height*0.5), text_height + foot + 86)
            self.target_h = desired
            self.y = self.base_y
            self.resize(self.w, self.target_h)
            self.move(self.x, self.y)
            if self.get_realized():
                self.reshape()
            return
        limit = max(252, int(self.monitor_rect.height*0.75))
        desired = min(limit, max(252, text_height+139))
        # Don't shrink and shift paragraphs when the model revises wording.
        self.target_h = desired if reset else max(self.target_h, desired)
        self.y = min(self.base_y, self.monitor_rect.y+self.monitor_rect.height-24-self.target_h)
        self.resize(self.w, self.target_h)
        self.move(self.x, self.y)

    def tick(self):
        now = time.monotonic()
        try:
            stamp = self.path.stat().st_mtime_ns
            if stamp != self.stamp:
                state = json.loads(self.path.read_text())
                self.stamp = stamp
                if state["expires"] > time.time():
                    was_recording = self.state.get("phase") == "recording"
                    previous = self.state if self.get_visible() else {}
                    state = next_state(previous, state)
                    if state.get("text", "") != previous.get("text", ""):
                        self.changed_ranges = changed_words(previous.get("text", ""), state.get("text", ""))
                        self.changed = now
                    self.state = state
                    self.last_seen = now
                    if state["phase"] == "recording" and not was_recording:
                        self.born = now
                        self.levels = [0.0] * 36
                    reset = not self.get_visible() or (state["phase"] == "recording" and not was_recording)
                    if self.get_visible() and compact(state) != compact(previous):
                        # The panel becomes the card (or back): move it, as if new.
                        self.hide()
                    if not self.get_visible():
                        self.born = now
                        self.position()
                        self.fit_text(reset=True)
                        self.show_all()
                    else:
                        self.fit_text(reset=reset)
        except (OSError, ValueError, KeyError):
            pass
        if self.get_visible():
            remaining = self.state.get("expires", 0) - time.time()
            if remaining <= 0:
                self.hide()
                self.last_seen = now
            else:
                age = now-self.born
                offset = -18 * max(0, 1-age/0.22)**3
                if remaining < 0.16:
                    offset = 12 * (1-remaining/0.16)**2
                if self.motion:
                    self.move(self.x, self.y+round(offset))
                self.sample_audio()
                self.area.queue_draw()
                # Keep above newly mapped windows without changing input focus.
                if self.frame % 30 == 0:
                    self.get_window().raise_()
                self.frame += 1
        elif now-self.last_seen > 60:
            Gtk.main_quit()
            return False
        return True

    def sample_audio(self):
        target = 0.0
        if self.state.get("phase") == "recording":
            try:
                path = self.state["audio"]
                channels, channel = int(self.state["channels"]), int(self.state["channel"])
                size = os.stat(path).st_size
                frame = channels*2
                end = size-size % frame
                if (path, end) != self.last_audio and end > 0 and 0 <= channel < channels:
                    with open(path, "rb") as stream:
                        stream.seek(max(0, end-2400*frame))
                        raw = stream.read(min(end, 2400*frame))
                    samples = array.array("h", raw)[channel::channels]
                    if sys.byteorder != "little":
                        samples.byteswap()
                    rms = math.sqrt(sum(v*v for v in samples)/max(1, len(samples))) / 32768
                    target = max(0, min(1, (20*math.log10(max(rms, 1e-6))+60)/54))
                    self.last_audio = (path, end)
            except (OSError, KeyError, ValueError):
                pass
        # Fast attack, soft release; bars represent recent real audio, not a
        # fake waveform claiming the mic works when no data is arriving.
        value = max(target, self.levels[-1]*0.86)
        self.levels = self.levels[1:] + [value]

    def text(self, cr, text, x, y, size, color, width=None, lines=1, bold=False, height=None, changes=None,
             dim_from=None, diff=None):
        layout = PangoCairo.create_layout(cr)
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size(size*Pango.SCALE)
        font.set_weight(Pango.Weight.BOLD if bold else Pango.Weight.NORMAL)
        layout.set_font_description(font)
        layout.set_text(text, -1)  # literal text, never markup
        if width:
            layout.set_width(width*Pango.SCALE)
            layout.set_wrap(Pango.WrapMode.WORD_CHAR)
            layout.set_height(height*Pango.SCALE if height else -lines)
            layout.set_ellipsize(Pango.EllipsizeMode.START if self.state.get("phase") in ("recording", "processing") else Pango.EllipsizeMode.END)
        attributes = Pango.AttrList()
        if changes:
            ranges, tint = changes
            for start, end in ranges:
                attr = Pango.attr_foreground_new(*(round(c*65535) for c in tint))
                attr.start_index, attr.end_index = start, end
                attributes.insert(attr)
        for start, end, kind in diff or []:
            # Correction diff: replaced words struck through and dim, new ones in the accent.
            tint = DIM if kind == "old" else self.accent
            style = Pango.attr_strikethrough_new(True) if kind == "old" else Pango.attr_weight_new(Pango.Weight.BOLD)
            for attr in (style, Pango.attr_foreground_new(*(round(c*65535) for c in tint))):
                attr.start_index, attr.end_index = start, end
                attributes.insert(attr)
        if dim_from is not None:
            # Streaming preview: the tail after the committed text may still change.
            attr = Pango.attr_foreground_new(*(round(c*65535) for c in (0.48, 0.54, 0.62)))
            attr.start_index, attr.end_index = dim_from, len(text.encode("utf-8"))
            attributes.insert(attr)
        layout.set_attributes(attributes)
        cr.set_source_rgb(*color)
        cr.move_to(x, y)
        PangoCairo.show_layout(cr, layout)

    def draw_card(self, cr, accent):
        cr.set_source_rgb(0.055, 0.067, 0.09)
        cr.paint()
        rounded(cr, 1, 1, self.w-2, self.h-2, 23)
        cr.set_source_rgb(0.19, 0.22, 0.27)
        cr.set_line_width(1.5)
        cr.stroke()
        cr.set_source_rgb(*accent)
        cr.arc(37, 33, 5, 0, math.tau)
        cr.fill()
        body, ranges = card_text(self.state)
        label = self.state.get("label", "Ready").upper()
        if self.state.get("phase") == "success":
            fixed = sum(1 for _, _, kind in ranges if kind == "new")
            label += f"  \u00b7  {fixed} FIXED" if fixed else ""
        self.text(cr, label, 54, 21, 15, accent, self.w-90, bold=True)
        rows, notes = card_rows(self.state)
        note = self.card_note(notes)
        note_h = self.measure(note, FOOT_SIZE) if note else 0
        if body:
            self.accent = accent
            size = DIFF_SIZE if self.state.get("phase") == "success" else 18
            self.text(cr, body, 32, 56, size, BRIGHT if size == 18 else (0.70, 0.75, 0.82), self.w-64,
                      height=self.h-86-len(rows)*ROW_H-note_h, diff=ranges)
        self.draw_rows(cr, rows, self.h-20-note_h-(14 if note else 0)-len(rows)*ROW_H, accent)
        if note:
            self.text(cr, note, 32, self.h-20-note_h, FOOT_SIZE, DIM, self.w-64, height=note_h)

    def draw(self, _, cr):
        now = time.monotonic()
        phase = self.state.get("phase", "recording")
        accent = {"recording": (0.40, 0.88, 0.73), "processing": (0.65, 0.64, 1.0),
                  "success": (0.40, 0.88, 0.73), "captured": (0.40, 0.88, 0.73),
                  "error": (1.0, 0.63, 0.46)}[phase]
        if compact(self.state):
            self.draw_card(cr, accent)
            return False
        cr.set_source_rgb(0.055, 0.067, 0.09)
        cr.paint()
        rounded(cr, 1, 1, self.w-2, self.h-2, 23)
        cr.set_source_rgb(0.19, 0.22, 0.27)
        cr.set_line_width(1.5)
        cr.stroke()
        pulse = 0.8+0.2*math.sin(now*3) if self.motion else 1
        cr.set_source_rgba(*accent, pulse)
        cr.arc(37, 37, 5, 0, math.tau)
        cr.fill()
        label = self.state.get("label", "Listening")
        self.text(cr, label.upper(), 54, 24, 17, accent, self.w-240, bold=True)
        if phase == "recording":
            elapsed = int(now-self.born)
            meta = f"{elapsed//60}:{elapsed%60:02d}  /  LIVE"
        elif phase == "processing":
            meta = "WORKING"
        else:
            meta = "DICTATION"
        self.text(cr, meta, self.w-177, 25, 15, (0.48, 0.54, 0.62))
        body = self.state.get("text", "")
        tentative = self.state.get("tentative", "")
        dim_from = len(body.encode("utf-8")) if tentative else None
        body += tentative
        if not body:
            body = "Speak naturally…" if phase == "recording" else "Preparing your text…"
        fade = min(1, (now-self.changed)/0.14) if self.motion else 1
        color = tuple(a+(b-a)*fade for a, b in zip((0.30, 0.35, 0.41), (0.89, 0.92, 0.96)))
        changes = (self.changed_ranges, color) if fade < 1 else None
        self.text(cr, body, 32, 68, self.size, (0.89, 0.92, 0.96), self.w-64, 3,
                  height=self.h-139, changes=changes, dim_from=dim_from)
        cr.set_source_rgb(0.14, 0.17, 0.21)
        cr.rectangle(32, self.h-57, self.w-64, 1)
        cr.fill()
        hint = {"recording": "Release Caps Lock to transcribe", "processing": "Finishing your dictation",
                "success": "Ready", "error": "Check dictation status"}[phase]
        # Which implementation and models are working: Rust messages name
        # them; the Bash script sends none.
        engine = self.state.get("engine") or f"Bash \u00b7 {hint}"
        self.text(cr, engine, 32, self.h-39, 15, (0.48, 0.54, 0.62), self.w-64-(230 if phase == "recording" else 90))
        if phase == "recording":
            for i, value in enumerate(self.levels):
                height = 3+value*25
                rounded(cr, self.w-250+i*6, self.h-29-height/2, 3, height, 1.5)
                cr.set_source_rgba(*accent, 0.35+0.65*i/35)
                cr.fill()
        elif phase == "processing":
            for i in range(3):
                alpha = 0.3+0.7*(0.5+0.5*math.sin(now*5-i*0.8)) if self.motion else 0.8
                cr.set_source_rgba(*accent, alpha)
                cr.arc(self.w-72+i*14, self.h-29, 3, 0, math.tau)
                cr.fill()
        else:
            fraction = max(0, min(1, (self.state["expires"]-time.time())/self.state["duration"]))
            cr.set_source_rgba(*accent, 0.65)
            cr.rectangle(32, self.h-2, (self.w-64)*fraction, 2)
            cr.fill()
        return False


class LayerGrid(Gtk.Window):
    """The macro pad's second layer while its layer key is held: a 2x3 grid
    of what each button does now, right of the dictation panel. Driven by
    `layer.json` beside `state.json` (written by `dictate hotkey`):
    `{active, ts, cells: [{label, base, kind}] x 6}`, cells row by row from
    the top left; `kind` is `layer` (the held key), `action` or `empty`."""

    CELL_W, CELL_H, GAP, HEAD = 220, 78, 10, 46

    def __init__(self, overlay, path):
        super().__init__(type=Gtk.WindowType.POPUP)
        popup(self)
        self.set_title("Dictation layer")
        self.overlay = overlay
        self.path = Path(path)
        self.stamp = None
        self.state = {}
        self.w = 2*self.CELL_W+3*self.GAP
        self.h = self.HEAD+3*self.CELL_H+4*self.GAP
        self.set_default_size(self.w, self.h)
        area = Gtk.DrawingArea()
        area.connect("draw", self.draw)
        self.add(area)
        self.connect("realize", self.realized)
        GLib.timeout_add(33, self.tick)

    def realized(self, _):
        self.input_shape_combine_region(cairo.Region())
        self.get_window().set_pass_through(True)
        self.get_window().shape_combine_region(silhouette(self.w, self.h, 18), 0, 0)

    def tick(self):
        try:
            stamp = self.path.stat().st_mtime_ns
            if stamp != self.stamp:
                self.stamp = stamp
                self.state = json.loads(self.path.read_text())
                self.queue_draw()
        except (OSError, ValueError):
            self.state = {}
        # A listener that died mid-hold must not leave the grid up.
        active = self.state.get("active") and time.time()-self.state.get("ts", 0) < 30
        if active and not self.get_visible():
            rect = monitor_rect(self)
            panel_right = rect.x+(rect.width+self.overlay.full_w)//2
            x = min(panel_right+24, rect.x+rect.width-self.w-24)
            self.move(x, rect.y+24)
            self.show_all()
        elif not active and self.get_visible():
            self.hide()
        if self.get_visible():
            # Keeps the renderer from quitting idle while the grid is up.
            self.overlay.last_seen = time.monotonic()
        return True

    def label(self, cr, text, x, y, size, color, bold=False):
        layout = PangoCairo.create_layout(cr)
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size(size*Pango.SCALE)
        font.set_weight(Pango.Weight.BOLD if bold else Pango.Weight.NORMAL)
        layout.set_font_description(font)
        layout.set_text(text, -1)
        layout.set_width((self.CELL_W-28)*Pango.SCALE)
        layout.set_ellipsize(Pango.EllipsizeMode.END)
        cr.set_source_rgb(*color)
        cr.move_to(x, y)
        PangoCairo.show_layout(cr, layout)

    def draw(self, _, cr):
        accent = (0.65, 0.64, 1.0)
        cr.set_source_rgb(0.055, 0.067, 0.09)
        cr.paint()
        rounded(cr, 1, 1, self.w-2, self.h-2, 17)
        cr.set_source_rgb(0.19, 0.22, 0.27)
        cr.set_line_width(1.5)
        cr.stroke()
        self.label(cr, self.state.get("title", "LAYER 2"), 22, 14, LABEL_SIZE, accent, bold=True)
        for i, cell in enumerate(self.state.get("cells", [])[:6]):
            x = self.GAP+(i % 2)*(self.CELL_W+self.GAP)
            y = self.HEAD+self.GAP+(i//2)*(self.CELL_H+self.GAP)
            kind = cell.get("kind", "action")
            rounded(cr, x, y, self.CELL_W, self.CELL_H, 12)
            if kind == "layer":
                cr.set_source_rgba(*accent, 0.28)
            else:
                cr.set_source_rgb(0.09, 0.11, 0.14)
            cr.fill()
            color = DIM if kind == "empty" else (accent if kind == "layer" else BRIGHT)
            self.label(cr, cell.get("label", ""), x+14, y+12, 20, color, bold=kind != "empty")
            if cell.get("base"):
                self.label(cr, cell["base"], x+14, y+46, FOOT_SIZE, DIM)
        return False


if __name__ == "__main__":
    overlay = Overlay(sys.argv[1])
    grid = LayerGrid(overlay, Path(sys.argv[1]).with_name("layer.json"))
    Gtk.main()
