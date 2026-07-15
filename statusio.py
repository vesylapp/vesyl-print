"""Agent ↔ LCD status file (JSON under state_dir)."""

from __future__ import annotations

import json
import os
import tempfile
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal

PairingState = Literal["unpaired", "paired", "revoked"]
CloudState = Literal["unknown", "online", "offline"]


@dataclass
class AgentStatus:
    pairing: PairingState = "unpaired"
    cloud: CloudState = "unknown"
    node_id: str | None = None
    name: str | None = None
    organization_name: str | None = None
    warehouse_name: str | None = None
    last_heartbeat_at: str | None = None
    last_error: str | None = None
    agent_version: str | None = None
    updated_at: str | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        extra = d.pop("extra", {}) or {}
        d.update(extra)
        return d


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def write_status(path: Path, status: AgentStatus) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    status.updated_at = _utc_now_iso()
    raw = json.dumps(status.to_dict(), indent=2) + "\n"
    # Atomic write so LCD never reads a partial file.
    fd, tmp_name = tempfile.mkstemp(
        dir=str(path.parent), prefix=".status.", suffix=".tmp"
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(raw)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp_name, path)
    except Exception:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def read_status(path: Path) -> AgentStatus | None:
    path = Path(path)
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(data, dict):
        return None
    pairing = data.get("pairing", "unpaired")
    if pairing not in ("unpaired", "paired", "revoked"):
        pairing = "unpaired"
    cloud = data.get("cloud", "unknown")
    if cloud not in ("unknown", "online", "offline"):
        cloud = "unknown"
    return AgentStatus(
        pairing=pairing,  # type: ignore[arg-type]
        cloud=cloud,  # type: ignore[arg-type]
        node_id=data.get("node_id"),
        name=data.get("name"),
        organization_name=data.get("organization_name"),
        warehouse_name=data.get("warehouse_name"),
        last_heartbeat_at=data.get("last_heartbeat_at"),
        last_error=data.get("last_error"),
        agent_version=data.get("agent_version"),
        updated_at=data.get("updated_at"),
    )
