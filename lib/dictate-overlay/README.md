# Dictation overlay

See [configuration](CONFIGURATION.md) for config-file precedence, privacy
guidance, and platform boundaries.

`dictate` uses a small GTK 3 / Cairo popup on X11. Try `dictate demo` for a
visual-only walkthrough: it does not record, transcribe, copy, or paste.

The panel appears in the center of the focused window's monitor (pointer's
monitor as fallback). It has a real rounded X Shape silhouette, a short eased
entrance/exit, a gentle text-color transition, live recording time, and a
30 Hz audio history meter. The meter reads the last 50 ms of the existing
48 kHz multichannel s16 capture on the configured channel; a stalled or silent
capture decays to a flat line. Transcription and wording cleanup have separate
processing states. The panel grows to fit longer transcripts, up to 75% of the current monitor
height. Beyond that cap, text is ellipsized to keep the panel on screen. It
stays expanded through revisions and processing, and resets for a new dictation. The preview stays in
place during transcription, including its truncation direction. Only inserted
or revised words receive the brief color transition; unchanged words stay at
full brightness. Preview requests run sequentially, with a 5 ms pause after
each completed request (`DICTATE_STREAM_PAUSE=0.005`). There is no target
interval by default (`DICTATE_STREAM_INTERVAL=0`). Each response is still
a complete transcript snapshot, so word-by-word arrival is not guaranteed.
Word ranges use UTF-8 byte offsets for Pango, including Unicode
transcripts. Starting a new recording clears the previous preview.

No compositor is needed. The panel is opaque; text blends against the panel,
not the desktop. No screenshot of the desktop is used to simulate transparency.

The project has no telemetry. Audio, transcripts, history, timing metadata, and
temporary files stay on the local machine unless a user explicitly configures a
remote transcription or cleanup endpoint. Keep local configuration, personal
vocabulary, recordings, history, models, and credentials outside the repository.

Focus safety is part of the window itself: GTK `POPUP` / override-redirect,
`accept_focus=false`, `focus_on_map=false`, and an empty input shape installed
through **Gtk.Widget**, which preserves it across maps. Setting just the
Gdk.Window input shape was insufficient: GTK overwrote it when showing the
window. Clicks and pointer movement pass to the underlying application. The
existing i3 rules remain useful for the optional legacy feh backend.

`bin/dictate` atomically writes a private JSON state file in the runtime
directory. A singleton renderer handles animation and expiration, stays warm
for 60 seconds after hiding, and reuses its window. No transcript update waits
for image generation or animation. The first launch builds a small cached Nix
runtime using `<nixpkgs>`; it needs no NixOS or home-manager rebuild. Build or
renderer errors go to the private state log at
`$XDG_STATE_HOME/dictate/dictate.log` (or
`~/.local/state/dictate/dictate.log`) and do not block text delivery.
The current Nix expression imports `<nixpkgs>`, so a Nix channel providing that
path must be configured; a pure flakes-only setup is not supported yet.

Tuning (environment variables inherited by the renderer at startup):

- `DICTATE_OSD_DONE_MS=1000`: completion display time; takes effect per message.
- `DICTATE_OSD_WIDTH=960`: width in pixels, clamped to 560–1600.
- `DICTATE_OSD_SIZE=28`: transcript font size in pixels, clamped to 18–40.
- `DICTATE_OSD_FONT='DejaVu Sans'`: Pango font family.
- `DICTATE_OSD_MOTION=0`: disable sliding, pulsing, and text transitions.
- `DICTATE_OSD_BACKEND=feh`: use the previous image-based renderer for comparison.

Allow the renderer's 60-second idle timeout before changing its environment.
The legacy backend's color, height, and slide settings apply only to feh.

Run `dictate-overlay --check` for isolated Xvfb integration checks: focus on
map/update/remap, actual click delivery to the underlying window, window reuse,
expiration, malformed messages, selected audio channel, and meter decay.
These checks never interact with the real desktop. Bash syntax and ShellCheck
also cover both launch scripts.

`dictate timings` summarizes the latest recording's preview performance (minimum,
median, p95, maximum). The metadata-only log is
`~/.cache/dictate/dictate-preview-timing.tsv` (or `$DICTATE_DIR`), replaced at
preview startup so it stays bounded. It includes preparation, ASR HTTP request
plus parsing, text styling and overlay submission, total work, actual request
start intervals, whether the text changed, and failures. It does not contain
transcript text. Total work excludes intentional waiting, logging overhead,
and the overlay's asynchronous rendering; it is not spoken-word-to-screen latency.
The summary excludes failed requests from duration statistics but counts failures.

The default 5 ms pause is experimental. Set `DICTATE_STREAM_PAUSE=0` to try
no intentional pause, or `DICTATE_STREAM_INTERVAL=0.2` to restore a 200 ms
start-to-start target. These settings belong to the process starting recording.
Requests never overlap, even with zero pause. Actual cadence includes all
processing and bookkeeping, so zero pause does not mean zero latency. For
further gains, use real-session timings to identify the bottleneck.

Run `python3 lib/dictate-overlay/check_preview.py` to check cadence, stage
accounting, error recovery, and timing summaries without recording or inference.

Preview silence gating uses the existing raw capture, not a speech model. A
small persistent helper checks newly recorded audio in 20 ms blocks on the
selected channel. Above-threshold audio triggers a request; a 300 ms pause
allows one settling request, then the text stays unchanged until audio resumes.
The helper checks for resumed audio every 40 ms while quiet. It never removes
audio: every preview and the final transcription still receive the full capture.

`DICTATE_STREAM_GATE_DB=-48` sets the RMS threshold in dBFS. A more negative
value catches quieter speech but also more background noise; a less negative
value ignores more noise. `DICTATE_STREAM_GATE=0` restores continuous requests.
This is an energy threshold, so sufficiently loud non-speech sounds can also
trigger previews. The gate affects only live previews, not final transcription.

`dictate timings release` shows the latest release-to-paste breakdown, stored in
`dictate-release-timing.tsv` under the cache directory. Timing starts at Bash
script entry (not the physical key release), includes config loading, recorder
shutdown, audio preparation/history, final recognition, and clipboard work, and
ends with shortcut submission. The target application's insertion time is not
observable through this path. The log contains only event names and timestamps,
is replaced for each stop, and can also show incomplete or non-paste outcomes.

Paste delivery now sends one complete shortcut using `xdotool key
--clearmodifiers`, with 12 ms event spacing by default. The old implementation
used separate modifier-down, key, and modifier-up processes, plus two 100 ms
waits; a held Shift could leak into a Ctrl+V shortcut. Setting
`DICTATE_PASTE_KEY_DELAY=0` removes event spacing for experiments. This setting
now controls spacing inside the chord, rather than sleeps around the V key.

The clipboard backend is selected once per delivery. Clipboard contents are
still verified before sending the shortcut. Restoration runs in the background
and checks that the clipboard still holds the dictation before restoring prior
text. Its 400 ms default preserves approximately the old effective retention
window after removing the post-key wait; it does not delay the shortcut or
panel dismissal. `DICTATE_PASTE_SETTLE` overrides it, or
`DICTATE_CLIPBOARD=keep` disables restoration.

Restoration remains best-effort: it has no application acknowledgement, the
check-and-restore is not atomic, and only nonempty plain text is restored.
Application shortcuts remain a class-based assumption. In particular, stock
xterm does not bind Ctrl+Shift+V to clipboard paste; the terminal test explicitly
configures that binding. A future delivery backend should make those capabilities
explicit rather than assume every terminal shares the same shortcut.

Run `dictate-overlay --check-delivery` for isolated X11 delivery checks. It uses
Xvfb, GTK, xclip, and xterm, without accessing the live desktop's clipboard.
It exercises Ctrl+V and Ctrl+Shift+V, held Ctrl/Shift, a busy receiver, 0/12 ms
spacing, clipboard readiness, restoration, and preserving newly copied text.

Timing values are hardware-, model-, and application-dependent. Use
`dictate timings` and `dictate timings release` from your own sessions when
evaluating changes.
