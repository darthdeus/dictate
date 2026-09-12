# Smoke test

The unit tests and `dictate replay` cover configuration and the audio → text
path. Nothing automated covers capture, live preview, the overlay, or pasting
into real applications. Run this by hand before switching an installation to
a new build, and again after changes to those areas.

## Before you start

```sh
cargo build --release
target/release/dictate doctor        # every line ok
target/release/dictate replay 50     # "50/50 identical" on unchanged config
```

A side-by-side trial is easiest with a second hold-to-talk binding that runs
the new build while the usual key keeps running the installed one. Both
implementations share the pidfile, scratch audio, and history, so a recording
started by one can be stopped by the other.

## Checklist

Tick each item with the new build doing both start and stop.

- [ ] **GUI app.** Dictate a sentence into a browser text field or editor. The
      text appears once, styled as configured.
- [ ] **Terminal.** Same in a terminal; it must use the terminal paste shortcut.
- [ ] **Clipboard restore.** Copy something, dictate, wait a second, paste. The
      original clipboard content comes back.
- [ ] **Live preview.** Hold for 10 s or more while speaking. The overlay
      updates as you talk and highlights changed words.
- [ ] **Silence gate.** Pause for a few seconds while holding. Preview requests
      pause (the transcription server log goes quiet) and resume when you speak.
- [ ] **Focus safety.** While the overlay is visible, keep typing and clicking
      in the target app. Focus never moves to the overlay, and clicks pass through it.
- [ ] **Too short.** Tap the key. A "too short" notice appears and nothing is pasted.
- [ ] **Silence only.** Hold for 2 s without speaking. A "silence" notice
      appears (or no speech) and nothing is pasted.
- [ ] **Long hold.** Hold for 30 s. Exactly one new history entry
      (`dictate history 3`), so key autorepeat did not start a second recording.
- [ ] **History and timings.** `dictate history 3` shows `delivery-attempted`;
      `dictate timings release` shows a plausible release-to-paste breakdown.
- [ ] **Retry.** `dictate retry latest` pastes the last transcript again.
- [ ] **Server down.** Stop the transcription server, dictate, then restart it.
      Dictation still works through `whisper-cli`, only slower, with no preview.
- [ ] **Stuck recorder.** Run `dictate start` in a terminal and don't stop it.
      Any recording indicator shows the elapsed time. `dictate stop` finishes
      the recording normally.

## After switching

Point every integration (key bindings, service units, launchers) at the new
build and restart the transcription server so it runs the new `serve`. Keep
the previous build available until a few days of normal use show no problems.
