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
chmod +x bin/dictate bin/dictate-overlay
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/dictate"
cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/dictate/config.toml"
```

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
bin/dictate doctor
```

Set `DICTATE_CONFIG=/path/to/config.toml` to use another profile. Set
`DICTATE_CONFIG=""` to disable file loading and use environment variables only.

## Usage

```sh
bin/dictate start       # begin capture
bin/dictate stop        # transcribe and deliver
bin/dictate toggle      # start or stop
bin/dictate demo        # show the overlay without recording
bin/dictate history     # list recoverable dictations
bin/dictate retry latest
```

Bind `start` and `stop` to a press/release key in the desktop environment of
your choice. The keyd → sxhkd → F19 chain is one example integration, not a
requirement; any input tool that invokes `start` on press and `stop` on release
can be used. The overlay is designed not to take focus or intercept clicks.

The default GTK overlay is built on first use. If Nix or the `<nixpkgs>`
channel is unavailable, `dictate-overlay` reports the failure once through the
desktop notification system when available and records it in the private state
log. `DICTATE_OSD_BACKEND=feh` is a legacy alternative; it requires `feh`,
ImageMagick, and a window-manager rule that keeps the `dictate-osd` window
unfocused, floating, and borderless.

## Checks

```sh
python3 lib/dictate-overlay/check_config.py
python3 lib/dictate-overlay/check_transcription.py
python3 lib/dictate-overlay/check_preview.py
bin/dictate-overlay --check
bin/dictate-overlay --check-delivery
bash -n bin/dictate bin/dictate-overlay
```

These checks do not replace a real dictation smoke test in the target
applications. See [`lib/dictate-overlay/README.md`](lib/dictate-overlay/README.md)
for overlay, preview, timing, and delivery details.

## License

MIT. See [`LICENSE`](LICENSE).
