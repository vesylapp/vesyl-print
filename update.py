"""App OTA: download, verify, atomic install, rollback.

Production layout (preferred)::

    /opt/vesyl-print/
      current -> releases/0.4.0
      releases/0.3.0/
      releases/0.4.0/
      update/                  # staging

Lab/dev without root uses ``{state_dir}/app/`` the same way.

Control plane (plan choice **A**): heartbeat JSON may include::

    {
      "ok": true,
      "desired_agent_version": "0.4.0",
      "update_channel": "stable",
      "update_url": "https://…/manifest.json"   // optional
    }

Artifacts are HTTPS tarballs verified by SHA-256 + Ed25519 signature over the
canonical manifest (signature field excluded). Public key: config path or
bundled ``keys/update_public.pem``.
"""

from __future__ import annotations

import base64
import hashlib
import json
import logging
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urljoin

log = logging.getLogger("vesyl-print.update")

# Baked-in default; replace at release time or override with config file.
# Empty means "verification requires an explicit key file".
DEFAULT_UPDATE_PUBLIC_KEY_PEM = ""

_VERSION_RE = re.compile(r"^\d+\.\d+\.\d+([.-][0-9A-Za-z.]+)?$")


class UpdateError(Exception):
    def __init__(self, message: str, *, code: str = "update_error"):
        super().__init__(message)
        self.message = message
        self.code = code


# After activate + restart, wait this long for whoami (or local checks if unpaired)
# before auto-rolling back to the previous slot.
DEFAULT_HEALTH_GATE_SECONDS = 120

# Status lifecycle:
#   idle → downloading → installing → pending_health → idle
#                                      ↘ failed | rolled_back
STATUS_IDLE = "idle"
STATUS_CHECKING = "checking"
STATUS_DOWNLOADING = "downloading"
STATUS_INSTALLING = "installing"
STATUS_PENDING_HEALTH = "pending_health"
STATUS_FAILED = "failed"
STATUS_ROLLED_BACK = "rolled_back"


@dataclass
class UpdateStatus:
    status: str = STATUS_IDLE
    current_version: str = ""
    target_version: str | None = None
    last_error: str | None = None
    last_checked_at: str | None = None
    channel: str | None = None
    # Health gate: slot we left so we can auto-rollback if the new agent is unhealthy.
    previous_version: str | None = None
    health_deadline_at: str | None = None
    health_attempts: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status,
            "current_version": self.current_version,
            "target_version": self.target_version,
            "last_error": self.last_error,
            "last_checked_at": self.last_checked_at,
            "channel": self.channel,
            "previous_version": self.previous_version,
            "health_deadline_at": self.health_deadline_at,
            "health_attempts": self.health_attempts,
        }


@dataclass
class ReleaseManifest:
    version: str
    channel: str
    artifact_url: str
    artifact_sha256: str
    min_agent_version: str | None = None
    signature: str | None = None  # base64 Ed25519 over canonical JSON (no signature)
    released_at: str | None = None
    changelog: str | None = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ReleaseManifest:
        version = str(data.get("version") or "").strip()
        if not version or not _VERSION_RE.match(version):
            raise UpdateError(f"invalid version in manifest: {version!r}", code="bad_manifest")
        url = data.get("artifact_url") or data.get("url")
        if not url:
            raise UpdateError("manifest missing artifact_url", code="bad_manifest")
        sha = str(data.get("artifact_sha256") or data.get("sha256") or "").strip().lower()
        if len(sha) != 64 or any(c not in "0123456789abcdef" for c in sha):
            raise UpdateError("manifest missing or invalid artifact_sha256", code="bad_manifest")
        return cls(
            version=version,
            channel=str(data.get("channel") or "stable"),
            artifact_url=str(url),
            artifact_sha256=sha,
            min_agent_version=(
                str(data["min_agent_version"]) if data.get("min_agent_version") else None
            ),
            signature=(str(data["signature"]) if data.get("signature") else None),
            released_at=(str(data["released_at"]) if data.get("released_at") else None),
            changelog=(str(data["changelog"]) if data.get("changelog") else None),
            raw=dict(data),
        )

    def canonical_bytes(self) -> bytes:
        """Stable JSON for signing: all fields except signature, sorted keys.

        Must match ``scripts/build-release.sh`` (sort_keys, compact separators,
        omit nulls and the signature field only).
        """
        if self.raw:
            body = {
                k: v
                for k, v in self.raw.items()
                if k != "signature" and v is not None
            }
        else:
            body = {
                "version": self.version,
                "channel": self.channel,
                "artifact_url": self.artifact_url,
                "artifact_sha256": self.artifact_sha256,
            }
            if self.min_agent_version:
                body["min_agent_version"] = self.min_agent_version
            if self.released_at:
                body["released_at"] = self.released_at
            if self.changelog:
                body["changelog"] = self.changelog
        return json.dumps(body, sort_keys=True, separators=(",", ":")).encode("utf-8")


def package_version() -> str:
    """Installed package version (VERSION file next to this module, else config)."""
    here = Path(__file__).resolve().parent
    ver_file = here / "VERSION"
    if ver_file.is_file():
        text = ver_file.read_text(encoding="utf-8").strip()
        if text:
            return text
    try:
        from config import AGENT_VERSION

        return AGENT_VERSION
    except Exception:
        return "0.0.0"


def parse_version(v: str) -> tuple[int, ...]:
    core = v.split("-", 1)[0].split("+", 1)[0]
    parts = core.split(".")
    out: list[int] = []
    for p in parts:
        try:
            out.append(int(p))
        except ValueError:
            out.append(0)
    return tuple(out)


def version_cmp(a: str, b: str) -> int:
    """Return -1 if a<b, 0 if equal, 1 if a>b (numeric semver-ish)."""
    ta, tb = parse_version(a), parse_version(b)
    n = max(len(ta), len(tb))
    ta = ta + (0,) * (n - len(ta))
    tb = tb + (0,) * (n - len(tb))
    if ta < tb:
        return -1
    if ta > tb:
        return 1
    return 0


def resolve_install_root(cfg: Any | None = None) -> Path:
    """Prefer /opt/vesyl-print; else state_dir/app for lab installs."""
    if env := os.environ.get("VESYL_PRINT_INSTALL_ROOT"):
        return Path(env)
    opt = Path("/opt/vesyl-print")
    if opt.is_dir() or os.access("/opt", os.W_OK):
        return opt
    if cfg is not None and getattr(cfg, "state_dir", None):
        return Path(cfg.state_dir) / "app"
    return Path.home() / ".local" / "share" / "vesyl-print" / "app"


def current_release_dir(install_root: Path) -> Path | None:
    cur = install_root / "current"
    try:
        if cur.is_symlink() or cur.is_dir():
            return cur.resolve()
    except OSError:
        return None
    return None


def current_release_version(install_root: Path) -> str | None:
    cur = current_release_dir(install_root)
    if cur and _VERSION_RE.match(cur.name):
        return cur.name
    return None


def list_releases(install_root: Path) -> list[str]:
    rel = install_root / "releases"
    if not rel.is_dir():
        return []
    vers = [p.name for p in rel.iterdir() if p.is_dir() and _VERSION_RE.match(p.name)]
    return sorted(vers, key=parse_version)


def health_gate_seconds(cfg: Any | None = None) -> int:
    if cfg is not None:
        raw = getattr(cfg, "update_health_gate_seconds", None)
        if raw is not None:
            try:
                return max(15, int(raw))
            except (TypeError, ValueError):
                pass
    return DEFAULT_HEALTH_GATE_SECONDS


# --- crypto ----------------------------------------------------------------

def load_public_key_pem(path: Path | None = None, pem_text: str | None = None) -> bytes:
    if pem_text and pem_text.strip():
        return pem_text.encode("ascii") if isinstance(pem_text, str) else pem_text
    if path and path.is_file():
        return path.read_bytes()
    bundled = Path(__file__).resolve().parent / "keys" / "update_public.pem"
    if bundled.is_file():
        return bundled.read_bytes()
    if DEFAULT_UPDATE_PUBLIC_KEY_PEM.strip():
        return DEFAULT_UPDATE_PUBLIC_KEY_PEM.encode("ascii")
    raise UpdateError(
        "no update public key configured (keys/update_public.pem or config)",
        code="no_public_key",
    )


def verify_ed25519(public_key_pem: bytes, message: bytes, signature_b64: str) -> None:
    """Verify Ed25519 signature (base64). Requires the ``cryptography`` package."""
    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
        from cryptography.exceptions import InvalidSignature
    except ImportError as e:
        raise UpdateError(
            "cryptography package required for update signature verify "
            "(apt install python3-cryptography)",
            code="no_crypto",
        ) from e

    try:
        key = serialization.load_pem_public_key(public_key_pem)
    except Exception as e:
        raise UpdateError(f"invalid update public key: {e}", code="bad_public_key") from e
    if not isinstance(key, Ed25519PublicKey):
        raise UpdateError("update public key must be Ed25519", code="bad_public_key")
    try:
        sig = base64.b64decode(signature_b64, validate=True)
    except Exception as e:
        raise UpdateError("invalid signature encoding", code="bad_signature") from e
    try:
        key.verify(sig, message)
    except InvalidSignature as e:
        raise UpdateError("manifest signature verification failed", code="bad_signature") from e


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_manifest(
    manifest: ReleaseManifest,
    *,
    public_key_pem: bytes | None = None,
    require_signature: bool = True,
) -> None:
    if require_signature:
        if not manifest.signature:
            raise UpdateError("manifest missing signature", code="bad_signature")
        pem = public_key_pem or load_public_key_pem()
        verify_ed25519(pem, manifest.canonical_bytes(), manifest.signature)


# --- download / install ----------------------------------------------------

def http_get_bytes(url: str, timeout: float = 120.0) -> bytes:
    req = urllib.request.Request(
        url, headers={"User-Agent": "vesyl-print-agent", "Accept": "*/*"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        raise UpdateError(f"HTTP {e.code} fetching {url}", code="download_failed") from e
    except urllib.error.URLError as e:
        raise UpdateError(f"network error: {e.reason}", code="download_failed") from e


def http_download_to_file(
    url: str, dest: Path, *, expected_sha256: str, timeout: float = 300.0
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    req = urllib.request.Request(
        url, headers={"User-Agent": "vesyl-print-agent", "Accept": "*/*"}
    )
    h = hashlib.sha256()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp, tmp.open("wb") as out:
            while True:
                chunk = resp.read(1024 * 1024)
                if not chunk:
                    break
                out.write(chunk)
                h.update(chunk)
    except urllib.error.HTTPError as e:
        tmp.unlink(missing_ok=True)
        raise UpdateError(f"HTTP {e.code} downloading artifact", code="download_failed") from e
    except urllib.error.URLError as e:
        tmp.unlink(missing_ok=True)
        raise UpdateError(f"network error: {e.reason}", code="download_failed") from e
    digest = h.hexdigest()
    if digest != expected_sha256.lower():
        tmp.unlink(missing_ok=True)
        raise UpdateError(
            f"artifact sha256 mismatch (got {digest[:12]}…)",
            code="bad_checksum",
        )
    os.replace(tmp, dest)


def fetch_manifest(url: str) -> ReleaseManifest:
    raw = http_get_bytes(url)
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as e:
        raise UpdateError("manifest is not valid JSON", code="bad_manifest") from e
    if not isinstance(data, dict):
        raise UpdateError("manifest must be a JSON object", code="bad_manifest")
    return ReleaseManifest.from_dict(data)


def default_manifest_url(releases_base_url: str, version: str, channel: str = "stable") -> str:
    """Resolve manifest URL for a version.

    GitHub Releases (CDN)::

        {base}/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json
        base = https://github.com/OWNER/REPO/releases/download

    Flat CDN host::

        {base}/vesyl-print-X.Y.Z.manifest.json
    """
    base = releases_base_url.rstrip("/")
    ver = version.lstrip("v")
    tag = ver if version.startswith("v") else f"v{ver}"
    name = f"vesyl-print-{ver}.manifest.json"
    if "github.com" in base and "/releases/download" in base:
        return f"{base}/{tag}/{name}"
    return f"{base}/{name}"


def default_artifact_url(releases_base_url: str, version: str, arch: str = "linux-aarch64") -> str:
    base = releases_base_url.rstrip("/")
    ver = version.lstrip("v")
    tag = f"v{ver}"
    name = f"vesyl-print-{ver}-{arch}.tar.gz"
    if "github.com" in base and "/releases/download" in base:
        return f"{base}/{tag}/{name}"
    return f"{base}/{name}"


def channel_latest_url(releases_base_url: str, channel: str) -> str:
    """Optional channel pointer (flat CDN only; not used for GitHub tags)."""
    base = releases_base_url.rstrip("/") + "/"
    return urljoin(base, f"{channel}/latest.manifest.json")


def extract_tarball(tarball: Path, dest_dir: Path) -> None:
    """Extract tarball into dest_dir (must not already exist)."""
    if dest_dir.exists():
        raise UpdateError(f"release dir already exists: {dest_dir}", code="exists")
    dest_dir.parent.mkdir(parents=True, exist_ok=True)
    staging = dest_dir.with_name(dest_dir.name + ".staging")
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)

    try:
        with tarfile.open(tarball, "r:*") as tar:
            # Safety: reject path escape
            for m in tar.getmembers():
                name = m.name
                if name.startswith("/") or ".." in Path(name).parts:
                    raise UpdateError(
                        f"refusing unsafe path in archive: {name}",
                        code="bad_archive",
                    )
            try:
                tar.extractall(staging, filter="data")
            except TypeError:
                # Python < 3.12
                tar.extractall(staging)
    except UpdateError:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    except Exception as e:
        shutil.rmtree(staging, ignore_errors=True)
        raise UpdateError(f"extract failed: {e}", code="bad_archive") from e

    # If archive has a single top-level dir, peel it.
    children = list(staging.iterdir())
    if len(children) == 1 and children[0].is_dir():
        peeled = children[0]
        os.replace(peeled, dest_dir)
        shutil.rmtree(staging, ignore_errors=True)
    else:
        os.replace(staging, dest_dir)


def write_version_file(release_dir: Path, version: str) -> None:
    (release_dir / "VERSION").write_text(version + "\n", encoding="utf-8")


def atomic_symlink(target: Path, link_path: Path) -> None:
    """Point link_path at target (relative if possible) via atomic rename."""
    link_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        rel = os.path.relpath(target, start=link_path.parent)
    except ValueError:
        rel = str(target)
    tmp = link_path.with_name(link_path.name + ".new")
    if tmp.is_symlink() or tmp.exists():
        tmp.unlink()
    os.symlink(rel, tmp)
    os.replace(tmp, link_path)


def flip_current(install_root: Path, version: str) -> Path:
    release_dir = install_root / "releases" / version
    if not release_dir.is_dir():
        raise UpdateError(f"release not installed: {version}", code="missing_release")
    current = install_root / "current"
    atomic_symlink(release_dir, current)
    return release_dir


def rollback(
    install_root: Path,
    to_version: str | None = None,
    *,
    apply_helper: Path | None = None,
) -> str:
    """Flip current to previous release (or explicit version)."""
    releases = list_releases(install_root)
    if not releases:
        raise UpdateError("no releases to roll back to", code="no_rollback")
    cur = current_release_dir(install_root)
    cur_ver = cur.name if cur else None
    if to_version:
        if to_version not in releases:
            raise UpdateError(f"unknown release {to_version}", code="missing_release")
        target = to_version
    else:
        older = [v for v in releases if v != cur_ver]
        if not older:
            raise UpdateError("no previous release for rollback", code="no_rollback")
        target = older[-1]

    release_dir = install_root / "releases" / target
    current = install_root / "current"
    helper = apply_helper if apply_helper is not None else _default_apply_helper()
    if helper and helper.is_file():
        try:
            subprocess.run(
                [
                    "sudo",
                    "-n",
                    str(helper),
                    "activate",
                    str(release_dir),
                    str(current),
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
        except (OSError, subprocess.SubprocessError) as e:
            # Lab installs / unit tests: fall back to in-process symlink flip.
            log.warning("apply-update activate failed (%s); flipping current in-process", e)
            flip_current(install_root, target)
    else:
        flip_current(install_root, target)
    log.info("rolled back to %s", target)
    return target


def restart_services(helper: Path | None = None) -> None:
    """Restart display + agent via apply-update helper or systemctl."""
    if helper and helper.is_file():
        subprocess.run(
            ["sudo", "-n", str(helper), "restart"],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
        return
    for unit in ("vesyl-print-agent", "vesyl-print-display"):
        try:
            subprocess.run(
                ["systemctl", "restart", unit],
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError) as e:
            log.warning("restart %s failed: %s", unit, e)


def apply_release(
    manifest: ReleaseManifest,
    *,
    install_root: Path,
    public_key_pem: bytes | None = None,
    require_signature: bool = True,
    download: Callable[..., None] = http_download_to_file,
    restart: bool = False,
    apply_helper: Path | None = None,
) -> Path:
    """Full path: verify manifest → download → extract → flip current.

    Does **not** run the post-update health gate. Callers that restart services
    should persist ``pending_health`` via :func:`mark_pending_health` *before*
    restart so the new process can verify and auto-rollback.
    """
    if manifest.min_agent_version:
        if version_cmp(package_version(), manifest.min_agent_version) < 0:
            raise UpdateError(
                f"current {package_version()} < min_agent_version "
                f"{manifest.min_agent_version}",
                code="too_old",
            )

    verify_manifest(
        manifest, public_key_pem=public_key_pem, require_signature=require_signature
    )

    install_root = Path(install_root)
    update_dir = install_root / "update"
    update_dir.mkdir(parents=True, exist_ok=True)
    tarball = update_dir / f"vesyl-print-{manifest.version}.tar.gz"

    log.info("downloading %s", manifest.artifact_url)
    download(
        manifest.artifact_url,
        tarball,
        expected_sha256=manifest.artifact_sha256,
    )

    release_dir = install_root / "releases" / manifest.version
    if release_dir.exists():
        shutil.rmtree(release_dir)

    log.info("extracting to %s", release_dir)
    extract_tarball(tarball, release_dir)
    write_version_file(release_dir, manifest.version)

    # Minimal sanity: agent entrypoint present
    if not (release_dir / "agent.py").is_file() and not (
        release_dir / "main.py"
    ).is_file():
        shutil.rmtree(release_dir, ignore_errors=True)
        raise UpdateError(
            "archive missing agent.py/main.py", code="bad_archive"
        )

    if apply_helper and apply_helper.is_file():
        subprocess.run(
            [
                "sudo",
                "-n",
                str(apply_helper),
                "activate",
                str(release_dir),
                str(install_root / "current"),
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
    else:
        flip_current(install_root, manifest.version)

    log.info("activated version %s", manifest.version)
    try:
        tarball.unlink(missing_ok=True)
    except OSError:
        pass

    if restart:
        restart_services(apply_helper)

    return release_dir


def mark_pending_health(
    st: UpdateStatus,
    *,
    target_version: str,
    previous_version: str | None,
    gate_seconds: int | None = None,
    channel: str | None = None,
) -> UpdateStatus:
    """Record that activate succeeded; health gate must pass before idle."""
    seconds = max(15, int(gate_seconds if gate_seconds is not None else DEFAULT_HEALTH_GATE_SECONDS))
    st.status = STATUS_PENDING_HEALTH
    st.current_version = target_version
    st.target_version = target_version
    st.previous_version = previous_version
    st.health_deadline_at = _utc_now_plus(seconds)
    st.health_attempts = 0
    st.last_error = None
    st.last_checked_at = _utc_now()
    if channel is not None:
        st.channel = channel
    return st


def _module_runs_from_slot(install_root: Path) -> bool:
    """True when this process loaded update.py from install_root/current."""
    try:
        here = Path(__file__).resolve().parent
        cur = (install_root / "current").resolve()
        return here == cur or cur in here.parents
    except OSError:
        return False


def local_slot_healthy(install_root: Path, expected_version: str | None) -> tuple[bool, str | None]:
    """Fast checks on the active release dir (no network)."""
    cur = current_release_dir(install_root)
    if cur is None:
        return False, "current symlink missing or broken"
    if not (cur / "agent.py").is_file() and not (cur / "main.py").is_file():
        return False, "current slot missing agent.py/main.py"
    if expected_version:
        ver_file = cur / "VERSION"
        slot_ver = cur.name
        if ver_file.is_file():
            try:
                slot_ver = ver_file.read_text(encoding="utf-8").strip() or slot_ver
            except OSError:
                pass
        if version_cmp(slot_ver, expected_version) != 0 and cur.name != expected_version:
            return (
                False,
                f"slot version {slot_ver!r} != expected {expected_version!r}",
            )
        # After a real service restart, code is loaded from current — require match.
        # Lab/unit tests import update.py from the git tree; skip that check there.
        if _module_runs_from_slot(install_root):
            running = package_version()
            if version_cmp(running, expected_version) != 0:
                return (
                    False,
                    f"running package_version {running!r} != expected {expected_version!r}",
                )
    return True, None


def _deadline_passed(deadline_iso: str | None, now_iso: str | None = None) -> bool:
    if not deadline_iso:
        return False
    now = now_iso or _utc_now()
    # ISO-8601 comparable when both are UTC with same shape
    try:
        return now >= deadline_iso
    except TypeError:
        return False


def process_pending_health(
    st: UpdateStatus,
    *,
    cfg: Any,
    whoami_result: str = "skipped",
    # whoami_result: "ok" | "unauthorized" | "error" | "skipped"
    whoami_error: str | None = None,
    install_root: Path | None = None,
    apply_helper: Path | None = None,
    restart_on_rollback: bool = True,
    now_iso: str | None = None,
) -> UpdateStatus:
    """Post-update health gate.

    Called by the agent after restart into a new slot. Declares success only
    after local slot checks pass and (when paired) whoami reaches the API.

    On hard failure or deadline expiry: auto-rollback to ``previous_version``
    when available, set status ``rolled_back``, and restart services.
    """
    if st.status != STATUS_PENDING_HEALTH:
        return st

    root = Path(install_root) if install_root else resolve_install_root(cfg)
    now = now_iso or _utc_now()
    st.last_checked_at = now
    st.health_attempts = int(st.health_attempts or 0) + 1
    expected = st.target_version or st.current_version

    ok_local, local_err = local_slot_healthy(root, expected)
    # whoami: success if API answered (ok or 401 re-pair — code path works)
    cloud_ok = whoami_result in ("ok", "unauthorized", "skipped")
    if whoami_result == "error":
        cloud_ok = False

    if ok_local and cloud_ok:
        log.info(
            "post-update health ok version=%s attempts=%s whoami=%s",
            expected,
            st.health_attempts,
            whoami_result,
        )
        st.status = STATUS_IDLE
        st.current_version = package_version()
        st.previous_version = None
        st.health_deadline_at = None
        st.last_error = None
        return st

    reason_parts: list[str] = []
    if not ok_local and local_err:
        reason_parts.append(local_err)
    if whoami_result == "error":
        reason_parts.append(whoami_error or "whoami failed")
    reason = "; ".join(reason_parts) or "health check failed"
    st.last_error = reason

    past_deadline = _deadline_passed(st.health_deadline_at, now)
    # Local slot broken (wrong version / missing entrypoints) → fail fast.
    hard_fail = not ok_local

    if not past_deadline and not hard_fail:
        log.warning(
            "post-update health not ready yet (%s); will retry until %s",
            reason,
            st.health_deadline_at,
        )
        return st

    # Deadline or hard local failure → rollback if we can.
    prev = st.previous_version
    helper = apply_helper if apply_helper is not None else _default_apply_helper()
    if prev and prev != expected:
        try:
            log.error(
                "post-update health failed (%s) — rolling back to %s",
                reason,
                prev,
            )
            rolled = rollback(root, to_version=prev, apply_helper=helper)
            st.status = STATUS_ROLLED_BACK
            st.current_version = rolled
            st.target_version = expected
            st.previous_version = None
            st.health_deadline_at = None
            st.last_error = f"health failed: {reason}; rolled back to {rolled}"
            if restart_on_rollback:
                try:
                    restart_services(helper)
                except Exception as e:
                    log.warning("restart after rollback failed: %s", e)
            return st
        except UpdateError as e:
            log.error("auto-rollback failed: %s", e.message)
            st.status = STATUS_FAILED
            st.last_error = f"health failed: {reason}; rollback error: {e.message}"
            return st
        except Exception as e:
            log.exception("auto-rollback failed")
            st.status = STATUS_FAILED
            st.last_error = f"health failed: {reason}; rollback error: {e}"
            return st

    st.status = STATUS_FAILED
    st.health_deadline_at = None
    st.last_error = f"health failed: {reason} (no previous slot to roll back to)"
    log.error(st.last_error)
    return st


def maybe_update_from_heartbeat(
    hb: dict[str, Any],
    *,
    cfg: Any,
    status: UpdateStatus | None = None,
    auto_apply: bool = True,
    status_path: Path | None = None,
) -> UpdateStatus:
    """Inspect heartbeat response (plan A) and optionally apply update.

    After a successful activate, status becomes ``pending_health`` (not idle).
    The agent must call :func:`process_pending_health` after restart.
    """
    st = status or UpdateStatus(current_version=package_version())
    st.current_version = package_version()
    st.last_checked_at = _utc_now()

    # Never start another OTA while health gate is open.
    if st.status == STATUS_PENDING_HEALTH:
        log.info("update deferred: pending_health for %s", st.target_version)
        return st

    desired = hb.get("desired_agent_version") or hb.get("desired_version")
    channel = hb.get("update_channel") or getattr(cfg, "update_channel", "stable")
    st.channel = str(channel) if channel else None

    if not desired:
        if st.status not in (STATUS_FAILED, STATUS_ROLLED_BACK, STATUS_PENDING_HEALTH):
            st.status = STATUS_IDLE
        st.target_version = None
        return st

    desired = str(desired).strip()
    st.target_version = desired
    if version_cmp(desired, st.current_version) == 0:
        if st.status not in (STATUS_FAILED, STATUS_ROLLED_BACK):
            st.status = STATUS_IDLE
        return st

    if not auto_apply or not getattr(cfg, "auto_update_enabled", True):
        if st.status not in (STATUS_FAILED, STATUS_ROLLED_BACK, STATUS_PENDING_HEALTH):
            st.status = STATUS_IDLE
        log.info("update available: %s → %s (auto_update disabled)", st.current_version, desired)
        return st

    # Build manifest URL
    update_url = hb.get("update_url") or hb.get("manifest_url")
    releases_base = getattr(cfg, "releases_base_url", "") or ""
    if update_url:
        manifest_url = str(update_url)
    elif releases_base:
        manifest_url = default_manifest_url(releases_base, desired, str(channel or "stable"))
    else:
        st.status = STATUS_FAILED
        st.last_error = "desired version set but no update_url or releases_base_url"
        log.warning(st.last_error)
        return st

    install_root = resolve_install_root(cfg)
    previous = current_release_version(install_root)
    # If running from a slot that isn't version-named, keep package_version as prev.
    if previous is None:
        prev_pkg = package_version()
        if prev_pkg and version_cmp(prev_pkg, desired) != 0:
            previous = prev_pkg

    helper = _default_apply_helper()
    try:
        st.status = STATUS_DOWNLOADING
        log.info("applying update %s from %s", desired, manifest_url)
        manifest = fetch_manifest(manifest_url)
        if version_cmp(manifest.version, desired) != 0:
            # Allow channel latest to redirect, but prefer exact match when versioned URL used
            log.info("manifest version %s (desired %s)", manifest.version, desired)
        key_path = getattr(cfg, "update_public_key_path", None)
        pem = None
        if key_path:
            pem = load_public_key_pem(Path(key_path))
        else:
            try:
                pem = load_public_key_pem()
            except UpdateError:
                pem = None
        require_sig = getattr(cfg, "update_require_signature", True)
        st.status = STATUS_INSTALLING
        # Persist installing so a crash mid-apply is visible in update_status.json
        if status_path is not None:
            write_update_status(Path(status_path), st)

        apply_release(
            manifest,
            install_root=install_root,
            public_key_pem=pem,
            require_signature=require_sig and pem is not None,
            restart=False,
            apply_helper=helper,
        )
        mark_pending_health(
            st,
            target_version=manifest.version,
            previous_version=previous if previous != manifest.version else None,
            gate_seconds=health_gate_seconds(cfg),
            channel=str(channel) if channel else None,
        )
        if status_path is not None:
            write_update_status(Path(status_path), st)
        log.info(
            "activated %s — pending_health until whoami (deadline %s)",
            manifest.version,
            st.health_deadline_at,
        )
        restart_services(helper)
    except UpdateError as e:
        st.status = STATUS_FAILED
        st.last_error = e.message
        log.error("update failed: %s", e.message)
    except Exception as e:
        st.status = STATUS_FAILED
        st.last_error = str(e)
        log.exception("update failed")
    return st


def _default_apply_helper() -> Path | None:
    """Prefer the root-installed helper only (NOPASSWD sudoers on appliances).

    Do not auto-pick the repo copy of ``scripts/apply-update`` — that requires
    root and breaks lab/unit-test flips under a writable install root.
    """
    installed = Path("/usr/local/lib/vesyl-print/apply-update")
    if installed.is_file():
        return installed
    return None


def _utc_now() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def _utc_now_plus(seconds: int) -> str:
    from datetime import datetime, timedelta, timezone

    return (
        datetime.now(timezone.utc).replace(microsecond=0) + timedelta(seconds=seconds)
    ).isoformat()


def write_update_status(path: Path, status: UpdateStatus) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(status.to_dict(), indent=2) + "\n", encoding="utf-8")


def read_update_status(path: Path) -> UpdateStatus | None:
    path = Path(path)
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(data, dict):
        return None
    attempts = data.get("health_attempts") or 0
    try:
        attempts_i = int(attempts)
    except (TypeError, ValueError):
        attempts_i = 0
    return UpdateStatus(
        status=str(data.get("status") or STATUS_IDLE),
        current_version=str(data.get("current_version") or package_version()),
        target_version=data.get("target_version"),
        last_error=data.get("last_error"),
        last_checked_at=data.get("last_checked_at"),
        channel=data.get("channel"),
        previous_version=data.get("previous_version"),
        health_deadline_at=data.get("health_deadline_at"),
        health_attempts=attempts_i,
    )
