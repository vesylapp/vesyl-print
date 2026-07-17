"""LCD messaging helpers (no Pillow / framebuffer dependency).

Maps agent + OTA status into short labels the display loop can paint.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import update as update_mod
from config import AGENT_VERSION

if TYPE_CHECKING:
    pass

# RGB tuples kept here so tests can assert colors without importing main.
OK = (80, 220, 120)
DOWN = (232, 72, 72)
WARN = (255, 180, 60)


def format_agent_version(version: str | None) -> str:
    """Normalize to a short ``vX.Y.Z`` label for the footer."""
    v = (version or AGENT_VERSION or "").strip()
    if not v:
        return ""
    if not v.lower().startswith("v"):
        v = f"v{v}"
    return v


def _looks_like_post_activate_glitch(ust: update_mod.UpdateStatus) -> bool:
    """True when activate likely succeeded but status was marked failed (self-restart).

    Classic case: ``apply-update restart`` SIGTERMs the agent while it is still
    waiting; status becomes ``failed`` even though ``current`` already points at
    the new release. LCD should show Verifying…, not Update failed.
    """
    target = (ust.target_version or "").strip()
    if not target:
        return False
    err = (ust.last_error or "").lower()
    if "sigterm" in err or "apply-update" in err and "restart" in err:
        return True
    # After activate we set current_version == target before restart.
    cur = (ust.current_version or "").strip()
    if cur and update_mod.version_cmp(cur, target) == 0:
        return True
    return False


def ota_display_message(
    ust: update_mod.UpdateStatus | None,
) -> tuple[str, tuple[int, int, int]] | None:
    """Map update_status → (footer label, color) for the LCD, or None if idle."""
    if ust is None:
        return None
    target = (ust.target_version or "").strip().lstrip("v")
    s = ust.status

    if s == update_mod.STATUS_DOWNLOADING:
        label = f"Updating {target}…".strip() if target else "Updating…"
        return label, WARN
    if s == update_mod.STATUS_INSTALLING:
        label = f"Installing {target}…".strip() if target else "Installing…"
        return label, WARN
    if s == update_mod.STATUS_PENDING_HEALTH:
        label = f"Verifying {target}…".strip() if target else "Verifying…"
        return label, WARN
    if s == update_mod.STATUS_FAILED:
        # Don't flash red "Update failed" for self-restart false negatives.
        if _looks_like_post_activate_glitch(ust):
            label = f"Verifying {target}…".strip() if target else "Verifying…"
            return label, WARN
        return "Update failed", DOWN
    if s == update_mod.STATUS_ROLLED_BACK:
        return "Rolled back", WARN
    return None
