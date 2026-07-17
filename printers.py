"""Discover and provision network printers via CUPS.

On startup the app polls CUPS for discoverable network printers and adds
any that are not already configured (driverless / IPP Everywhere), naming
each queue after the printer model. The live display lists every configured
network printer. Requires membership in the 'lpadmin' group to add printers
(no sudo needed).
"""

from __future__ import annotations

import os
import re
import subprocess

# Test page sent to a printer right after the app auto-provisions it.
TEST_IMAGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "base.jpg")

# CUPS device-URI schemes that indicate a printer reached over the network
# (as opposed to usb://, parallel://, serial://, file://, ...).
_NETWORK_URI_SCHEMES = (
    "ipp", "ipps", "http", "https", "socket", "lpd", "dnssd", "smb",
)


def _run(cmd: list[str], timeout: float) -> str:
    try:
        return subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return ""


def _is_network_uri(uri: str) -> bool:
    return "://" in uri and uri.split(":", 1)[0].lower() in _NETWORK_URI_SCHEMES


# --- currently configured printers -----------------------------------------

def configured_network_queues() -> list[tuple[str, str]]:
    """(queue_name, uri) for every CUPS queue with a network device URI.

    Parses `lpstat -v` lines of the form "device for <name>: <uri>".
    """
    out = _run(["lpstat", "-v"], 3)
    prefix = "device for "
    queues: list[tuple[str, str]] = []
    for line in out.splitlines():
        if not line.startswith(prefix):
            continue
        name, _, uri = line[len(prefix):].partition(":")
        uri = uri.strip()
        if _is_network_uri(uri):
            queues.append((name.strip(), uri))
    return queues


def configured_printers() -> list[str]:
    """Display names of all configured network queues (stable order)."""
    return [_display_name(queue) for queue, _ in configured_network_queues()]


def configured_printer() -> str | None:
    """Display name of the first configured network queue, or None."""
    names = configured_printers()
    return names[0] if names else None


# Driver names CUPS reports that aren't the real printer model. A driverless
# (IPP Everywhere) queue reports its make-and-model as "Printer - IPP
# Everywhere", so we skip those and fall back to the description instead.
_GENERIC_MODELS = {
    "", "unknown", "local printer", "local raw printer",
    "ipp everywhere", "printer - ipp everywhere",
}


def _display_name(queue: str) -> str:
    """Friendly name for a queue: real model, else description, else name."""
    out = _run(["lpoptions", "-p", queue], 3)

    def option(key: str) -> str:
        # values may be quoted ('Brother MFC…') or bare (myprinter)
        m = re.search(rf"{key}='([^']*)'", out) or re.search(rf"{key}=(\S+)", out)
        return m.group(1).strip() if m else ""

    model = option("printer-make-and-model")
    if model and model.lower() not in _GENERIC_MODELS:
        return model
    # driverless queue: the real model is stashed in the description (see
    # add_printer's -D), so prefer it over the generic driver name.
    return option("printer-info") or model or queue


# --- discovery + provisioning ----------------------------------------------

def discover_network_printers() -> list[tuple[str, str]]:
    """All discoverable network printers as (device_uri, make_and_model).

    Uses `lpinfo -l -v`, which browses the network (mDNS) and can take a few
    seconds. Skips backend placeholders (uri = "ipp", "socket", …) and
    devices with an unknown model. Dedupes by URI.
    """
    out = _run(["lpinfo", "-l", "-v"], 25)

    devices: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for line in out.splitlines():
        stripped = line.strip()
        if stripped.startswith("Device:"):
            if current is not None:
                devices.append(current)
            # "Device: uri = <uri>"
            _, _, uri = stripped.partition("=")
            current = {"uri": uri.strip()}
        elif current is not None and "=" in stripped:
            key, _, value = stripped.partition("=")
            current[key.strip()] = value.strip()
    if current is not None:
        devices.append(current)

    found: list[tuple[str, str]] = []
    seen_uris: set[str] = set()
    for d in devices:
        uri = d.get("uri", "")
        model = d.get("make-and-model", "")
        if (
            d.get("class") == "network"
            and _is_network_uri(uri)
            and model
            and model.lower() != "unknown"
            and uri not in seen_uris
        ):
            seen_uris.add(uri)
            found.append((uri, model))
    return found


def discover_network_printer() -> tuple[str, str] | None:
    """(device_uri, make_and_model) of the first discoverable network printer."""
    found = discover_network_printers()
    return found[0] if found else None


def _queue_name(model: str) -> str:
    """A CUPS-safe queue name derived from a model string."""
    name = re.sub(r"[^A-Za-z0-9._-]+", "_", model).strip("_")
    return name or "printer"


def add_printer(uri: str, model: str) -> str | None:
    """Add a driverless (IPP Everywhere) queue named after the model.

    Returns the queue name on success, or None on failure.
    """
    queue = _queue_name(model)
    try:
        result = subprocess.run(
            # -D stashes the real model as the description; a driverless queue
            # otherwise reports its model as the generic "IPP Everywhere".
            ["lpadmin", "-p", queue, "-v", uri, "-m", "everywhere",
             "-D", model, "-E"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return queue if result.returncode == 0 else None


def print_test_page(queue: str) -> bool:
    """Send the test image to a queue. Returns True if the job was accepted."""
    if not os.path.exists(TEST_IMAGE):
        return False
    try:
        result = subprocess.run(
            ["lp", "-d", queue, TEST_IMAGE],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def ensure_printers() -> list[str]:
    """Ensure every discoverable network printer has a CUPS queue.

    Returns display names of all configured network printers (including any
    that were already present). Discovery browses the network and can take
    several seconds, so call this off the render loop.
    """
    existing = configured_network_queues()
    existing_uris = {uri for _, uri in existing}
    existing_names = {name for name, _ in existing}

    for uri, model in discover_network_printers():
        if uri in existing_uris:
            continue
        queue = _queue_name(model)
        if queue in existing_names:
            continue
        added = add_printer(uri, model)
        if added:
            existing_uris.add(uri)
            existing_names.add(added)

    return configured_printers()


def ensure_printer() -> str | None:
    """Back-compat: first network printer display name after ensure_printers()."""
    names = ensure_printers()
    return names[0] if names else None


# CUPS printer-state-reasons → operator-facing labels (subset of IPP).
_REASON_LABELS: dict[str, str] = {
    "media-empty": "Out of paper",
    "media-empty-error": "Out of paper",
    "media-needed": "Out of paper",
    "media-jam": "Paper jam",
    "media-jam-error": "Paper jam",
    "media-low": "Paper low",
    "toner-empty": "Toner empty",
    "toner-empty-error": "Toner empty",
    "toner-low": "Toner low",
    "marker-supply-empty": "Supply empty",
    "marker-supply-low": "Supply low",
    "door-open": "Door open",
    "door-open-error": "Door open",
    "cover-open": "Cover open",
    "paused": "Paused",
    "offline": "Offline",
    "offline-report": "Offline",
    "connecting-to-device": "Connecting",
    "cups-insecure-filter-warning": "Filter warning",
    "cups-missing-filter-warning": "Missing filter",
    "shutdown": "Shutdown",
    "timed-out": "Timed out",
    "stopped": "Stopped",
}

# Reasons that mean the queue is effectively offline (not just stopped).
_OFFLINE_REASONS = frozenset(
    {
        "offline",
        "offline-report",
        "shutdown",
        "connecting-to-device",
        "cups-printer-missing",
    }
)

# Noise IPP always reports when healthy — ignore for status_message.
_BENIGN_REASONS = frozenset(
    {
        "none",
        "-",
        "",
        "media-empty-warning",  # keep real empty errors only via labels above
    }
)


def _normalize_reason(raw: str) -> str:
    r = raw.strip().lower().replace("_", "-")
    # CUPS sometimes suffixes "-error" / "-report" — keep as-is for label map.
    return r


def _human_status_message(reasons: list[str]) -> str | None:
    labels: list[str] = []
    seen: set[str] = set()
    for r in reasons:
        if r in _BENIGN_REASONS:
            continue
        label = _REASON_LABELS.get(r)
        if label is None:
            # Fall back to a cleaned reason token.
            label = r.replace("-", " ").strip()
            if label.endswith(" error"):
                label = label[: -len(" error")]
            label = label[:1].upper() + label[1:] if label else r
        if label and label not in seen:
            seen.add(label)
            labels.append(label)
    if not labels:
        return None
    return "; ".join(labels)


def _parse_lpstat_printer_block(text: str, queue: str) -> tuple[str, list[str]]:
    """Parse ``lpstat -p <queue> -l`` into (status, reasons).

    Status values match wms-api ``Constants::Print::PrinterStates``:
    idle | printing | stopped | offline | unknown
    """
    if not text or not text.strip():
        return "unknown", []

    # First line is typically:
    #   printer NAME is idle.  enabled since ...
    #   printer NAME now printing NAME-42.  enabled since ...
    #   printer NAME disabled since ... -
    #   printer NAME is offline.  ...
    first = ""
    for line in text.splitlines():
        if line.strip():
            first = line.strip()
            break
    low = first.lower()

    reasons: list[str] = []
    # Reason / alert lines are indented under the printer block.
    for line in text.splitlines()[1:]:
        stripped = line.strip()
        if not stripped:
            continue
        # Stop if another printer block starts (shouldn't happen with -p NAME).
        if stripped.lower().startswith("printer ") and not line[:1].isspace():
            break
        # "Alert: media-empty-error" / "Alerts: media-empty, paused"
        if stripped.lower().startswith("alert"):
            _, _, rest = stripped.partition(":")
            for part in re.split(r"[,;]+", rest):
                r = _normalize_reason(part)
                if r and r not in _BENIGN_REASONS:
                    reasons.append(r)
            continue
        # Bare reason tokens CUPS prints as the disable reason body.
        if ":" not in stripped and " " not in stripped:
            r = _normalize_reason(stripped)
            if r and r not in _BENIGN_REASONS:
                reasons.append(r)
            continue
        # "Reason: Out of paper" / "State Reason: media-empty"
        if "reason" in stripped.lower() and ":" in stripped:
            _, _, rest = stripped.partition(":")
            for part in re.split(r"[,;]+", rest):
                r = _normalize_reason(part)
                if r and r not in _BENIGN_REASONS:
                    reasons.append(r)

    # Dedupe preserving order
    seen: set[str] = set()
    uniq_reasons: list[str] = []
    for r in reasons:
        if r not in seen:
            seen.add(r)
            uniq_reasons.append(r)
    reasons = uniq_reasons

    status = "unknown"
    if " is offline" in low or low.endswith(" offline.") or " offline " in low:
        status = "offline"
    elif "disabled" in low:
        status = "stopped"
    elif "now printing" in low or " is printing" in low or "printing" in low:
        status = "printing"
    elif " is idle" in low or low.endswith(" idle."):
        status = "idle"
    elif "stopped" in low:
        status = "stopped"

    # Reasons can force a more severe status even when CUPS still says idle
    # (some drivers report media-empty while idle until a job is sent).
    if any(r in _OFFLINE_REASONS for r in reasons):
        status = "offline"
    elif reasons and status in ("idle", "unknown", "printing"):
        # Condition present (paper out, jam, door, …) → stopped so admin acts.
        # Even if CUPS still says "printing", the job is held at the device.
        actionable = {
            "Out of paper",
            "Paper jam",
            "Door open",
            "Cover open",
            "Toner empty",
            "Paused",
        }
        error_like = any(
            r.endswith("-error")
            or (r in _REASON_LABELS and _REASON_LABELS[r] in actionable)
            for r in reasons
        )
        if error_like:
            status = "stopped"

    if status == "unknown" and queue:
        # Last resort: any non-empty first line implies the queue exists.
        if first:
            status = "idle" if "enable" in low else "unknown"

    return status, reasons


def cups_queue_status(queue: str) -> dict[str, object]:
    """Live CUPS status for one queue: status, status_reasons, status_message."""
    out = _run(["lpstat", "-p", queue, "-l"], 5)
    status, reasons = _parse_lpstat_printer_block(out, queue)
    message = _human_status_message(reasons)
    return {
        "status": status,
        "status_reasons": reasons,
        "status_message": message,
    }


def inventory_payload() -> list[dict[str, object]]:
    """CUPS network printer inventory for heartbeat / report_printers.

    Each item includes:
      cups_name, uri, display_name, status,
      status_reasons (list), status_message (str|None)
    """
    items: list[dict[str, object]] = []
    for queue, uri in configured_network_queues():
        st = cups_queue_status(queue)
        items.append(
            {
                "cups_name": queue,
                "uri": uri,
                "display_name": _display_name(queue),
                "status": st["status"],
                "status_reasons": st["status_reasons"],
                "status_message": st["status_message"],
            }
        )
    return items
