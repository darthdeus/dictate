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
dictate daemon      # resident streaming engine (live preview, pause pass)
dictate serve       # resident whisper server
dictate demo        # show the overlay without recording
dictate history     # list recoverable dictations
dictate retry latest
dictate replay 50   # re-run saved recordings through the text path, read-only
```

Bind `start` and `stop` to a press/release key in the desktop environment of
your choice. The keyd → sxhkd → F19 chain is one example integration, not a
requirement; any input tool that invokes `start` on press and `stop` on release
can be used. The overlay is designed not to take focus or intercept clicks.

With `dictate daemon` running, the live preview comes from the Nemotron
streaming model (transcribe.cpp, `DICTATE_STREAM_MODEL`), and the final pass
(whisper or `DICTATE_FINAL=elevenlabs`, then the optional `DICTATE_CORRECT`
LLM correction) already runs during pauses, so a dictation that ends in a
pause is ready on release. Without the daemon, `start`/`stop` record per
process with a whisper snapshot preview.

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
