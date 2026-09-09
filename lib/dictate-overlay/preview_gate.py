"""Cheap preview scheduling from PCM energy; never trims recognition audio."""
import array
import math
from pathlib import Path
import sys
import time


class Gate:
    def __init__(self, path, channels, channel, threshold=-48):
        if not 0 <= channel < channels:
            raise ValueError("Invalid audio channel")
        self.path, self.channels, self.channel = Path(path), channels, channel
        self.threshold = 32768 * 10 ** (threshold / 20)
        self.offset = 0
        self.last_voice = None
        self.pending = False

    def ready(self, now):
        frame = self.channels * 2
        try:
            end = self.path.stat().st_size // frame * frame
            if end < self.offset:
                self.offset = 0
                self.last_voice = None
                self.pending = False
            # Scan new audio in 20 ms blocks so a brief word is not averaged
            # away by a longer pause. Bound catch-up work to the last 2 seconds.
            start = max(self.offset, end - 96000 * frame)
            with self.path.open("rb") as stream:
                stream.seek(start)
                data = stream.read(end - start)
            self.offset = end
            samples = array.array("h", data[:len(data)//frame*frame])
            if sys.byteorder != "little":
                samples.byteswap()
            samples = samples[self.channel::self.channels]
            loud = any(math.sqrt(sum(v*v for v in samples[i:i+960]) / len(samples[i:i+960]))
                       >= self.threshold for i in range(0, len(samples), 960))
            if loud:
                self.last_voice = now
                self.pending = True
                return True
        except OSError:
            pass
        # One settling request after a pause lets the recognizer finish the
        # last word. Then freeze the preview until new above-threshold audio.
        if self.pending and now - self.last_voice >= 0.3:
            self.pending = False
            return True
        return False


if __name__ == "__main__":
    gate = Gate(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[5]))
    recording = Path(sys.argv[4])
    deadline = time.monotonic() + 180
    for _ in sys.stdin:
        while recording.exists() and time.monotonic() < deadline:
            if gate.ready(time.monotonic()):
                print("ready", flush=True)
                break
            time.sleep(0.04)
        else:
            break
