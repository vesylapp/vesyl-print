"""Paths and API base URL for the LCD display.

The Rust agent (``vesyl-print``) owns ``config.json`` and every other setting;
the display only needs the config/state directories (to find the files the
agent writes) and the API base URL it shows on the stream page.
"""

from __future__ import annotations

import json
import os
import platform
from dataclasses import dataclass, field
from pathlib import Path

AGENT_VERSION = (
    Path(__file__).resolve().parent / "VERSION"
).read_text(encoding="utf-8").strip()

# Preferred for Pis: direct API host (paths are /print/v1/...).
DEFAULT_API_BASE_URL = "https://wms-api.vesyl.dev"

ENV_API_URL = "VESYL_PRINT_API_URL"
ENV_CONFIG_DIR = "VESYL_PRINT_CONFIG_DIR"
ENV_STATE_DIR = "VESYL_PRINT_STATE_DIR"


def default_platform() -> str:
    """e.g. linux-aarch64, linux-x86_64."""
    system = platform.system().lower() or "linux"
    machine = platform.machine().lower() or "unknown"
    return f"{system}-{machine}"


def _user_config_dir() -> Path:
    xdg = os.environ.get("XDG_CONFIG_HOME")
    if xdg:
        return Path(xdg) / "vesyl-print"
    return Path.home() / ".config" / "vesyl-print"


def _user_state_dir() -> Path:
    xdg = os.environ.get("XDG_STATE_HOME") or os.environ.get("XDG_DATA_HOME")
    if xdg:
        return Path(xdg) / "vesyl-print"
    return Path.home() / ".local" / "share" / "vesyl-print"


def resolve_config_dir() -> Path:
    if env := os.environ.get(ENV_CONFIG_DIR):
        return Path(env)
    system = Path("/etc/vesyl-print")
    if system.is_dir():
        return system
    return _user_config_dir()


def resolve_state_dir() -> Path:
    if env := os.environ.get(ENV_STATE_DIR):
        return Path(env)
    system = Path("/var/lib/vesyl-print")
    if system.is_dir():
        return system
    return _user_state_dir()


@dataclass
class Config:
    api_base_url: str = DEFAULT_API_BASE_URL
    config_dir: Path = field(default_factory=resolve_config_dir)
    state_dir: Path = field(default_factory=resolve_state_dir)

    def __post_init__(self) -> None:
        self.api_base_url = str(self.api_base_url).rstrip("/")
        self.config_dir = Path(self.config_dir)
        self.state_dir = Path(self.state_dir)

    @property
    def credentials_path(self) -> Path:
        return self.config_dir / "credentials.json"

    @property
    def config_path(self) -> Path:
        return self.config_dir / "config.json"

    @property
    def status_path(self) -> Path:
        return self.state_dir / "status.json"

    @property
    def update_status_path(self) -> Path:
        return self.state_dir / "update_status.json"

    @property
    def printers_path(self) -> Path:
        """Printer inventory snapshot the agent publishes for the display."""
        return self.state_dir / "printers.json"

    @property
    def queue_dir(self) -> Path:
        return self.state_dir / "queue"

    @property
    def processed_dir(self) -> Path:
        return self.state_dir / "processed"


def load_config(
    config_dir: Path | None = None,
    state_dir: Path | None = None,
) -> Config:
    """Resolve dirs and the API base URL (config.json, then env). Missing file is fine."""
    cdir = Path(config_dir) if config_dir else resolve_config_dir()
    sdir = Path(state_dir) if state_dir else resolve_state_dir()
    cfg = Config(config_dir=cdir, state_dir=sdir)

    path = cfg.config_path
    if path.is_file():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(data, dict) and (url := data.get("api_base_url")):
                cfg.api_base_url = str(url).rstrip("/")
        except (OSError, TypeError, ValueError):
            pass

    if env_url := os.environ.get(ENV_API_URL):
        cfg.api_base_url = env_url.rstrip("/")
    return cfg
