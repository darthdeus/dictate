# Configuration

Settings are read from `~/.config/dictate/config.toml`, respecting
`XDG_CONFIG_HOME`. Set `DICTATE_CONFIG=/path/to/config.toml` to select another
file, or `DICTATE_CONFIG=""` to disable file loading. A missing default file is
fine; a missing explicitly selected file is an error.

Precedence is built-in defaults < config file < environment. An explicitly
empty environment variable overrides the file, including
`DICTATE_SERVER=""` to force the command-line recognizer. Individual settings
retain their existing empty-value behavior.

The file is TOML data, not shell code. Values may be single-line strings,
numbers, or booleans (`true`/`false` become `1`/`0`). Paths are literal: use an
absolute path rather than `$HOME` or `~`. Unknown setting names and unsupported
value types fail before capture starts. Configuration errors do not print
configuration values.

Start from [`config.example.toml`](../../config.example.toml):

```toml
DICTATE_MIC = "auto"
DICTATE_CHANNELS = 1
DICTATE_CHAN = 0
DICTATE_LANG = "auto"
DICTATE_PROMPT = ""
DICTATE_STYLE = false
DICTATE_LLM = false
```

Treat the real configuration as private local data. Do not commit microphone
node names, personal vocabulary, local paths, recordings, history, model files,
or credentials. The example intentionally contains only generic values.

Legacy cleanup is disabled by default. It requires explicit
`DICTATE_LLM=1`, `DICTATE_LLM_URL`, `DICTATE_LLM_MODEL`, and
`DICTATE_LLM_KEY`; it never searches shell files or credential stores. Any
failure falls back to the raw transcript. Do not put credentials in a tracked
configuration file.

## Platform scope

The current implementation targets Linux/X11 and requires its existing desktop
tooling: PipeWire or PulseAudio, GTK, and the X11 clipboard/input utilities.
Nix, i3, key bindings, and user services are integrations around the command,
not part of the configuration format. macOS support is not implemented.

Keep capture, transcription, configuration, history, overlay state, and text
delivery conceptually separate when adding another platform. A platform
adapter must preserve focus safety, microphone permissions, and delivery
permissions.

## Recognition settings

`DICTATE_LANG` is sent with every Whisper HTTP request and passed to the CLI
fallback. The default `auto` preserves automatic language detection. Set a
specific language only after testing it against representative recordings.

`DICTATE_PROMPT` is sent with every live-preview and final Whisper HTTP request,
as well as the CLI path. Changing it takes effect on the next command; it does
not require restarting a resident server. An empty value explicitly clears
that request's prompt. HTTP uses `--form-string` so values beginning with `@`
or containing multipart syntax remain literal text.

Both settings are recognition hints, not guaranteed corrections. Personal terms
belong in the user's private profile, not in application defaults or tests.

`cargo test` covers configuration parsing and request forwarding.

### Live preview phrase boost

With `dictate daemon` running, the live preview comes from the Nemotron
streaming model (`DICTATE_STREAM_MODEL`, lookahead `DICTATE_STREAM_RIGHT`).
Its decoder is biased towards glossary terms (transcribe.cpp phrase
boosting: a logit bonus along each term's token path), so names such as
`NixOS` or `AGENTS.md` show up spelled right while you speak. It affects the
preview only; the final text still comes from the final pass.

- `DICTATE_PREVIEW_BOOST`: boost weight, default `2`. `0` or `false` turns
  boosting off. Higher weights fix more terms but rewrite more unrelated
  words.
- `DICTATE_PREVIEW_TERMS`: comma-separated terms to boost, used as is. When
  unset, the preview boosts `DICTATE_TERMS` minus terms of three characters
  or fewer (`AI`, `GPU`) and common English or everyday developer words
  (`master`, `prompt`, `diff`): boosting those mostly turns unrelated speech
  into the term. Multi-word phrases are always kept.
- `DICTATE_PREVIEW_STOP`: extra comma-separated terms (case-insensitive) to
  leave out of that default list, for personal terms that misfire.
