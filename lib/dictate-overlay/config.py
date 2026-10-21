"""Read declarative settings; output tab-separated assignments, never shell code."""
import math
from pathlib import Path
import sys
import tomllib

KNOWN = set("""DICTATE_CHAN DICTATE_CHANNELS DICTATE_CORRECT DICTATE_CORRECT_KEY DICTATE_CORRECT_MODEL DICTATE_CORRECT_TIMEOUT DICTATE_CORRECT_URL DICTATE_EARLY DICTATE_ELEVENLABS_MODEL DICTATE_ELEVENLABS_TIMEOUT DICTATE_FINAL DICTATE_EARLY_LEVEL DICTATE_EARLY_PAUSE_MS DICTATE_STREAM_MODEL DICTATE_STREAM_RIGHT DICTATE_TERMS DICTATE_CLIPBOARD DICTATE_DIR DICTATE_ENGINE DICTATE_FILTER DICTATE_HISTORY DICTATE_HISTORY_DAYS DICTATE_HISTORY_DIR DICTATE_HISTORY_MAX DICTATE_INPUT DICTATE_LANG DICTATE_LLM DICTATE_LLM_KEY DICTATE_LLM_MODEL DICTATE_LLM_TIMEOUT DICTATE_LLM_URL DICTATE_MAX_SECS DICTATE_MIC DICTATE_MIN_PEAK DICTATE_MODEL DICTATE_NOISE DICTATE_NOTYPE DICTATE_OSD_BACKEND DICTATE_OSD_BG DICTATE_OSD_DONE_MS DICTATE_OSD_FG DICTATE_OSD_FONT DICTATE_OSD_HEIGHT DICTATE_OSD_MOTION DICTATE_OSD_SIZE DICTATE_OSD_SLIDE_DROP DICTATE_OSD_SLIDE_STEPS DICTATE_OSD_SLIDE_STEP_DELAY DICTATE_OSD_WIDTH DICTATE_PARAKEET_MODEL DICTATE_PASTE_KEY_DELAY DICTATE_PASTE_SETTLE DICTATE_PROMPT DICTATE_PW DICTATE_SERVER DICTATE_STREAM DICTATE_STREAM_GATE DICTATE_STREAM_GATE_DB DICTATE_STREAM_INTERVAL DICTATE_STREAM_PAUSE DICTATE_STYLE DICTATE_TERMINALS DICTATE_THREADS DICTATE_TYPE_DELAY DICTATE_VAD DICTATE_VAD_MODEL DICTATE_VAD_THRESHOLD""".split())


def read_config(path):
    with Path(path).open("rb") as stream:
        data = tomllib.load(stream)
    settings = []
    for key, value in data.items():
        if key not in KNOWN:
            raise ValueError(f"Unknown setting: {key}")
        if isinstance(value, bool):
            value = "1" if value else "0"
        elif isinstance(value, (int, float)) and not isinstance(value, bool):
            if not math.isfinite(value):
                raise ValueError(f"Non-finite number for {key}")
            value = str(value)
        elif not isinstance(value, str):
            raise ValueError(f"Expected a string, number, or boolean for {key}")
        if any(char in value for char in "\n\r\t\0"):
            raise ValueError(f"Expected a single-line value for {key}")
        settings.append((key, value))
    return settings


if __name__ == "__main__":
    try:
        settings = read_config(sys.argv[1])
    except tomllib.TOMLDecodeError:
        sys.exit("dictate: invalid TOML configuration")
    except OSError:
        sys.exit("dictate: cannot read configuration file")
    except ValueError as error:
        sys.exit(f"dictate: {error}")
    for key, value in settings:
        print(f"{key}\t{value}")
