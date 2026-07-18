"""Load vesyl-print configuration (paths, API base URL, intervals)."""

from __future__ import annotations

import json
import os
import platform
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

def _read_version_file() -> str:
    try:
        p = Path(__file__).resolve().parent / "VERSION"
        if p.is_file():
            v = p.read_text(encoding="utf-8").strip()
            if v:
                return v
    except OSError:
        pass
    return "0.3.8"


AGENT_VERSION = _read_version_file()

# Preferred for Pis: direct API host (paths are /print/v1/...).
DEFAULT_API_BASE_URL = "https://wms-api.vesyl.dev"
# GitHub Releases act as the artifact CDN (see OTA_UPDATES.md / release workflow).
DEFAULT_RELEASES_BASE_URL = (
    "https://github.com/vesylapp/vesyl-print/releases/download"
)

ENV_API_URL = "VESYL_PRINT_API_URL"
ENV_CONFIG_DIR = "VESYL_PRINT_CONFIG_DIR"
ENV_STATE_DIR = "VESYL_PRINT_STATE_DIR"
ENV_INSTALL_ROOT = "VESYL_PRINT_INSTALL_ROOT"


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
    # Phase C: poll GET /print/v1/jobs/pending (disable if server lacks PR4).
    pull_jobs_enabled: bool = True
    # Phase D: ActionCable PrintNodeChannel push (pull remains safety net).
    cable_enabled: bool = True
    # OTA (app): cloud sets desired_agent_version on heartbeat response.
    auto_update_enabled: bool = True
    update_channel: str = "stable"
    releases_base_url: str = DEFAULT_RELEASES_BASE_URL
    update_require_signature: bool = True
    update_public_key_path: str = ""  # empty → keys/update_public.pem
    # Seconds after activate to pass whoami/local health before auto-rollback.
    update_health_gate_seconds: int = 120
    config_dir: Path = field(default_factory=resolve_config_dir)
    state_dir: Path = field(default_factory=resolve_state_dir)

    def __post_init__(self) -> None:
        self.api_base_url = str(self.api_base_url).rstrip("/")
        self.releases_base_url = str(self.releases_base_url).rstrip("/")
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
    def update_status_path(self) -> Path:
        return self.state_dir / "update_status.json"

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
    if "cable_enabled" in data:
        cfg.cable_enabled = bool(data["cable_enabled"])
    if "auto_update_enabled" in data:
        cfg.auto_update_enabled = bool(data["auto_update_enabled"])
    if ch := data.get("update_channel"):
        cfg.update_channel = str(ch)
    if rb := data.get("releases_base_url"):
        cfg.releases_base_url = str(rb).rstrip("/")
    if "update_require_signature" in data:
        cfg.update_require_signature = bool(data["update_require_signature"])
    if kp := data.get("update_public_key_path"):
        cfg.update_public_key_path = str(kp)
    if "update_health_gate_seconds" in data:
        try:
            cfg.update_health_gate_seconds = max(15, int(data["update_health_gate_seconds"]))
        except (TypeError, ValueError):
            pass


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
        "pull_jobs_enabled": True,
        "cable_enabled": True,
        "auto_update_enabled": True,
        "update_channel": "stable",
        "releases_base_url": DEFAULT_RELEASES_BASE_URL,
    }
    out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return out
