#!/usr/bin/env python3
"""Exercise real CLI draft capture on Xvfb and a private tmux server.

Requires installed/authenticated Codex/Claude, Xvfb, xterm, xdotool and xprop.
No prompt is submitted: input is only bracketed paste, Ctrl+C and Ctrl+G.
All dictation/runtime state and all test panes are isolated. Delivery provenance
is seeded from fixtures; input uses tmux bracketed paste, not the real clipboard.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import subprocess
import tempfile
import time
import wave


ROOT = Path(__file__).resolve().parents[1]


def wait(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise RuntimeError("timed out waiting for test UI")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--apps", nargs="+", choices=["codex", "claude"], default=["codex", "claude"])
    args = ap.parse_args()
    binary = ROOT / "target/release/dictate"
    launcher = Path(os.environ.get("DICTATE_LAUNCHER", ROOT / "bin/dictate-editor"))
    with tempfile.TemporaryDirectory(prefix="dictate-learn-cli-") as temporary:
        base = Path(temporary)
        trace = base / "editor.trace"
        editor = base / "editor"
        editor.write_text("#!/bin/sh\n"
                          + "printf '%s\\n' \"$TMUX_PANE $XDG_STATE_HOME\" >> " + shlex.quote(str(trace)) + "\n"
                          + "exec " + shlex.quote(str(launcher)) + ' "$@"\n')
        editor.chmod(0o755)
        env = os.environ.copy()
        for key in ("TMUX", "TMUX_PANE", "CLAUDECODE", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID"):
            env.pop(key, None)
        fake = base / "bin"
        fake.mkdir()
        (fake / "copyq").write_text("#!/bin/sh\nexit 1\n")
        (fake / "copyq").chmod(0o755)
        env.update({"XDG_STATE_HOME": str(base / "state"), "XDG_RUNTIME_DIR": str(base / "runtime"),
                    "DICTATE_DIR": str(base / "cache"), "DICTATE_CONFIG": "", "DICTATE_OSD_BACKEND": "feh",
                    "DICTATE_LEARN_BIN": str(binary), "EDITOR": str(editor), "VISUAL": str(editor),
                    "PATH": str(fake) + ":" + env.get("PATH", ""), "TERM": "xterm-256color"})
        Path(env["XDG_RUNTIME_DIR"]).mkdir(mode=0o700)
        readfd, writefd = os.pipe()
        with (base / "xvfb.log").open("w") as log:
            xvfb = subprocess.Popen(["Xvfb", "-displayfd", str(writefd), "-screen", "0", "1600x1000x24",
                                     "-nolisten", "tcp"], pass_fds=[writefd], stdout=log, stderr=log)
        os.close(writefd)
        if not select.select([readfd], [], [], 10)[0]:
            xvfb.terminate()
            raise RuntimeError("Xvfb did not start")
        env["DISPLAY"] = ":" + os.read(readfd, 64).decode().strip()
        os.close(readfd)
        socket = f"dictate-learn-{os.getpid()}"
        tmux = ["tmux", "-L", socket, "-f", "/dev/null"]
        terminals = []

        def run(cmd, **kwargs):
            result = subprocess.run(cmd, env=env, capture_output=True, text=True, **kwargs)
            if result.returncode:
                raise RuntimeError(f"{Path(cmd[0]).name} failed: {result.stderr.strip() or result.stdout.strip()}")
            return result

        def pane_text(pane):
            return run(tmux + ["capture-pane", "-p", "-t", pane]).stdout

        def paste(pane, text):
            run(tmux + ["load-buffer", "-b", "learn-test", "-"], input=text)
            run(tmux + ["paste-buffer", "-p", "-d", "-b", "learn-test", "-t", pane])
            time.sleep(0.25)

        def set_active(window):
            run(["xdotool", "windowfocus", window])
            # Xvfb has no WM. xprop would create a CARDINAL; EWMH requires WINDOW.
            libraries = run(["ldd", shutil.which("xprop")]).stdout
            library = next(line.split("=>", 1)[1].strip().split()[0]
                           for line in libraries.splitlines() if "libX11.so.6 =>" in line)
            x = ctypes.CDLL(library)
            x.XOpenDisplay.argtypes = [ctypes.c_char_p]
            x.XOpenDisplay.restype = ctypes.c_void_p
            x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
            x.XDefaultRootWindow.restype = ctypes.c_ulong
            x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
            x.XInternAtom.restype = ctypes.c_ulong
            x.XChangeProperty.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong,
                                         ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                         ctypes.POINTER(ctypes.c_ubyte), ctypes.c_int]
            x.XCloseDisplay.argtypes = [ctypes.c_void_p]
            display = x.XOpenDisplay(env["DISPLAY"].encode())
            value = ctypes.c_ulong(int(window))
            x.XChangeProperty(display, x.XDefaultRootWindow(display),
                              x.XInternAtom(display, b"_NET_ACTIVE_WINDOW", 0), 33, 32, 0,
                              ctypes.cast(ctypes.pointer(value), ctypes.POINTER(ctypes.c_ubyte)), 1)
            x.XCloseDisplay(display)

        def history(text, index):
            entry = Path(env["XDG_STATE_HOME"]) / "dictate/history" / f"2026-10-26_12-00-0{index}-123-0"
            entry.mkdir(parents=True)
            for name in ("transcript.txt", "transcript.raw.txt"):
                (entry / name).write_text(text + "\n")
            with wave.open(str(entry / "audio.wav"), "wb") as wav:
                wav.setnchannels(1)
                wav.setsampwidth(2)
                wav.setframerate(16000)
                wav.writeframes(b"\0\0" * 16000)
            return entry

        def seed_delivery(pane, app, terminal, entry, text):
            fields = run(tmux + ["display-message", "-p", "-t", pane,
                                 "#{pane_pid}\t#{pane_tty}"]).stdout.strip().split("\t")
            todo = [int(fields[0])]
            app_pid = None
            while todo:
                pid = todo.pop()
                try:
                    if Path(f"/proc/{pid}/comm").read_text().strip() == app:
                        app_pid = pid
                        break
                    todo.extend(map(int, Path(f"/proc/{pid}/task/{pid}/children").read_text().split()))
                except OSError:
                    pass
            assert app_pid is not None
            started = Path(f"/proc/{app_pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
            delivery = {"id": f"fixture-{app}-{entry.name}", "entry": entry.name, "at": time.time(),
                "text": text, "audio_aligned": True, "raw": text, "corrected": text,
                "target": {"window_pid": terminal.pid, "pane": pane, "pane_pid": int(fields[0]),
                           "app": app, "app_pid": app_pid, "app_started": started, "tty": fields[1]}}
            dest = Path(env["XDG_STATE_HOME"]) / "dictate/learn/deliveries"
            dest.mkdir(parents=True, exist_ok=True)
            (dest / f"{delivery['id']}.json").write_text(json.dumps(delivery))

        def capture():
            run([str(binary), "learn"], timeout=12)
            receipt = json.loads((Path(env["XDG_RUNTIME_DIR"]) / f"dictate-overlay-{os.getuid()}" / "state.json").read_text())
            assert receipt["phase"] == "captured" and receipt["label"] == "Draft captured", receipt
            assert "Captured draft:\n" in receipt["text"] and receipt["duration"] == 10.0, receipt
            captures = Path(env["XDG_STATE_HOME"]) / "dictate/learn/captures"
            newest = max(captures.iterdir(), key=lambda p: (p / "capture.json").stat().st_mtime_ns)
            return json.loads((newest / "capture.json").read_text()), (newest / "draft.txt").read_text()

        reports = []
        try:
            for i, app in enumerate(args.apps, 1):
                executable = Path.home() / ".local/bin" / app
                options = ["--no-daemon", "--no-alt-screen"] if app == "codex" else [
                    "--no-chrome", "--setting-sources", "",
                    "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}']
                command = "exec " + shlex.join([str(executable), *options])
                run(tmux + ["new-session", "-d", "-s", app, "-x", "140", "-y", "45",
                            "-c", str(ROOT), command,
                            ";", "set-option", "-g", "remain-on-exit", "on"])
                pane = run(tmux + ["list-panes", "-t", app, "-F", "#{pane_id}"]).stdout.strip()
                # All helper subprocesses must resolve this private tmux server.
                address = run(tmux + ["display-message", "-p", "-t", pane, "#{socket_path}"]).stdout.strip()
                env["TMUX"] = f"{address},0,0"
                title = f"DictateLearnTest-{app}"
                with (base / f"xterm-{app}.log").open("w") as log:
                    terminal = subprocess.Popen(["xterm", "-title", title, "-geometry", "140x45", "-e",
                                                  *tmux, "attach-session", "-t", app], env=env, stdout=log, stderr=log)
                terminals.append(terminal)
                def window_id():
                    r = subprocess.run(["xdotool", "search", "--name", title], env=env, capture_output=True, text=True)
                    return r.stdout.splitlines()[-1] if r.returncode == 0 and r.stdout.strip() else None
                window = wait(window_id)
                set_active(window)
                wait(lambda: any(mark in pane_text(pane) for mark in ("›", "❯", "? for shortcuts")), timeout=25)
                time.sleep(1)
                original = "Please ask the worker to fix the calling bug before we run the next test."
                corrected = original.replace("calling", "culling")
                entry = history(original, i)
                seed_delivery(pane, app, terminal, entry, original)
                paste(pane, original)
                run(tmux + ["send-keys", "-t", pane, "C-c"])
                time.sleep(0.2)
                paste(pane, corrected)
                record, draft = capture()
                receipt_path = Path(env["XDG_RUNTIME_DIR"]) / f"dictate-overlay-{os.getuid()}" / "state.json"
                assert "calling → culling" in json.loads(receipt_path.read_text())["text"]
                assert draft.strip() == corrected, (app, "draft mismatch", draft[:200])
                assert any(m["kind"] == "lexical" and m["edits"] == [{"from": "calling", "to": "culling"}]
                           for m in record["matches"]), (app, "lexical match missing", record["matches"])
                assert not (entry / "transcript.gold.txt").exists(), "capture silently made a gold label"
                _, second = capture()
                assert second == draft, (app, "capture changed the draft")
                # A pasted multiline blob may be shown as a placeholder in the UI.
                run(tmux + ["send-keys", "-t", pane, "C-c"])
                time.sleep(0.2)
                long_text = "\n\n".join(
                    f"Item {n} keeps this draft intact with every one of its original words."
                    for n in range(12))
                long_entry = history(long_text, i + 2)
                seed_delivery(pane, app, terminal, long_entry, long_text)
                paste(pane, long_text)
                _, long_draft = capture()
                assert long_draft.strip() == long_text, (app, "folded paste was not expanded", long_draft[:200])
                _, long_second = capture()
                assert long_second == long_draft, (app, "long draft changed on capture")
                # A shell/pipe caller cannot consume the composer's pending handoff.
                store = Path(env["XDG_STATE_HOME"]) / "dictate/learn"
                delivery = max((store / "deliveries").iterdir(), key=lambda p: p.stat().st_mtime_ns)
                d = json.loads(delivery.read_text())
                pending = store / "pending" / f"pane-{pane[1:]}.json"
                pending.write_text(json.dumps({"nonce": "0" * 32, "target": d["target"],
                    "created": time.time(), "expires": time.time() + 8, "deliveries": [d["id"]]}))
                foreign = subprocess.run([str(binary), "learn-editor", "--", str(long_entry / "transcript.txt")],
                    env={**env, "TMUX_PANE": pane}, stdin=subprocess.DEVNULL, capture_output=True, text=True)
                assert foreign.returncode == 3 and pending.exists(), (app, "foreign caller consumed a request")
                pending.unlink()
                reports.append({"app": app, "capture": "passed", "draft_preserved": True,
                                "word_pairs": 1, "folded_paste": "passed", "foreign_caller": "rejected"})
                print(json.dumps(reports[-1]), flush=True)
                run(tmux + ["kill-session", "-t", app])
        except Exception:
            print("EDITOR TRACE", trace.read_text() if trace.exists() else "not invoked")
            for app in args.apps:
                try:
                    panes = run(tmux + ["list-panes", "-t", app, "-F", "#{pane_id}"]).stdout.splitlines()
                    for pane in panes:
                        print("TEST UI", app, pane_text(pane)[-3500:])
                except Exception:
                    pass
            raise
        finally:
            subprocess.run(tmux + ["kill-server"], env=env, capture_output=True)
            for terminal in terminals:
                if terminal.poll() is None:
                    terminal.terminate()
            xvfb.terminate()
            xvfb.wait(timeout=5)
        print("No prompts were submitted; private test sessions stopped.")


if __name__ == "__main__":
    main()
