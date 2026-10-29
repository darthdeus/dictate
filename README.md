# dictate

`dictate` is a local hold-to-talk speech-to-text tool. It captures audio,
transcribes it with Whisper, shows an optional click-through X11 overlay, and
delivers the result to the focused application.

The current implementation targets Linux/X11. macOS support and platform
adapters are future work. Desktop key bindings, NixOS configuration, services,
and personal profiles are intentionally not part of this repository.

## Privacy and public-source hygiene

There is no telemetry. Audio, transcripts, history, timing data, and temporary
files stay local unless a user explicitly configures a remote transcription or
cleanup endpoint.

Keep these out of Git:

- customized configuration files, microphone or device identifiers, and local paths;
- personal vocabulary or transcript examples;
- recordings, history, model files, logs, and evaluation data;
- API keys, tokens, cookies, or provider configuration.

Use [`config.example.toml`](config.example.toml) as the public template. The
repository's `.gitignore` blocks common local artifacts, but review `git diff`
and `git status` before every public push.

## Dependencies

The command-line workflow currently expects:

- Bash, Python 3.11+, `ffmpeg`/`ffprobe`, `curl`, `jq`, `timeout`, `awk`, and
  `sha256sum`;
- `flock`, `pgrep`, and `pkill` (usually provided by util-linux and procps);
- PipeWire (`pw-record`) or PulseAudio capture, plus `pactl`;
- Whisper.cpp's `whisper-cli` and a compatible model; `whisper-server` is
  optional but enables live previews and avoids repeated model loading;
- `parakeet-cli` and its model only when `DICTATE_ENGINE=parakeet`;
- X11 utilities including `xdotool`, `xclip` or CopyQ, `xdpyinfo`, `xmodmap`,
  and `xset`; `dunstify` or `notify-send` is optional for visible overlay
  failure notices.

The default GTK/Cairo overlay launcher also needs `nix-build` and a Nix
installation with `<nixpkgs>` available through a configured channel. A pure
flakes-only setup without that channel is not supported by the current
`default.nix`. Its isolated checks additionally need Xvfb, GTK, xdotool, xclip,
and xterm.

The legacy `DICTATE_OSD_BACKEND=feh` comparison path additionally needs `feh`
and ImageMagick (`magick`).

Models and desktop integration are deliberately not bundled. Set
`DICTATE_MODEL` and, when needed, `DICTATE_MIC` or `DICTATE_CHANNELS` in the
local environment or configuration file.

## Install

```sh
git clone <repository-url> dictate
cd dictate
cargo build --release          # binary: target/release/dictate
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/dictate"
cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/dictate/config.toml"
```

The streaming engine currently pins transcribe.cpp's unreleased
`phrase-boost` commit from a local checkout (a `file://` git source in
`Cargo.toml`); until that commit is pushed somewhere reachable, the build only
works on a machine with that checkout. Point `git` at the published repository
before packaging or publishing.

`shell.nix` provides the toolchain (Rust, CMake, Vulkan headers and shader
compiler for the transcribe.cpp streaming engine): `nix-shell --run 'cargo
build --release'`.

Install Whisper.cpp and a model separately. The upstream project documents the
same build and model-download flow in its [README](https://github.com/ggml-org/whisper.cpp/blob/master/README.md):

```sh
git clone https://github.com/ggml-org/whisper.cpp.git
cmake -S whisper.cpp -B whisper.cpp/build
cmake --build whisper.cpp/build -j --config Release
sh whisper.cpp/models/download-ggml-model.sh large-v3-turbo
export PATH="$PWD/whisper.cpp/build/bin:$PATH"
export DICTATE_MODEL="$PWD/whisper.cpp/models/ggml-large-v3-turbo.bin"
```

Keep the model outside this repository. If you use a different model location,
set `DICTATE_MODEL` in the environment or in the local configuration.

Edit the copied configuration locally, then run:

```sh
target/release/dictate doctor
```

Set `DICTATE_CONFIG=/path/to/config.toml` to use another profile. Set
`DICTATE_CONFIG=""` to disable file loading and use environment variables only.

## Usage

```sh
dictate start       # begin capture
dictate stop        # transcribe and deliver
dictate toggle      # start or stop
dictate cancel      # discard the recording in progress
dictate daemon      # resident streaming engine (live preview, pause pass)
dictate serve       # resident whisper server
dictate hotkey      # X11 push-to-talk on F19 without a key grab
dictate demo        # show the overlay without recording
dictate history     # list recoverable dictations
dictate retry latest
dictate learn       # save corrections from the focused CLI draft, before sending
dictate corrections # list saved captures
dictate replay 50   # re-run saved recordings through the text path, read-only
```

Bind `start` and `stop` to a press/release key in the desktop environment of
your choice; any input tool that invokes `start` on press and `stop` on release
can be used. The overlay is designed not to take focus or intercept clicks.

On X11, `dictate hotkey` is a built-in binding for F19 (keycode 197; e.g. Caps
Lock mapped to F19 by keyd). Hotkey daemons like sxhkd grab the key, and while
a grabbed key is held X sends the focused window a grab FocusOut, which
terminals such as Alacritty draw as unfocused for the whole dictation.
`dictate hotkey` reads XInput2 raw key events instead (no grab), clears the
key's keysyms so apps ignore it, and runs `start`/`stop` on press/release.
Run it as part of the X session, e.g. a systemd user service. Shift held on
the key runs `stop --sentence` (see below). `DICTATE_HOTKEYS` binds more keys,
e.g. a macro pad mapped onto F13-F24 with keyd: `talk`, `talk-sentence`,
`cancel` (discard the recording in progress), `learn` (capture draft corrections), `retry` (re-paste the last
dictation), `window-prev`/`window-next` and `pane-prev`/`pane-next` (move in
the focused terminal's tmux) and `key:<chord>` (send a chord, e.g. `key:alt+BackSpace`). The
listener re-reads the config file when it changes, so no restart is needed.
A `layer` key switches to `DICTATE_HOTKEYS_LAYER2` while held and shows those
bindings as a grid beside the dictation panel (F13-F18 as a 2x3 macro pad).

`edit` (with `dictate daemon`): hold it during a dictation and say an
instruction about what you said so far ("no, make that Tuesday", "drop the
last sentence"). On release the correction model applies it in the background
and the overlay shows the edited text; keep talking, and the release of the
talk key pastes the edited start plus the rest.

After the text, dictate leaves the input ready for the next dictation. In
`DICTATE_PARAGRAPH_APPS` (default `claude|codex`, matched against the command
in the focused tmux pane) it sends Ctrl+J twice, a new paragraph; elsewhere a
space (never Ctrl+J: in a shell that is Enter). `stop --sentence` ends the
sentence instead: `. `, or a space after the text's own `.`, `?` or `!`.
`DICTATE_JOIN=off` pastes the text alone.

### Capture corrections you already made

For Codex CLI and Claude Code in tmux: dictate, edit the pasted text in the
composer, then invoke `dictate learn` from a hotkey **before sending**. The macro
pad's default layer-two binding is `f14=learn`; `f18` remains re-paste. This is a
local draft capture, with no transcription or correction API request.

A **Draft captured** card stays visible for ten seconds (click to dismiss). It
shows word replacements such as `calling → culling`, formatting/review counts,
and a short preview of the saved draft. An unchanged draft explicitly says
**No changes found**; a failed capture shows an error instead.

The capture reads the draft through the CLIs' Ctrl+G external editor, so set
`EDITOR`/`VISUAL` to `bin/dictate-editor` and your real editor in
`DICTATE_EDITOR` (default `vi`). While a capture is pending the wrapper hands the
draft file to `dictate learn-editor -- "$@"` instead of opening an editor: exit 0
means captured, exit 3 means ordinary editing (it opens the real editor), other
statuses mean capture failed. Your own wrapper can follow the same contract. A pending
request is limited to its tmux pane, CLI process instance and composer TTY;
normal editing and shell-tool subprocesses cannot consume another draft's capture.
An installation without the wrapper reports a capture timeout.

Each successful paste/re-paste records its exact delivered text, history id,
raw/corrected stages and destination. The capture considers the latest 20 deliveries
in the same CLI instance within 30 minutes. It reads the app's actual editor file,
including expanded pasted text, and leaves its contents unchanged. It does not
scrape terminal scrollback, change the clipboard, or send the prompt.

Private state is under `${XDG_STATE_HOME:-$HOME/.local/state}/dictate/learn/`:
`deliveries/` stores provenance; `captures/<id>/` stores `editor.raw.txt`, `draft.txt`
and `capture.json` with matched spans and word changes. Multiple dictations are
matched separately using unchanged text anchors. Formatting-only changes are
separate from localized word replacements; rewrites, short/weak matches, merged
spans, repeated text and unrelated drafts remain review candidates.

Capturing never silently creates a training label. Inspect with
`dictate correction CAPTURE_ID`; after checking that a matched span represents
what was spoken, `dictate correction CAPTURE_ID --approve HISTORY_ID` writes
`transcript.gold.txt` and its provenance for that recording. Approval requires
one unambiguous match, preserves any existing different gold label, and rejects
spoken-edit recordings whose saved audio covers only the post-edit tail. No
recognizer is retrained or switched automatically. The existing fine-tuning
collectors already prefer gold labels; keep their evaluation split fixed.

The isolated checks are `python3 bench/check-learn-editor.py` for the editor
wrapper and `python3 bench/check-learn-cli.py` for real installed Codex/Claude
drafts in a private Xvfb/tmux session. The latter submits no messages and uses
separate dictation/clipboard/runtime state.

With `dictate daemon` running, the live preview comes from the Nemotron
streaming model (transcribe.cpp, `DICTATE_STREAM_MODEL`), and the final pass
(whisper or `DICTATE_FINAL=elevenlabs`, then the optional `DICTATE_CORRECT`
LLM correction) already runs during pauses, so a dictation that ends in a
pause is ready on release. Each pause pass recognizes only the audio since
the previous pause (cut inside the silence, with the text so far as whisper
context) and the correction runs on the whole text, so every second of audio
is recognized once and release only has to recognize what followed the last
pause. With whisper a cut is kept only once its segment holds
`DICTATE_SEGMENT_MIN_MS` (6 s) of audio, since whisper is less accurate on
short pieces; shorter ones get a speculative pass that is used if the pause
was the end. `DICTATE_INCREMENTAL=0` makes each pause pass resend the whole
recording instead. Without the daemon, `start`/`stop` record per
process with a whisper snapshot preview.

The correction asks Qwen on Groq; when that request fails (the free tier's
daily limit answers 429) it asks the same model on OpenRouter
(`OPENROUTER_API_KEY` in keys.env; providers `DICTATE_CORRECT_FALLBACK_PROVIDERS`,
default ModelRun, DeepInfra, Cerebras; `DICTATE_CORRECT_FALLBACK=0` turns it
off). Same fixes, ~350 ms instead of ~200 ms, about $0.0001 a correction.
After a 429, Groq is skipped for its `retry-after` (state file
`correct-skip.txt`), so the fallback is asked directly until then.

The correction also formats code for agent chats: file names, paths,
commands, flags, environment variables and identifiers get backticks
("claude dot md" -> `CLAUDE.md`, "dash dash release" -> `--release`);
names like Rust or Locika stay plain. `DICTATE_CORRECT_FORMAT=0` turns it off.

The GTK overlay (`bin/dictate-overlay`, `lib/dictate-overlay/overlay.py`) is
built on first use. If Nix or the `<nixpkgs>` channel is unavailable, it
reports the failure once through the desktop notification system when
available and records it in the private state log.
`DICTATE_OSD_BACKEND=feh` disables the panel (text is still delivered).

## Checks

```sh
cargo test                                  # unit tests
cargo clippy --all-targets -- -D warnings   # must stay warning-free
bin/dictate-overlay --check                 # isolated overlay/focus/input tests
cargo run --release -- replay 50            # end-to-end text path over saved history
```

These checks do not replace a real dictation [smoke test](docs/smoke-test.md)
in the target applications. See [`lib/dictate-overlay/README.md`](lib/dictate-overlay/README.md)
for overlay details.

## License

MIT. See [`LICENSE`](LICENSE).
