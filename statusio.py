"""Agent → LCD status files (JSON under state_dir, written by the Rust agent).

- ``status.json``: pairing, cloud and identity (:func:`read_status`).
- ``printers.json``: the agent's printer inventory, rewritten about every
  15 s while it runs (:func:`read_printers`)::

      {"updated_at": "<utc iso>",
       "printers": [{"cups_name", "uri", "display_name", "status",
                     "status_reasons", "status_message", "supports_raw"}, ...]}

  A snapshot older than :data:`PRINTERS_STALE_AFTER_S` (120 s, by its
  ``updated_at``, or the file mtime when that is missing or unparseable)
  means the agent stopped refreshing it: the printers are still listed, but
  every ``status`` reads ``"unknown"`` (no ``status_message``), so the LCD
  never shows a stale "idle".
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal

PairingState = Literal["unpaired", "paired", "revoked"]
CloudState = Literal["unknown", "online", "offline"]

PRINTERS_STALE_AFTER_S = 120.0


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


@dataclass
class PrintersSnapshot:
    """``printers.json`` as the display uses it (statuses already blanked when stale)."""

    printers: list[dict[str, Any]] = field(default_factory=list)
    updated_at: str | None = None
    stale: bool = False


def _parse_utc(raw: Any) -> datetime | None:
    if not isinstance(raw, str) or not raw.strip():
        return None
    text = raw.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        ts = datetime.fromisoformat(text)
    except ValueError:
        return None
    if ts.tzinfo is None:
        ts = ts.replace(tzinfo=timezone.utc)
    return ts


def load_printers(
    path: Path,
    *,
    now: datetime | None = None,
    stale_after_s: float = PRINTERS_STALE_AFTER_S,
) -> PrintersSnapshot:
    """Read ``printers.json``. Raises ``OSError`` / ``ValueError`` when it is
    missing, unreadable or not ``{"printers": [...]}``."""
    path = Path(path)
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("printers"), list):
        raise ValueError(f"{path}: expected an object with a printers list")
    items = [dict(p) for p in data["printers"] if isinstance(p, dict)]

    now_dt = now or datetime.now(timezone.utc)
    if now_dt.tzinfo is None:
        now_dt = now_dt.replace(tzinfo=timezone.utc)
    updated = _parse_utc(data.get("updated_at"))
    age: float | None
    if updated is not None:
        age = (now_dt - updated).total_seconds()
    else:
        try:
            age = now_dt.timestamp() - path.stat().st_mtime
        except OSError:
            age = None
    stale = age is None or age > stale_after_s
    if stale:
        items = [
            {**p, "status": "unknown", "status_message": None, "status_reasons": []}
            for p in items
        ]
    raw_updated = data.get("updated_at")
    return PrintersSnapshot(
        printers=items,
        updated_at=raw_updated if isinstance(raw_updated, str) else None,
        stale=stale,
    )


def read_printers(
    path: Path,
    *,
    now: datetime | None = None,
    stale_after_s: float = PRINTERS_STALE_AFTER_S,
) -> PrintersSnapshot | None:
    """:func:`load_printers`, or ``None`` when the file is missing or unreadable."""
    try:
        return load_printers(path, now=now, stale_after_s=stale_after_s)
    except (OSError, ValueError):
        return None
