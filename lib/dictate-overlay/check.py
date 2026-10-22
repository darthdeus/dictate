"""Run with the overlay's Python runtime; requires Xvfb and xdotool.
Uses an isolated X server. Never touches the real desktop, microphone or clipboard.
"""
import importlib.util
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace


def wait_for(fn, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = fn()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("Timed out waiting for X11 state")


sys.dont_write_bytecode = True
source = Path(__file__).with_name("overlay.py")
with tempfile.TemporaryDirectory() as tmp:
    tmp = Path(tmp)
    processes = []
    try:
        server = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0", "1600x900x24"], stdout=subprocess.PIPE)
        processes.append(server)
        display = ":" + server.stdout.readline().decode().strip()
        env = {**os.environ, "DISPLAY": display, "GDK_BACKEND": "x11", "NO_AT_BRIDGE": "1"}

        def xd(*args):
            return subprocess.run(["xdotool", *map(str, args)], env=env, text=True, capture_output=True).stdout.strip()

        target = subprocess.Popen([sys.executable, "-c", '''import gi, sys
from pathlib import Path
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, Gdk
w = Gtk.Window(title="Focus sentinel")
w.set_default_size(1600, 900)
box = Gtk.EventBox()
box.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
box.connect("button-press-event", lambda *_: Path(sys.argv[1]).touch())
w.add(box)
w.show_all()
Gtk.main()
''', str(tmp/"clicked")], env=env)
        processes.append(target)
        wid = wait_for(lambda: xd("search", "--onlyvisible", "--name", "Focus sentinel"))
        xd("windowfocus", wid)
        xd("mousemove", 800, 450, "click", 1)
        wait_for(lambda: (tmp/"clicked").exists())
        (tmp/"clicked").unlink()
        state = tmp/"state.json"

        def update(phase, text, duration=10, **extra):
            new = tmp/"new.json"
            new.write_text(json.dumps({**dict(phase=phase, label=phase.title(), text=text,
                expires=time.time()+duration, duration=duration, audio=str(tmp/"audio.pcm"), channels=10, channel=0),
                **extra}))
            new.replace(state)

        update("recording", "A quieter panel, with room to follow your thoughts.")
        overlay = subprocess.Popen([sys.executable, str(source), str(state)], env=env)
        processes.append(overlay)
        hud = wait_for(lambda: xd("search", "--onlyvisible", "--class", "dictate-osd"))
        time.sleep(0.3)
        assert xd("getwindowfocus") == wid, "Mapping stole focus"
        xd("mousemove", 800, 450, "click", 1)
        wait_for(lambda: (tmp/"clicked").exists())
        assert xd("getwindowfocus") == wid, "Click-through changed focus"
        update("processing", "Příliš žluťoučký kůň — 日本語 — <literal markup> & long text. "*30)
        time.sleep(0.3)
        assert xd("search", "--onlyvisible", "--class", "dictate-osd") == hud
        assert xd("getwindowfocus") == wid, "Updating stole focus"
        geometry = dict(line.split("=", 1) for line in xd("getwindowgeometry", "--shell", hud).splitlines())
        assert 252 < int(geometry["HEIGHT"]) <= 675, geometry
        (tmp/"clicked").unlink()
        xd("mousemove", 800, 450, "click", 1)
        wait_for(lambda: (tmp/"clicked").exists())
        assert xd("getwindowfocus") == wid, "Resizing stole focus"
        if os.getenv("DICTATE_CHECK_IMAGE"):
            subprocess.run(["magick", "import", "-display", display, "-window", hud,
                            os.environ["DICTATE_CHECK_IMAGE"]], env=env, check=True)
        state.write_text("invalid JSON")
        time.sleep(0.1)
        assert overlay.poll() is None, "Malformed message crashed renderer"
        update("success", "Ready", 0.3)
        wait_for(lambda: not xd("search", "--onlyvisible", "--class", "dictate-osd"))
        update("error", "Check the microphone.")
        assert wait_for(lambda: xd("search", "--onlyvisible", "--class", "dictate-osd")) == hud
        time.sleep(0.1)
        assert "WIDTH=480" in xd("getwindowgeometry", "--shell", hud)  # errors show as the compact card
        assert xd("getwindowfocus") == wid, "Remapping stole focus"
        # A finished dictation with a correction: one card, the changes above the stats.
        update("success", "Okay, can you look at the dictate diff in CLAUDE.md and run a sub-agent?", 30,
               label="Pasted", raw="Okay, can you look at the dictate div in Claude.md and run a sub agent?",
               stats="total\t0.31\ts\nfinal pass\tin pause\t\ncorrection\t0.18\ts\naudio\t6.2\ts\n2026-10-22_11-00-00-1-0",
               engine="Rust \u00b7 final: whisper large-v3-turbo + qwen3.8-27b")
        card = wait_for(lambda: xd("search", "--onlyvisible", "--class", "dictate-osd"))
        assert len(card.split()) == 1, card
        time.sleep(0.4)
        assert "WIDTH=480" in xd("getwindowgeometry", "--shell", card)
        assert xd("getwindowfocus") == wid, "Card stole focus"
        if os.getenv("DICTATE_CHECK_DIFF_IMAGE"):
            subprocess.run(["magick", "import", "-display", display, "-window", card,
                            os.environ["DICTATE_CHECK_DIFF_IMAGE"]], env=env, check=True)
        (tmp/"clicked").unlink(missing_ok=True)
        xd("mousemove", 1600-24-240, 60, "click", 1)
        wait_for(lambda: not xd("search", "--onlyvisible", "--class", "dictate-osd"), timeout=3)
        assert not (tmp/"clicked").exists(), "Click on the card reached the window below"
        assert xd("getwindowfocus") == wid, "Dismissing the card changed focus"
        update("processing", "Next one")
        assert len(wait_for(lambda: xd("search", "--onlyvisible", "--class", "dictate-osd")).split()) == 1

        spec = importlib.util.spec_from_file_location("overlay", source)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        def changed(previous, current):
            encoded = current.encode("utf-8")
            return [encoded[a:b].decode("utf-8") for a, b in module.changed_words(previous, current)]

        assert changed("keep these words", "keep these words steady") == ["steady"]
        assert changed("a quiet panel here", "a calmer panel here") == ["calmer"]
        assert changed("same words", "same words") == []
        assert changed("remove this word", "remove word") == []
        assert changed("Příliš 日本語", "Příliš krásné 日本語") == ["krásné"]
        assert module.diff_snippets("the dictate div in Claude.md", "the dictate diff in CLAUDE.md") == \
            ([[("ctx", "the dictate"), ("old", "div"), ("new", "diff"), ("ctx", "in"),
               ("old", "Claude.md"), ("new", "CLAUDE.md")]], 0)
        long = " ".join(f"w{i}" for i in range(40))
        snippets, more = module.diff_snippets(long.replace("w20", "x"), long, context=2)
        assert snippets == [[("ctx", "\u2026"), ("ctx", "w18 w19"), ("old", "x"), ("new", "w20"),
                             ("ctx", "w21 w22"), ("ctx", "\u2026")]] and more == 0
        assert module.diff_snippets("same", "same") == ([], 0)
        many = module.diff_snippets(long.replace("w5 ", "a ").replace("w15", "b").replace("w25", "c")
                                    .replace("w35", "d").replace("w38", "e"), long, limit=3)
        assert len(many[0]) == 3 and many[1] == 1, many
        text, ranges = module.diff_text(*module.diff_snippets("a div b", "a diff b"))
        assert text == "a div diff b" and [text.encode()[s:e].decode() for s, e, _ in ranges] == ["div", "diff"]
        assert module.diff_text([], 2) == ("\n+2 more", [])
        assert changed("the dictate div in Claude.md", "the dictate diff in CLAUDE.md") == ["diff", "CLAUDE.md"]
        recording = dict(phase="recording", text="Keep this preview")
        processing = module.next_state(recording, dict(phase="processing", text=""))
        assert processing["text"] == "Keep this preview"
        assert module.next_state(processing, dict(phase="processing", text=""))["text"] == "Keep this preview"
        assert module.next_state(processing, dict(phase="processing", text="Corrected text"))["text"] == "Corrected text"
        assert module.next_state(processing, dict(phase="recording", text=""))["text"] == ""
        assert module.next_state({}, dict(phase="processing", text=""))["text"] == ""
        assert module.next_state(dict(phase="success", text="Old session"), dict(phase="processing", text=""))["text"] == ""
        pcm = tmp/"audio.pcm"
        pcm.write_bytes(b"".join(struct.pack("<10h", int(5000*math.sin(i/20)), *([0]*9)) for i in range(2400)))
        model = SimpleNamespace(state=dict(phase="recording", audio=str(pcm), channels=10, channel=0),
                                levels=[0.0]*36, last_audio=None)
        module.Overlay.sample_audio(model)
        peak = model.levels[-1]
        assert peak > 0.5, "Meter missed audio on selected channel"
        module.Overlay.sample_audio(model)
        assert 0 < model.levels[-1] < peak, "Stalled capture should decay"
        model.state["channel"] = 1
        model.last_audio = None
        model.levels = [0.0]*36
        module.Overlay.sample_audio(model)
        assert model.levels[-1] == 0, "Meter read the wrong channel"
        print("PASS: focus on map/update/remap; clicks reach underlying window; reuse; expiry; malformed state; audio channel and decay; transcript continuity; changed-word UTF-8 ranges; correction diff in the card; click dismisses the card")
    finally:
        for process in reversed(processes):
            process.terminate()
            process.wait()
