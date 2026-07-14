"""Collect basic system information for display."""

from __future__ import annotations

import socket
import subprocess
from datetime import datetime


def hostname() -> str:
    return socket.gethostname()


def ip_addresses() -> list[str]:
    """Return non-loopback IPv4 addresses currently assigned to the host."""
    try:
        out = subprocess.run(
            ["hostname", "-I"], capture_output=True, text=True, timeout=2
        ).stdout
    except (OSError, subprocess.SubprocessError):
        out = ""

    addrs = []
    for token in out.split():
        # keep IPv4 only (skip IPv6 which contains ':')
        if ":" not in token and token != "127.0.0.1":
            addrs.append(token)
    return addrs


def primary_ip() -> str:
    addrs = ip_addresses()
    return addrs[0] if addrs else "no network"


def now() -> datetime:
    return datetime.now()
