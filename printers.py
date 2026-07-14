"""Discover and provision network printers via CUPS.

The app shows the current network printer, and — if none is configured —
auto-adds the first one it discovers on the network (driverless / IPP
Everywhere), naming the queue after the printer model. Requires membership
in the 'lpadmin' group to add printers (no sudo needed).
"""

from __future__ import annotations

import re
import subprocess

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


# --- currently configured printer ------------------------------------------

def configured_printer() -> str | None:
    """Display name (model/description) of the first configured network
    queue, or None if there isn't one."""
    queue = _first_network_queue()
    return _display_name(queue) if queue else None


def _first_network_queue() -> str | None:
    """Name of the first CUPS queue with a network device URI, or None.

    Parses `lpstat -v` lines of the form "device for <name>: <uri>".
    """
    out = _run(["lpstat", "-v"], 3)
    prefix = "device for "
    for line in out.splitlines():
        if not line.startswith(prefix):
            continue
        name, _, uri = line[len(prefix):].partition(":")
        if _is_network_uri(uri.strip()):
            return name.strip()
    return None


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

def discover_network_printer() -> tuple[str, str] | None:
    """(device_uri, make_and_model) of the first discoverable network printer.

    Uses `lpinfo -l -v`, which browses the network (mDNS) and can take a few
    seconds. Skips backend placeholders (uri = "ipp", "socket", …) and
    devices with an unknown model.
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

    for d in devices:
        uri = d.get("uri", "")
        model = d.get("make-and-model", "")
        if (
            d.get("class") == "network"
            and _is_network_uri(uri)
            and model
            and model.lower() != "unknown"
        ):
            return uri, model
    return None


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


def ensure_printer() -> str | None:
    """Display name of a network printer, provisioning one if none exists.

    Returns the model/description to show, or None if nothing is configured
    and nothing could be discovered. May block for several seconds while
    browsing the network, so call this off the render loop.
    """
    existing = configured_printer()
    if existing:
        return existing

    found = discover_network_printer()
    if not found:
        return None

    uri, model = found
    queue = add_printer(uri, model)
    return _display_name(queue) if queue else None
