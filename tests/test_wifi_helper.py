"""The root Wi-Fi helper (scripts/wifi-setup) must not write bytecode.

It imports wifi_setup.py from the release slot, which the service user owns.
Run as root, Python would leave root-owned __pycache__ in the slot (the agent
cannot delete it when it replaces that slot), and so would the portal the
helper spawns.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
HELPER = REPO / "scripts" / "wifi-setup"

FAKE_WIFI_SETUP = '''
import json, os, subprocess, sys
import fakedep  # another slot module: must not be cached either


def main():
    child = subprocess.run(
        [sys.executable, "-c", "import os; print(os.environ.get('PYTHONDONTWRITEBYTECODE'))"],
        capture_output=True, text=True, check=True,
    )
    print(json.dumps({
        "dont_write_bytecode": sys.dont_write_bytecode,
        "child_env": child.stdout.strip(),
    }))
    return 0
'''


@unittest.skipIf(
    Path("/opt/vesyl-print/current/wifi_setup.py").is_file(),
    "an installed release would shadow the stand-in wifi_setup.py",
)
class WifiHelperBytecodeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="vp-wifi-helper-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        (self.tmp / "scripts").mkdir()
        shutil.copy2(HELPER, self.tmp / "scripts" / "wifi-setup")
        (self.tmp / "wifi_setup.py").write_text(FAKE_WIFI_SETUP)
        (self.tmp / "fakedep.py").write_text("VALUE = 1\n")

    def test_helper_and_its_children_write_no_bytecode(self) -> None:
        env = {k: v for k, v in os.environ.items() if k != "PYTHONDONTWRITEBYTECODE"}
        out = subprocess.run(
            [sys.executable, str(self.tmp / "scripts" / "wifi-setup")],
            capture_output=True, text=True, env=env, timeout=60,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        report = json.loads(out.stdout.strip().splitlines()[-1])
        self.assertTrue(report["dont_write_bytecode"])
        self.assertEqual(report["child_env"], "1")
        caches = sorted(str(p.relative_to(self.tmp)) for p in self.tmp.rglob("__pycache__"))
        self.assertEqual(caches, [], "the helper wrote bytecode into the slot")


if __name__ == "__main__":
    unittest.main()
