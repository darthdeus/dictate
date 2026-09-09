"""Summarize observed command-to-shortcut stages, not application acknowledgement."""
import csv
import math
import sys

try:
    with open(sys.argv[1]) as stream:
        rows = list(csv.DictReader(stream, delimiter="\t"))
except FileNotFoundError:
    print("No release timings yet. Finish a dictation first.")
    raise SystemExit(0)
events = []
for row in rows:
    try:
        stamp = float(row["timestamp"])
        if math.isfinite(stamp) and row["event"]:
            events.append((row["event"], stamp))
    except (ValueError, TypeError, KeyError):
        pass
if not events:
    print("No complete timing events yet.")
    raise SystemExit(0)
start = previous = events[0][1]
print(f"{'Stage completed':28} {'stage ms':>10} {'elapsed ms':>11}")
for name, stamp in events:
    print(f"{name:28} {(stamp-previous)*1000:10.1f} {(stamp-start)*1000:11.1f}")
    previous = stamp
sent = next((stamp for name, stamp in events if name == "paste_shortcut_sent"), None)
if sent is not None:
    print(f"Command entry to paste shortcut sent: {(sent-start)*1000:.1f} ms")
else:
    print("No paste shortcut recorded: still running, skipped, failed, or used typing/clipboard-only mode.")
print("Starts at script entry, after key-binding dispatch. Does not measure when the target app inserts text.")
print(f"Log: {sys.argv[1]}")
