"""Discover and provision network printers via CUPS.

On startup the app polls CUPS for discoverable network printers and adds
any that are not already configured (driverless / IPP Everywhere), naming
each queue after the printer model. The live display lists every configured
network printer. Requires membership in the 'lpadmin' group to add printers
(no sudo needed).
"""

from __future__ import annotations

import logging
import os
import re
import subprocess
import tempfile
from pathlib import Path

# Test page sent to a printer right after the app auto-provisions it.
TEST_IMAGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "base.jpg")

log = logging.getLogger("vesyl-print.printers")

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


# CUPS / IPP printer-state-reasons → operator-facing labels (subset of IPP).
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
    "input-tray-missing": "Tray missing",
    "output-tray-missing": "Output tray missing",
    "paused": "Paused",
    "offline": "Offline",
    "offline-report": "Offline",
    "connecting-to-device": "Connecting",
    "cups-insecure-filter-warning": "Filter warning",
    "cups-missing-filter-warning": "Missing filter",
    "shutdown": "Shutdown",
    "timed-out": "Timed out",
    "stopped": "Stopped",
    # Job-level reasons that surface during paper-out holds
    "job-hold-until-specified": "Held",
    "resources-are-not-ready": "Resources not ready",
    "printer-stopped": "Printer stopped",
    "printer-stopped-partly": "Printer stopped",
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

# Actionable device conditions → force status "stopped" for admin.
_ACTIONABLE_REASON_PREFIXES = (
    "media-empty",
    "media-needed",
    "media-jam",
    "toner-empty",
    "marker-supply-empty",
    "door-open",
    "cover-open",
    "input-tray-missing",
    "output-tray-missing",
    "paused",
    "stopped",
    "printer-stopped",
    "resources-are-not-ready",
)

# Noise IPP always reports when healthy — ignore for status_message.
_BENIGN_REASONS = frozenset(
    {
        "none",
        "-",
        "",
        # Informational CUPS progress tokens — not operator faults.
        "cups-waiting-for-job-completed",
        "job-printing",
        "job-completed-successfully",
        "processing-to-stop-point",
        "moving-to-paused",
    }
)

# IPP printer-state enum → our status strings.
_IPP_STATE_MAP = {
    "3": "idle",
    "idle": "idle",
    "4": "printing",
    "processing": "printing",
    "5": "stopped",
    "stopped": "stopped",
}

# Embedded Get-Printer-Attributes test used with ipptool (no external file dep).
_IPP_GET_PRINTER_TEST = """\
{
OPERATION Get-Printer-Attributes
GROUP operation-attributes-tag
ATTR charset attributes-charset utf-8
ATTR naturalLanguage attributes-natural-language en
ATTR uri printer-uri $uri
ATTR keyword requested-attributes printer-state,printer-state-reasons,printer-state-message,queued-job-count,printer-is-accepting-jobs
}
"""

_IPP_GET_JOBS_TEST = """\
{
OPERATION Get-Jobs
GROUP operation-attributes-tag
ATTR charset attributes-charset utf-8
ATTR naturalLanguage attributes-natural-language en
ATTR uri printer-uri $uri
ATTR keyword which-jobs not-completed
ATTR keyword requested-attributes job-id,job-state,job-state-reasons,job-printer-state-reasons,job-printer-state-message,job-state-message
}
"""


def _normalize_reason(raw: str) -> str:
    r = raw.strip().lower().replace("_", "-")
    # Strip surrounding quotes from ipptool dumps.
    return r.strip("\"'")


def _dedupe_reasons(reasons: list[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for r in reasons:
        if r in _BENIGN_REASONS or r in seen:
            continue
        seen.add(r)
        out.append(r)
    return out


def _human_status_message(
    reasons: list[str], *, state_message: str | None = None
) -> str | None:
    """Prefer explicit IPP printer-state-message, else map reasons to labels."""
    if state_message:
        msg = state_message.strip()
        if msg and msg.lower() not in ("none", "-", "n/a"):
            return msg

    labels: list[str] = []
    seen: set[str] = set()
    for r in reasons:
        if r in _BENIGN_REASONS:
            continue
        label = _REASON_LABELS.get(r)
        if label is None:
            # media-empty-warning etc. → try base token before -warning/-report
            base = re.sub(r"-(warning|report|error)$", "", r)
            label = _REASON_LABELS.get(base)
        if label is None:
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


def _reason_is_actionable(reason: str) -> bool:
    if reason in _BENIGN_REASONS:
        return False
    if reason.endswith("-error"):
        return True
    return any(
        reason == p or reason.startswith(p + "-") for p in _ACTIONABLE_REASON_PREFIXES
    )


def _status_from_state_and_reasons(
    base_status: str, reasons: list[str]
) -> str:
    """Upgrade idle/printing → stopped/offline when device conditions present."""
    if any(r in _OFFLINE_REASONS for r in reasons):
        return "offline"
    if any(_reason_is_actionable(r) for r in reasons):
        return "stopped"
    return base_status if base_status in (
        "idle", "printing", "stopped", "offline", "unknown"
    ) else "unknown"


def _parse_ipp_attr_values(text: str, attr_name: str) -> list[str]:
    """Extract values for an attribute from ``ipptool -tv`` output.

    Handles:
      attr (keyword) = media-empty
      attr (1setOf keyword) = media-empty,media-needed
      attr (enum) = processing
      attr (textWithoutLanguage) = Out of paper

    Line-based: empty values like ``message = `` must not swallow the next
    attribute (``\\s*`` after ``=`` would otherwise consume the newline).
    """
    values: list[str] = []
    # Only horizontal whitespace after '=' so blank values stay blank.
    pat = re.compile(
        rf"^\s*{re.escape(attr_name)}\s*\([^)]*\)\s*=[ \t]*(.*)$",
        re.IGNORECASE,
    )
    for line in (text or "").splitlines():
        m = pat.match(line)
        if not m:
            continue
        raw = m.group(1).strip()
        if not raw:
            continue
        # 1setOf often comma-separated on one line.
        for p in re.split(r"\s*,\s*", raw):
            p = p.strip().strip("\"'")
            if p:
                values.append(p)
    return values


def _parse_ipp_printer_attrs(text: str) -> dict[str, object]:
    """Parse Get-Printer-Attributes ipptool -tv dump → status fields."""
    states = _parse_ipp_attr_values(text, "printer-state")
    reason_raw = _parse_ipp_attr_values(text, "printer-state-reasons")
    messages = _parse_ipp_attr_values(text, "printer-state-message")
    queued = _parse_ipp_attr_values(text, "queued-job-count")

    base = "unknown"
    if states:
        token = states[0].strip().lower()
        # enum may be "processing" or integer "4"
        base = _IPP_STATE_MAP.get(token, "unknown")
        if base == "unknown" and token.isdigit():
            base = _IPP_STATE_MAP.get(token, "unknown")

    reasons = _dedupe_reasons([_normalize_reason(r) for r in reason_raw])
    state_message = messages[0].strip() if messages else None
    if state_message in ("",):
        state_message = None

    queued_n = 0
    if queued:
        try:
            queued_n = int(queued[0])
        except ValueError:
            queued_n = 0

    status = _status_from_state_and_reasons(base, reasons)
    return {
        "status": status,
        "status_reasons": reasons,
        "status_message": _human_status_message(reasons, state_message=state_message),
        "queued_job_count": queued_n,
        "base_status": base,
    }


def _parse_ipp_jobs_attrs(text: str) -> dict[str, object]:
    """Pull condition reasons from active jobs (paper-out often lives here)."""
    reasons: list[str] = []
    for attr in (
        "job-printer-state-reasons",
        "job-state-reasons",
        "job-printer-state-message",
        "job-state-message",
    ):
        for v in _parse_ipp_attr_values(text, attr):
            # Messages may be free text; still try to normalize token-ish values.
            if " " in v and attr.endswith("message"):
                # Free-text message — keep as pseudo-reason only if known phrase.
                low = v.strip().lower()
                if "out of paper" in low or "paper empty" in low or "no paper" in low:
                    reasons.append("media-empty")
                elif "paper jam" in low or "jam" in low:
                    reasons.append("media-jam")
                elif "door" in low or "cover" in low:
                    reasons.append("door-open")
                continue
            reasons.append(_normalize_reason(v))
    return {"status_reasons": _dedupe_reasons(reasons)}


def _ipptool(uri: str, test_body: str, *, timeout: float = 5.0) -> str:
    """Run ipptool -tv against *uri* with an inline test file; return stdout."""
    try:
        with tempfile.NamedTemporaryFile(
            "w", suffix=".test", prefix="vesyl-ipp-", delete=False, encoding="utf-8"
        ) as f:
            f.write(test_body)
            path = f.name
        try:
            result = subprocess.run(
                ["ipptool", "-tv", "-T", str(max(1, int(timeout))), uri, path],
                capture_output=True,
                text=True,
                timeout=timeout + 2.0,
            )
            return (result.stdout or "") + (result.stderr or "")
        finally:
            try:
                Path(path).unlink(missing_ok=True)
            except OSError:
                pass
    except (OSError, subprocess.SubprocessError) as e:
        log.debug("ipptool %s failed: %s", uri, e)
        return ""


def _ipp_local_uri(queue: str) -> str:
    # CUPS always exposes local queues on the loopback IPP service.
    return f"ipp://localhost/printers/{queue}"


def _ipp_query_queue(queue: str, device_uri: str | None = None) -> dict[str, object] | None:
    """Best-effort IPP status for a CUPS queue (+ optional device URI merge)."""
    local_uri = _ipp_local_uri(queue)
    raw = _ipptool(local_uri, _IPP_GET_PRINTER_TEST, timeout=4.0)
    if not raw or "printer-state" not in raw:
        return None

    parsed = _parse_ipp_printer_attrs(raw)

    # Active jobs often carry media-empty / jam when the queue still says
    # "processing" with reasons=none (common on driverless IPP Everywhere).
    jobs_raw = _ipptool(local_uri, _IPP_GET_JOBS_TEST, timeout=4.0)
    if jobs_raw:
        job_info = _parse_ipp_jobs_attrs(jobs_raw)
        job_reasons = list(job_info.get("status_reasons") or [])
        if job_reasons:
            merged = _dedupe_reasons(list(parsed["status_reasons"]) + job_reasons)
            parsed["status_reasons"] = merged
            parsed["status"] = _status_from_state_and_reasons(
                str(parsed.get("base_status") or parsed["status"]), merged
            )
            parsed["status_message"] = _human_status_message(
                merged,
                state_message=str(parsed["status_message"])
                if parsed.get("status_message")
                else None,
            )

    # When the local queue still has no actionable reason, ask the device itself.
    # Skip implicitclass / non-IPP backends (no Get-Printer-Attributes).
    if device_uri and not any(_reason_is_actionable(r) for r in parsed["status_reasons"]):
        scheme = device_uri.split(":", 1)[0].lower()
        if scheme in ("ipp", "ipps", "http", "https"):
            dev_raw = _ipptool(device_uri, _IPP_GET_PRINTER_TEST, timeout=3.0)
            if dev_raw and "printer-state" in dev_raw:
                dev = _parse_ipp_printer_attrs(dev_raw)
                dev_reasons = list(dev.get("status_reasons") or [])
                if dev_reasons:
                    merged = _dedupe_reasons(
                        list(parsed["status_reasons"]) + dev_reasons
                    )
                    parsed["status_reasons"] = merged
                    # Prefer device base state when more severe / informative.
                    base = str(
                        dev.get("base_status")
                        or parsed.get("base_status")
                        or parsed["status"]
                    )
                    parsed["status"] = _status_from_state_and_reasons(base, merged)
                    msg = dev.get("status_message") or parsed.get("status_message")
                    parsed["status_message"] = _human_status_message(
                        merged,
                        state_message=str(msg) if msg else None,
                    )

    return {
        "status": parsed["status"],
        "status_reasons": parsed["status_reasons"],
        "status_message": parsed["status_message"],
    }


def _parse_lpstat_printer_block(text: str, queue: str) -> tuple[str, list[str]]:
    """Parse ``lpstat -p <queue> -l`` into (status, reasons) — fallback only.

    ``lpstat`` rarely exposes media-empty; prefer IPP via :func:`_ipp_query_queue`.
    Status values match wms-api ``Constants::Print::PrinterStates``.
    """
    if not text or not text.strip():
        return "unknown", []

    first = ""
    for line in text.splitlines():
        if line.strip():
            first = line.strip()
            break
    low = first.lower()

    reasons: list[str] = []
    for line in text.splitlines()[1:]:
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.lower().startswith("printer ") and not line[:1].isspace():
            break
        if stripped.lower().startswith("alert"):
            _, _, rest = stripped.partition(":")
            for part in re.split(r"[,;]+", rest):
                r = _normalize_reason(part)
                if r and r not in _BENIGN_REASONS:
                    reasons.append(r)
            continue
        if ":" not in stripped and " " not in stripped:
            r = _normalize_reason(stripped)
            if r and r not in _BENIGN_REASONS:
                reasons.append(r)
            continue
        if "reason" in stripped.lower() and ":" in stripped:
            _, _, rest = stripped.partition(":")
            for part in re.split(r"[,;]+", rest):
                r = _normalize_reason(part)
                if r and r not in _BENIGN_REASONS:
                    reasons.append(r)

    reasons = _dedupe_reasons(reasons)

    base = "unknown"
    if " is offline" in low or low.endswith(" offline.") or " offline " in low:
        base = "offline"
    elif "disabled" in low:
        base = "stopped"
    elif "now printing" in low or " is printing" in low or "printing" in low:
        base = "printing"
    elif " is idle" in low or low.endswith(" idle."):
        base = "idle"
    elif "stopped" in low:
        base = "stopped"
    elif first and "enable" in low:
        base = "idle"

    return _status_from_state_and_reasons(base, reasons), reasons


def cups_queue_status(
    queue: str, *, device_uri: str | None = None
) -> dict[str, object]:
    """Live CUPS/IPP status for one queue: status, status_reasons, status_message.

    Prefers IPP Get-Printer-Attributes (via ``ipptool``) so media-empty / jam
    surface correctly. ``lpstat -p`` only reports idle/printing and is fallback.
    """
    ipp = _ipp_query_queue(queue, device_uri=device_uri)
    if ipp is not None:
        return ipp

    out = _run(["lpstat", "-p", queue, "-l"], 5)
    status, reasons = _parse_lpstat_printer_block(out, queue)
    return {
        "status": status,
        "status_reasons": reasons,
        "status_message": _human_status_message(reasons),
    }


def inventory_payload() -> list[dict[str, object]]:
    """CUPS network printer inventory for heartbeat / report_printers.

    Each item includes:
      cups_name, uri, display_name, status,
      status_reasons (list), status_message (str|None)
    """
    items: list[dict[str, object]] = []
    for queue, uri in configured_network_queues():
        st = cups_queue_status(queue, device_uri=uri)
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
