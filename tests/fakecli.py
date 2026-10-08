"""Fake ``vesyl-print`` binary for display tests: a shell script on a temp path."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def make_fake_cli(
    directory: str | Path,
    stdout: str | dict[str, Any] = "",
    *,
    stderr: str = "",
    rc: int = 0,
    sleep: float = 0.0,
) -> Path:
    """Write ``directory/vesyl-print`` and return its path.

    When run it records its argv (one per line) in ``args.txt`` and
    ``$VESYL_PRINT_CONFIG_DIR`` / ``$VESYL_PRINT_STATE_DIR`` in ``env.txt``,
    then prints ``stdout`` (a dict is printed as JSON) and ``stderr`` and
    exits with ``rc``. ``sleep`` makes it hang (for timeouts).
    """
    d = Path(directory)
    if not isinstance(stdout, str):
        stdout = json.dumps(stdout) + "\n"
    (d / "stdout.txt").write_text(stdout, encoding="utf-8")
    (d / "stderr.txt").write_text(stderr, encoding="utf-8")
    exe = d / "vesyl-print"
    exe.write_text(
        "#!/bin/sh\n"
        'd=$(dirname "$0")\n'
        ': > "$d/args.txt"\n'
        'for a in "$@"; do printf \'%s\\n\' "$a" >> "$d/args.txt"; done\n'
        'printf \'%s\\n\' "${VESYL_PRINT_CONFIG_DIR-}" "${VESYL_PRINT_STATE_DIR-}"'
        ' > "$d/env.txt"\n'
        + (f"exec sleep {sleep:g}\n" if sleep else "")
        + 'cat "$d/stderr.txt" >&2\n'
        'cat "$d/stdout.txt"\n'
        f"exit {int(rc)}\n",
        encoding="utf-8",
    )
    exe.chmod(0o755)
    return exe


def recorded_args(directory: str | Path) -> list[str] | None:
    """The argv the fake binary was last run with (None if it never ran)."""
    p = Path(directory) / "args.txt"
    if not p.is_file():
        return None
    return p.read_text(encoding="utf-8").splitlines()


def recorded_env(directory: str | Path) -> list[str]:
    """``[VESYL_PRINT_CONFIG_DIR, VESYL_PRINT_STATE_DIR]`` seen by the fake binary."""
    return (Path(directory) / "env.txt").read_text(encoding="utf-8").splitlines()
