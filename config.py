"""Load vesyl-print configuration (paths, API base URL, intervals)."""

from __future__ import annotations

import json
import os
import platform
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

AGENT_VERSION = "0.3.0"

# Preferred for Pis: direct API host (paths are /print/v1/...).
DEFAULT_API_BASE_URL = "https://wms.api.staging.vesyl.com"

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


def _derive_cable_url(api_base_url: str) -> str:
    """Guess ActionCable URL from REST base (used when cable_url omitted)."""
    base = api_base_url.rstrip("/")
    if base.endswith("/api"):
        origin = base[: -len("/api")]
    else:
        origin = base
    if origin.startswith("https://"):
        return "wss://" + origin[len("https://") :] + "/print/cable"
    if origin.startswith("http://"):
        return "ws://" + origin[len("http://") :] + "/print/cable"
    return origin + "/print/cable"


@dataclass
class Config:
    api_base_url: str = DEFAULT_API_BASE_URL
    cable_url: str = ""
    heartbeat_seconds: int = 30
    pull_interval_seconds: int = 5
    pull_jobs_enabled: bool = False
    config_dir: Path = field(default_factory=resolve_config_dir)
    state_dir: Path = field(default_factory=resolve_state_dir)

    def __post_init__(self) -> None:
        self.api_base_url = str(self.api_base_url).rstrip("/")
        self.config_dir = Path(self.config_dir)
        self.state_dir = Path(self.state_dir)
        if not self.cable_url:
            self.cable_url = _derive_cable_url(self.api_base_url)

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
    def queue_dir(self) -> Path:
        return self.state_dir / "queue"

    @property
    def processed_dir(self) -> Path:
        return self.state_dir / "processed"

    def ensure_dirs(self) -> None:
        """Create config/state dirs used by agent and CLI (best-effort)."""
        self.config_dir.mkdir(parents=True, exist_ok=True)
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.queue_dir.mkdir(parents=True, exist_ok=True)
        self.processed_dir.mkdir(parents=True, exist_ok=True)


def _apply_file(data: dict[str, Any], cfg: Config) -> None:
    if url := data.get("api_base_url"):
        cfg.api_base_url = str(url).rstrip("/")
    if cable := data.get("cable_url"):
        cfg.cable_url = str(cable)
    if "heartbeat_seconds" in data:
        cfg.heartbeat_seconds = int(data["heartbeat_seconds"])
    if "pull_interval_seconds" in data:
        cfg.pull_interval_seconds = int(data["pull_interval_seconds"])
    if "pull_jobs_enabled" in data:
        cfg.pull_jobs_enabled = bool(data["pull_jobs_enabled"])


def load_config(
    config_dir: Path | None = None,
    state_dir: Path | None = None,
) -> Config:
    """Load config.json + env overrides. Missing file is fine (defaults)."""
    cdir = Path(config_dir) if config_dir else resolve_config_dir()
    sdir = Path(state_dir) if state_dir else resolve_state_dir()
    cfg = Config(config_dir=cdir, state_dir=sdir, cable_url="")

    file_cable: str | None = None
    path = cfg.config_path
    if path.is_file():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                if data.get("cable_url"):
                    file_cable = str(data["cable_url"])
                _apply_file(data, cfg)
        except (OSError, json.JSONDecodeError, TypeError, ValueError):
            pass

    if env_url := os.environ.get(ENV_API_URL):
        cfg.api_base_url = env_url.rstrip("/")

    cfg.api_base_url = cfg.api_base_url.rstrip("/")
    # Env API change should re-derive cable unless config.json set it explicitly.
    if file_cable:
        cfg.cable_url = file_cable
    else:
        cfg.cable_url = _derive_cable_url(cfg.api_base_url)
    return cfg


def write_default_config(path: Path | None = None) -> Path:
    """Write a starter config.json if missing. Returns path written/existing."""
    cfg = load_config()
    out = path or cfg.config_path
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.is_file():
        return out
    payload = {
        "api_base_url": cfg.api_base_url,
        "cable_url": cfg.cable_url,
        "heartbeat_seconds": cfg.heartbeat_seconds,
        "pull_interval_seconds": cfg.pull_interval_seconds,
        "pull_jobs_enabled": False,
    }
    out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return out
