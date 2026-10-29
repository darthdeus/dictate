#!/usr/bin/env python3
"""Exercise the `bin/dictate-editor` learn-editor handoff without launching a real editor.

`DICTATE_LAUNCHER` tests another wrapper with the same contract instead."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


LAUNCHER = Path(
    os.environ.get(
        "DICTATE_LAUNCHER", str(Path(__file__).resolve().parent.parent / "bin/dictate-editor")
    )
).resolve()

FAKE_COMMAND = """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
trace = Path(os.environ["TRACE_DIR"]) / f"{name}.json"
trace.write_text(json.dumps(sys.argv[1:]), encoding="utf-8")
if name == "dictate":
    raise SystemExit(int(os.environ.get("FAKE_DICTATE_STATUS", "0")))
raise SystemExit(0)
"""


class LearnEditorLauncherTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="check-learn-editor-")
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.state_home = self.root / "state"
        self.trace_dir = self.root / "trace"
        self.trace_dir.mkdir()
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        self.dictate = self.fake_bin / "dictate"
        self.nvim = self.fake_bin / "nvim"
        for command in (self.dictate, self.nvim):
            command.write_text(FAKE_COMMAND, encoding="utf-8")
            command.chmod(0o755)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def pending(self, pane: str) -> Path:
        path = self.state_home / "dictate/learn/pending" / f"pane-{pane}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{}\n", encoding="utf-8")
        return path

    def run_launcher(
        self,
        args: list[str],
        *,
        pane: str | None,
        dictate_status: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(self.home),
                "XDG_STATE_HOME": str(self.state_home),
                "NVIM_RUNTIME_DIR": str(self.root),
                "PATH": f"{self.fake_bin}:{env.get('PATH', '/usr/bin:/bin')}",
                "DICTATE_LEARN_BIN": str(self.dictate),
                "DICTATE_EDITOR": "nvim",
                "TRACE_DIR": str(self.trace_dir),
                "FAKE_DICTATE_STATUS": str(dictate_status),
            }
        )
        if pane is None:
            env.pop("TMUX_PANE", None)
        else:
            env["TMUX_PANE"] = pane

        return subprocess.run(
            [str(LAUNCHER), *args],
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def trace(self, command: str) -> list[str] | None:
        path = self.trace_dir / f"{command}.json"
        if not path.exists():
            return None
        return json.loads(path.read_text(encoding="utf-8"))

    def assert_nvim_args(self, args: list[str]) -> None:
        self.assertEqual(self.trace("nvim"), args)

    def test_no_pending_capture_opens_nvim(self) -> None:
        args = ["+12", "file with spaces.txt"]
        result = self.run_launcher(args, pane="%10")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(self.trace("dictate"))
        self.assert_nvim_args(args)

    def test_handled_capture_skips_nvim_and_preserves_args(self) -> None:
        self.pending("10")
        args = ["+42", "--", "file with spaces.txt", "two words"]
        result = self.run_launcher(args, pane="%10")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.trace("dictate"), ["learn-editor", "--", *args])
        self.assertIsNone(self.trace("nvim"))

    def test_exit_three_falls_through_to_nvim(self) -> None:
        self.pending("10")
        args = ["+7", "--", "draft.md"]
        result = self.run_launcher(args, pane="%10", dictate_status=3)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.trace("dictate"), ["learn-editor", "--", *args])
        self.assert_nvim_args(args)

    def test_other_failure_is_preserved_without_opening_nvim(self) -> None:
        self.pending("10")
        args = ["+1", "draft.md"]
        result = self.run_launcher(args, pane="%10", dictate_status=7)

        self.assertEqual(result.returncode, 7)
        self.assertEqual(self.trace("dictate"), ["learn-editor", "--", *args])
        self.assertIsNone(self.trace("nvim"))

    def test_invalid_pane_falls_through_to_nvim(self) -> None:
        self.pending("bad")
        args = ["+3", "draft.md"]
        result = self.run_launcher(args, pane="%bad")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(self.trace("dictate"))
        self.assert_nvim_args(args)

    def test_pending_for_different_pane_falls_through_to_nvim(self) -> None:
        self.pending("22")
        args = ["+8", "draft.md"]
        result = self.run_launcher(args, pane="%17")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(self.trace("dictate"))
        self.assert_nvim_args(args)


if __name__ == "__main__":
    unittest.main()
