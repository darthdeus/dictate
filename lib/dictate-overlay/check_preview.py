"""Exercise preview cadence and timing reports without microphone, model or UI."""
import csv
import os
from pathlib import Path
import subprocess
import tempfile

source = Path(__file__).resolve().parents[2] / "bin/dictate"
script = source.read_text()
functions = script[script.index("preview_timing() {"):script.index("stop_preview() {")]
for interval, pause in (("0.1", "0.005"), ("0", "0.005"), ("0", "0")):
    with tempfile.TemporaryDirectory() as tmp:
        env = {**os.environ, "DICTATE_DIR": tmp, "DICTATE_HISTORY": "0", "DICTATE_STREAM_GATE": "0", "LC_ALL": "C.UTF-8",
               "TEST_INTERVAL": interval, "TEST_PAUSE": pause}
        harness = r'''
    set -euo pipefail
    DATADIR="$DICTATE_DIR"; RAW="$DATADIR/raw"; PIDFILE="$DATADIR/pid"
    PREVIEW_TIMING="$DATADIR/dictate-preview-timing.tsv"
    STREAM_INTERVAL="$TEST_INTERVAL"; STREAM_PAUSE="$TEST_PAUSE"; MAX_SECS=300; SERVER=unused; LANG_=auto; PROMPT="example-project, sub-agent"
    printf audio >"$RAW"; touch "$PIDFILE"
    n=0
    raw_to_mono16() {
      n=$((n + 1))
      sleep 0.015
      [ "$n" != 2 ]
    }
    curl() {
      local found_prompt=0 found_language=0 previous="" arg
      for arg in "$@"; do
        if [ "$previous" = --form-string ] && [ "$arg" = "prompt=$PROMPT" ]; then found_prompt=1; fi
        if [ "$previous" = --form-string ] && [ "$arg" = "language=$LANG_" ]; then found_language=1; fi
        previous="$arg"
      done
      [ "$found_prompt" = 1 ] && [ "$found_language" = 1 ] || return 99
      sleep 0.025
      [ "$n" != 3 ] || return 22
      printf '{"text":"preview %s"}' "$n"
    }
    restyle() { sleep 0.005; printf '%s' "$1"; }
    notify() { if [ "$n" -ge 5 ]; then rm "$PIDFILE"; fi; }
    '''
        subprocess.run(["bash", "-c", harness + functions + "\npreview_loop\n"], env=env, check=True, timeout=10)
        with Path(tmp, "dictate-preview-timing.tsv").open() as stream:
            rows = list(csv.DictReader(stream, delimiter="\t"))
        assert [r["status"] for r in rows] == ["ok", "prepare_error", "request_error", "ok", "ok"]
        for row in rows:
            assert abs(sum(float(row[k]) for k in ("prep_ms", "request_ms", "finish_ms")) - float(row["work_ms"])) < .05
        assert all(0 < float(row["prep_ms"]) < 1000 for row in rows), rows
        assert all(0 < float(row["request_ms"]) < 1000 for row in rows if row["status"] == "ok"), rows
        if interval == "0.1":
            assert all(float(row["start_gap_ms"]) >= 95 for row in rows[1:]), rows
        else:
            assert all(float(row["start_gap_ms"]) >= float(previous["work_ms"])
                       for previous, row in zip(rows, rows[1:])), rows
        # Incomplete trailing rows must not break a summary during recording.
        with Path(tmp, "dictate-preview-timing.tsv").open("a") as stream:
            stream.write("1234\t12")
        output = subprocess.check_output(["bash", str(source), "timings"], env=env, text=True)
        assert "5 requests, 2 failures" in output, output
        assert "ASR request" in output
        assert "preview 1" not in output
        print(f"PASS: interval={interval}, pause={pause}; locale-independent timings; errors; clean stop; summary")

# Exercise energy gating independently of any speech model.
import importlib.util
import struct
import sys
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("preview_gate", Path(__file__).with_name("preview_gate.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with tempfile.TemporaryDirectory() as tmp:
    raw = Path(tmp, "audio.pcm")
    raw.write_bytes(bytes(960 * 4))
    gate = module.Gate(raw, 2, 0)
    assert not gate.ready(0), "Silence should not request transcription"
    with raw.open("ab") as stream:
        stream.write(struct.pack("<2h", 1000, 0) * 960)
    assert gate.ready(0.1), "Speech energy should trigger promptly"
    with raw.open("ab") as stream:
        stream.write(bytes(960 * 4))
    assert not gate.ready(0.2)
    assert gate.ready(0.41), "Allow one settling request"
    assert not gate.ready(0.8), "Paused text should stay frozen"
    assert not gate.ready(1.2)
    other = module.Gate(raw, 2, 1)
    assert not other.ready(1.2), "Only selected channel should trigger"
    with raw.open("ab") as stream:
        stream.write(struct.pack("<2h", 1000, 0) * 960)
    assert gate.ready(1.3), "Resumed speech should trigger"
    raw.write_bytes(bytes(960 * 4))
    assert not gate.ready(1.4), "Truncation should reset gate state"
print("PASS: silence gate, settling update, resumed speech, channel selection, truncation")

# Exercise the real Bash coprocess wiring, including quiet waiting and teardown.
import re
import time
with tempfile.TemporaryDirectory() as tmp:
    env = {**os.environ, "DICTATE_DIR": tmp, "DICTATE_HISTORY": "0", "DICTATE_STREAM_GATE": "1",
           "TEST_GATE": str(Path(__file__).with_name("preview_gate.py").resolve())}
    raw = Path(tmp, "raw")
    raw.write_bytes(struct.pack("<h", 1000) * 960)
    wired = re.sub(r'    helper=.*', '    helper="$TEST_GATE"', functions)
    harness = r'''
set -euo pipefail
DATADIR="$DICTATE_DIR"; RAW="$DATADIR/raw"; PIDFILE="$DATADIR/pid"; LOG="$DATADIR/log"
PREVIEW_TIMING="$DATADIR/timing.tsv"
STREAM_INTERVAL=0; STREAM_PAUSE=0.005; MAX_SECS=300; SERVER=unused; LANG_=auto; PROMPT="example-project, sub-agent"; CHANNELS=1; CHAN=0
touch "$PIDFILE"
raw_to_mono16() { :; }
curl() { echo request >>"$DATADIR/requests"; printf '{"text":"test"}'; }
restyle() { printf '%s' "$1"; }
notify() { :; }
'''
    process = subprocess.Popen(["bash", "-c", harness + wired + "\npreview_loop\n"], env=env)
    try:
        def count():
            path = Path(tmp, "requests")
            return len(path.read_text().splitlines()) if path.exists() else 0
        deadline = time.monotonic() + 3
        while count() < 2 and time.monotonic() < deadline:
            time.sleep(.05)
        assert count() == 2, "Initial audio plus one settling request expected"
        time.sleep(.4)
        assert count() == 2, "No repeated requests while quiet"
        with raw.open("ab") as stream:
            stream.write(struct.pack("<h", 1000) * 960)
        deadline = time.monotonic() + 2
        while count() < 3 and time.monotonic() < deadline:
            time.sleep(.02)
        assert count() == 3, "Speech resumes requests"
        Path(tmp, "pid").unlink()
        process.wait(timeout=2)
        assert process.returncode == 0
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=2)
print("PASS: live gate coprocess pauses requests, resumes on audio, exits with recording")
