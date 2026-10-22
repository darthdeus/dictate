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


def corrections(raw, text):
    """What the correction changed: ("before", "after") word runs, in order."""
    before, after = raw.split(), text.split()
    matcher = SequenceMatcher(None, before, after, autojunk=False)
    return [(" ".join(before[i:j]), " ".join(after[k:l]))
            for op, i, j, k, l in matcher.get_opcodes() if op != "equal"]


def wants(role, state):
    """The card window shows every phase; the text window only finished dictations."""
    return role == "card" or (state.get("phase") == "success" and bool(state.get("text")))


def next_state(previous, incoming):
    state = dict(incoming)
    if (state.get("phase") == "processing" and not state.get("text")
            and previous.get("phase") in ("recording", "processing")):
        state["text"] = previous.get("text", "") + previous.get("tentative", "")
    return state


def compact(state, role="card"):
    """Finished and failed dictations show as a small card at the top right;
    the text window shows the finished text as the full panel instead."""
    return role == "card" and state.get("phase") in ("success", "error")


COMPACT_W = 480
# The success card shows only how the dictation went (the text is already
# pasted), large enough to read at a glance.
STATS_SIZE = 22
LABEL_SIZE = 17
FOOT_SIZE = 14
ROW_H = 34
DIM = (0.48, 0.54, 0.62)
BRIGHT = (0.89, 0.92, 0.96)


def card_rows(state):
    """Stats lines "label<TAB>value<TAB>unit" become table rows; others are notes."""
    lines = state.get("stats", "").splitlines()
    rows = [line.split("\t")+["", ""] for line in lines if "\t" in line]
    notes = [line for line in lines if line and "\t" not in line]
    return [r[:3] for r in rows], notes


def card_text(state):
    return "" if state.get("phase") == "success" else state.get("text", "")


def rounded(cr, x, y, w, h, r):
    cr.new_sub_path()
    for cx, cy, start in ((x+w-r, y+r, -90), (x+w-r, y+h-r, 0),
                          (x+r, y+h-r, 90), (x+r, y+r, 180)):
        cr.arc(cx, cy, r, math.radians(start), math.radians(start+90))
    cr.close_path()


class Overlay(Gtk.Window):
    def __init__(self, path, role="card"):
        # POPUP is override-redirect: i3 never manages/focuses this window.
        super().__init__(type=Gtk.WindowType.POPUP)
        self.set_wmclass("dictate-osd", "dictate-osd")
        self.set_title("Dictation")
        self.set_accept_focus(False)
        self.set_focus_on_map(False)
        self.set_keep_above(True)
        self.set_skip_taskbar_hint(True)
        self.set_skip_pager_hint(True)
        self.set_decorated(False)
        self.path = Path(path)
        self.role = role
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
        self.size = max(18, min(40, int(os.getenv("DICTATE_OSD_SIZE", "28"))))
        self.motion = os.getenv("DICTATE_OSD_MOTION", "1") != "0"
        self.set_default_size(self.w, self.h)
        self.area = Gtk.DrawingArea()
        self.area.connect("draw", self.draw)
        self.add(self.area)
        self.connect("realize", self.realized)
        self.connect("size-allocate", self.allocated)
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

    def allocated(self, _, allocation):
        if allocation.height != self.h:
            self.h = allocation.height
            if self.get_realized():
                self.reshape()

    def reshape(self):
        # Rebuild the silhouette after resizing; preserve the empty input shape.
        win = self.get_window()
        region = cairo.Region()
        radius = 24
        for y in range(self.h):
            dy = max(radius-y-0.5, y-(self.h-radius)+0.5, 0)
            inset = math.ceil(radius-math.sqrt(max(0, radius*radius-dy*dy)))
            region.union(cairo.RectangleInt(inset, y, self.w-2*inset, 1))
        win.shape_combine_region(region, 0, 0)

    def position(self):
        display = self.get_display()
        active = self.get_screen().get_active_window()
        if active:
            monitor = display.get_monitor_at_window(active)
        else:
            _, x, y = display.get_default_seat().get_pointer().get_position()
            monitor = display.get_monitor_at_point(x, y)
        rect = monitor.get_geometry()
        self.monitor_rect = rect
        self.w = COMPACT_W if compact(self.state, self.role) else self.full_w
        if compact(self.state, self.role):
            self.x = rect.x + rect.width - self.w - 24
            self.base_y = rect.y + 24
        else:
            self.x = rect.x + (rect.width-self.w)//2
            if self.role == "text":
                # Beside the stats card (top right), never under it.
                self.x = max(rect.x+24, min(self.x, rect.x+rect.width-COMPACT_W-48-self.w))
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
            vw, vh = value.get_pixel_size()
            lh = label.get_pixel_size()[1]
            base = row_y + vh  # align label/unit baseline-ish with the number
            for layout, x, top, color in ((label, 32, base-lh-1, DIM),
                                          (value, right-vw, row_y, accent if i == 0 else BRIGHT),
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
        small = compact(self.state, self.role)
        layout = self.area.create_pango_layout(card_text(self.state) if compact(self.state, self.role) else self.state.get("text", ""))
        font = Pango.FontDescription()
        font.set_family(os.getenv("DICTATE_OSD_FONT", "DejaVu Sans"))
        font.set_absolute_size((18 if small else self.size)*Pango.SCALE)
        layout.set_font_description(font)
        layout.set_width((self.w-64)*Pango.SCALE)
        layout.set_wrap(Pango.WrapMode.WORD_CHAR)
        _, text_height = layout.get_pixel_size()
        if small:
            rows, notes = card_rows(self.state)
            foot = len(rows)*ROW_H
            foot += self.measure(self.card_note(notes), FOOT_SIZE)+14 if self.card_note(notes) else 0
            if not card_text(self.state):
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
                if not wants(self.role, state):
                    self.state = state
                    if self.get_visible():
                        self.hide()
                elif state["expires"] > time.time():
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
                    if self.get_visible() and compact(state, self.role) != compact(previous, self.role):
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
        elif self.role == "card" and now-self.last_seen > 60:
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
             dim_from=None):
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
        self.text(cr, self.state.get("label", "Ready").upper(), 54, 21, 15, accent, self.w-90, bold=True)
        rows, notes = card_rows(self.state)
        note = self.card_note(notes)
        note_h = self.measure(note, FOOT_SIZE) if note else 0
        body = card_text(self.state)
        if body:
            self.text(cr, body, 32, 56, 18, BRIGHT, self.w-64, height=self.h-86-len(rows)*ROW_H-note_h)
        self.draw_rows(cr, rows, self.h-20-note_h-(14 if note else 0)-len(rows)*ROW_H, accent)
        if note:
            self.text(cr, note, 32, self.h-20-note_h, FOOT_SIZE, DIM, self.w-64, height=note_h)

    def draw(self, _, cr):
        now = time.monotonic()
        phase = self.state.get("phase", "recording")
        accent = {"recording": (0.40, 0.88, 0.73), "processing": (0.65, 0.64, 1.0),
                  "success": (0.40, 0.88, 0.73), "error": (1.0, 0.63, 0.46)}[phase]
        if compact(self.state, self.role):
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
        fixes = corrections(self.state.get("raw", ""), self.state.get("text", "")) \
            if self.role == "text" and self.state.get("raw") else []
        label = self.state.get("label", "Listening")
        self.text(cr, label.upper(), 54, 24, 17, accent, self.w-240, bold=True)
        if phase == "recording":
            elapsed = int(now-self.born)
            meta = f"{elapsed//60}:{elapsed%60:02d}  /  LIVE"
        elif phase == "processing":
            meta = "WORKING"
        else:
            meta = f"{len(fixes)} FIXED" if fixes else "DICTATION"
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
        if fixes:
            # Corrected words stay in the accent colour for the whole display.
            changes = (changed_words(self.state["raw"], self.state.get("text", "")), accent)
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
        if self.role == "text":
            pairs = ["%s \u2192 %s" % (a or "\u2205", b or "\u2205") for a, b in fixes]
            engine = "fixed: " + "  \u00b7  ".join(pairs) if pairs else "no corrections"
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


if __name__ == "__main__":
    overlay = Overlay(sys.argv[1])
    # Second window: after release, the pasted text at the top with the
    # corrected words highlighted, next to the stats card.
    text_panel = Overlay(sys.argv[1], role="text")
    Gtk.main()
