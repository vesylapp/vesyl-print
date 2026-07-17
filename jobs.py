"""Local print jobs: durable queue, content fetch, CUPS submit + completion.

Ordering (at-least-once, crash-safe):

1. If processed/<job_id> exists → already finished (idempotent), skip print
2. If no queue file → write + fsync full job JSON to queue/<job_id>.json
3. Only then call ack callback (when cloud supports it)
4. Materialize content → lp -d <cups_name>
5. Report **delivered** when lp accepts the job
6. Poll CUPS when possible → report **printed** or **error**
7. On success path: write processed/<job_id>, delete queue file

On agent start: drain_queue() recovers queue/*.json left from crashes.

raw_* content types are rejected until a thermal/raw spike is documented.
"""

from __future__ import annotations

import base64
import json
import logging
import os
import re
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Protocol

log = logging.getLogger("vesyl-print.jobs")

# content_type → file suffix for temp materialization
_SUFFIX = {
    "pdf_uri": ".pdf",
    "pdf_base64": ".pdf",
    "png_uri": ".png",
    "png_base64": ".png",
    "jpeg_uri": ".jpg",
    "jpeg_base64": ".jpg",
    "jpg_uri": ".jpg",
    "jpg_base64": ".jpg",
    "local_path": None,  # use source path as-is
}

_SUPPORTED = set(_SUFFIX) | {"local_path"}
_RAW_TYPES = {"raw_uri", "raw_base64"}


class JobError(Exception):
    """Print / queue failure with a short machine-friendly code."""

    def __init__(self, message: str, *, code: str = "job_error"):
        super().__init__(message)
        self.message = message
        self.code = code


@dataclass
class PrintJob:
    id: str
    cups_name: str
    content_type: str
    content: str
    title: str | None = None
    printer_id: str | None = None
    options: dict[str, Any] = field(default_factory=dict)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PrintJob:
        job_id = data.get("id") or data.get("job_id")
        if not job_id:
            raise JobError("job missing id", code="invalid_job")
        cups = data.get("cups_name") or data.get("cups_queue")
        if not cups:
            raise JobError("job missing cups_name", code="invalid_job")
        ctype = data.get("content_type") or data.get("type")
        if not ctype:
            raise JobError("job missing content_type", code="invalid_job")
        content = data.get("content")
        if content is None or content == "":
            raise JobError("job missing content", code="invalid_job")
        opts = data.get("options") if isinstance(data.get("options"), dict) else {}
        return cls(
            id=str(job_id),
            cups_name=str(cups),
            content_type=str(ctype).lower(),
            content=str(content),
            title=(str(data["title"]) if data.get("title") else None),
            printer_id=(str(data["printer_id"]) if data.get("printer_id") else None),
            options=dict(opts),
            raw=dict(data),
        )

    def to_dict(self) -> dict[str, Any]:
        """Serialize for durable queue (prefer original payload if present)."""
        if self.raw:
            out = dict(self.raw)
            out.setdefault("id", self.id)
            out.setdefault("cups_name", self.cups_name)
            out.setdefault("content_type", self.content_type)
            out.setdefault("content", self.content)
            return out
        return {
            "id": self.id,
            "cups_name": self.cups_name,
            "content_type": self.content_type,
            "content": self.content,
            "title": self.title,
            "printer_id": self.printer_id,
            "options": self.options,
        }


# Optional cloud hooks (Phase C). Defaults are no-ops so Phase B is local-only.
AckFn = Callable[[PrintJob], None]
StateFn = Callable[[PrintJob, str, str | None], None]


def noop_ack(_job: PrintJob) -> None:
    return None


def noop_state(_job: PrintJob, _state: str, _detail: str | None = None) -> None:
    return None


# Back-compat aliases
_noop_ack = noop_ack
_noop_state = noop_state


class LpRunner(Protocol):
    def __call__(
        self,
        cups_name: str,
        path: Path,
        *,
        title: str | None,
        copies: int,
    ) -> str | None: ...


_LP_REQUEST_RE = re.compile(
    r"request id is\s+(\S+)", re.IGNORECASE
)


def default_lp(
    cups_name: str,
    path: Path,
    *,
    title: str | None = None,
    copies: int = 1,
) -> str | None:
    """Submit a file to CUPS via `lp`.

    Returns the CUPS request id (e.g. ``Zebra_1-42``) when parseable, else None.
    Raises JobError on submission failure.
    """
    cmd = ["lp", "-d", cups_name]
    if copies and copies > 1:
        cmd.extend(["-n", str(copies)])
    if title:
        cmd.extend(["-t", title])
    cmd.append(str(path))
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=60
        )
    except (OSError, subprocess.SubprocessError) as e:
        raise JobError(f"lp failed: {e}", code="lp_error") from e
    if result.returncode != 0:
        err = (result.stderr or result.stdout or "lp failed").strip()
        raise JobError(err, code="lp_error")
    text = f"{result.stdout or ''}\n{result.stderr or ''}"
    m = _LP_REQUEST_RE.search(text)
    return m.group(1).rstrip("()") if m else None


# Paper-out / jam recovery can take a long time; keep watching CUPS while the
# job remains in not-completed so we still report ``printed`` after refill.
_DEFAULT_CUPS_WAIT_S = 24 * 60 * 60.0  # 24h hard ceiling
_CUPS_WAIT_LOG_EVERY_S = 60.0


def wait_cups_job(
    request_id: str,
    *,
    timeout_s: float = _DEFAULT_CUPS_WAIT_S,
    poll_s: float = 2.0,
    on_tick: Callable[[], None] | None = None,
) -> str:
    """Poll CUPS until the job leaves the active (not-completed) queue.

    Stays in the loop for up to ``timeout_s`` while the job is still active so
    out-of-paper / jam recovery still transitions delivered → printed.

    Returns:
      ``printed`` — job left not-completed without cancel/abort markers
      ``error`` — CUPS reports canceled/aborted (best-effort)
      ``unknown`` — hard timeout, or CUPS queries failed repeatedly
    """
    if not request_id:
        return "unknown"

    deadline = time.monotonic() + max(30.0, timeout_s)
    # Job id for lpstat filters is often "Printer-N" or "Printer-N.1"
    job_key = request_id.split()[0]
    consecutive_query_failures = 0
    last_log = 0.0
    started = time.monotonic()

    while time.monotonic() < deadline:
        try:
            active = subprocess.run(
                ["lpstat", "-W", "not-completed"],
                capture_output=True,
                text=True,
                timeout=15,
            )
            consecutive_query_failures = 0
        except (OSError, subprocess.SubprocessError) as e:
            consecutive_query_failures += 1
            log.debug("lpstat not-completed failed: %s", e)
            if consecutive_query_failures >= 5:
                log.warning(
                    "CUPS lpstat failed %d times for %s — leaving delivered",
                    consecutive_query_failures,
                    job_key,
                )
                return "unknown"
            time.sleep(max(0.5, poll_s))
            continue

        active_out = (active.stdout or "") + (active.stderr or "")
        if job_key not in active_out:
            # Not in active queue — check completed for abort markers if possible
            try:
                done = subprocess.run(
                    ["lpstat", "-W", "completed", "-l"],
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                done_out = (done.stdout or "") + (done.stderr or "")
            except (OSError, subprocess.SubprocessError):
                done_out = ""

            # Find a block mentioning the job and look for failure keywords
            if job_key in done_out:
                # crude: if aborted/canceled near the job id, treat as error
                idx = done_out.find(job_key)
                snippet = done_out[idx : idx + 400].lower()
                if any(k in snippet for k in ("canceled", "cancelled", "aborted")):
                    return "error"
            return "printed"

        now = time.monotonic()
        if now - last_log >= _CUPS_WAIT_LOG_EVERY_S:
            log.info(
                "CUPS job %s still active after %.0fs (waiting for printer)",
                job_key,
                now - started,
            )
            last_log = now

        # Keep admin inventory fresh while this thread is blocked on paper-out.
        if on_tick is not None:
            try:
                on_tick()
            except Exception:
                log.debug("wait_cups_job on_tick failed", exc_info=True)

        time.sleep(max(0.25, poll_s))

    log.warning(
        "CUPS job %s still active after %.0fs — leaving delivered",
        job_key,
        timeout_s,
    )
    return "unknown"


@dataclass
class JobStore:
    """Durable queue + processed markers under state_dir."""

    queue_dir: Path
    processed_dir: Path

    def ensure(self) -> None:
        self.queue_dir.mkdir(parents=True, exist_ok=True)
        self.processed_dir.mkdir(parents=True, exist_ok=True)

    def queue_path(self, job_id: str) -> Path:
        return self.queue_dir / f"{job_id}.json"

    def processed_path(self, job_id: str) -> Path:
        return self.processed_dir / job_id

    def is_processed(self, job_id: str) -> bool:
        return self.processed_path(job_id).is_file()

    def has_queue_file(self, job_id: str) -> bool:
        return self.queue_path(job_id).is_file()

    def write_queue(self, job: PrintJob) -> Path:
        """Write job JSON with fsync. Idempotent if file already exists."""
        self.ensure()
        path = self.queue_path(job.id)
        if path.is_file():
            return path
        raw = json.dumps(job.to_dict(), indent=2) + "\n"
        tmp = path.with_suffix(path.suffix + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(raw)
                f.flush()
                os.fsync(f.fileno())
            os.replace(tmp, path)
            # fsync directory for durability on crash
            try:
                dir_fd = os.open(str(self.queue_dir), os.O_RDONLY)
                try:
                    os.fsync(dir_fd)
                finally:
                    os.close(dir_fd)
            except OSError:
                pass
        except Exception:
            try:
                tmp.unlink(missing_ok=True)
            except OSError:
                pass
            raise
        return path

    def mark_processed(self, job_id: str) -> None:
        self.ensure()
        path = self.processed_path(job_id)
        path.write_text(_utc_marker(), encoding="utf-8")
        try:
            os.chmod(path, 0o644)
        except OSError:
            pass

    def delete_queue(self, job_id: str) -> None:
        try:
            self.queue_path(job_id).unlink(missing_ok=True)
        except OSError:
            pass

    def load_queued(self, job_id: str) -> PrintJob:
        path = self.queue_path(job_id)
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            raise JobError(f"corrupt queue file {path}: {e}", code="corrupt_queue") from e
        if not isinstance(data, dict):
            raise JobError(f"corrupt queue file {path}", code="corrupt_queue")
        return PrintJob.from_dict(data)

    def list_queued_ids(self) -> list[str]:
        self.ensure()
        ids: list[str] = []
        for p in sorted(self.queue_dir.glob("*.json")):
            if p.name.endswith(".tmp"):
                continue
            ids.append(p.stem)
        return ids

    def has_pending_work(self) -> bool:
        """True if any durable queue file exists (job mid-print or crash recovery)."""
        return bool(self.list_queued_ids())


def _utc_marker() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).replace(microsecond=0).isoformat() + "\n"


def sniff_suffix(data: bytes) -> str | None:
    """Detect file type from magic bytes (overrides wrong content_type labels)."""
    if data.startswith(b"%PDF"):
        return ".pdf"
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if len(data) >= 3 and data[:3] == b"\xff\xd8\xff":
        return ".jpg"
    return None


def _write_temp_content(work_dir: Path, job_id: str, declared_suffix: str, data: bytes) -> Path:
    """Write bytes and rename to sniffed type so CUPS filters see the real format."""
    real = sniff_suffix(data)
    suffix = real or declared_suffix or ".bin"
    if real and declared_suffix and real != declared_suffix:
        log.warning(
            "job %s content magic is %s but content_type implied %s — using magic",
            job_id,
            real,
            declared_suffix,
        )
    out = work_dir / f"{job_id}{suffix}"
    out.write_bytes(data)
    return out


def materialize_content(
    job: PrintJob,
    *,
    work_dir: Path | None = None,
    fetch_url: Callable[[str], bytes] | None = None,
) -> tuple[Path, bool]:
    """Return (path, is_temp). Caller must delete temp files when is_temp.

    Supports pdf/png/jpeg uri|base64 and local_path. raw_* raises JobError.
    File extension is corrected from magic bytes when content_type disagrees
    (e.g. pdf_uri that actually returns a PNG label).
    """
    ctype = job.content_type
    if ctype in _RAW_TYPES:
        raise JobError(
            f"content_type {ctype} not supported (raw/thermal spike required)",
            code="unsupported_content",
        )
    if ctype not in _SUPPORTED:
        raise JobError(f"unsupported content_type: {ctype}", code="unsupported_content")

    if ctype == "local_path":
        path = Path(job.content).expanduser()
        if not path.is_file():
            raise JobError(f"local file not found: {path}", code="content_missing")
        return path, False

    if work_dir is None:
        work_dir = Path(tempfile.mkdtemp(prefix="vesyl-print-"))
    else:
        work_dir.mkdir(parents=True, exist_ok=True)

    declared = _SUFFIX.get(ctype) or ".bin"

    if ctype.endswith("_base64"):
        try:
            # tolerate data-url prefix
            payload = job.content
            if "," in payload and payload.strip().lower().startswith("data:"):
                payload = payload.split(",", 1)[1]
            data = base64.b64decode(payload, validate=False)
        except (ValueError, TypeError) as e:
            raise JobError(f"invalid base64 content: {e}", code="content_bad") from e
        if not data:
            raise JobError("empty base64 content", code="content_bad")
        return _write_temp_content(work_dir, job.id, declared, data), True

    # *_uri
    fetcher = fetch_url or _http_get
    try:
        data = fetcher(job.content)
    except Exception as e:
        raise JobError(f"fetch failed: {e}", code="content_fetch") from e
    if not data:
        raise JobError("empty content from uri", code="content_bad")
    return _write_temp_content(work_dir, job.id, declared, data), True


def _http_get(url: str, timeout: float = 60.0) -> bytes:
    req = urllib.request.Request(
        url, headers={"User-Agent": "vesyl-print-agent", "Accept": "*/*"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        body = e.read()[:200] if e.fp else b""
        raise JobError(
            f"HTTP {e.code} fetching content", code="content_fetch"
        ) from e
    except urllib.error.URLError as e:
        raise JobError(f"network error fetching content: {e.reason}", code="content_fetch") from e


def process_job(
    job: PrintJob,
    store: JobStore,
    *,
    lp: LpRunner = default_lp,
    ack: AckFn = noop_ack,
    report_state: StateFn = noop_state,
    fetch_url: Callable[[str], bytes] | None = None,
    work_dir: Path | None = None,
    on_wait_tick: Callable[[], None] | None = None,
) -> str:
    """Run the full durable pipeline for one job.

    Returns final local result: ``printed``, ``delivered`` (no CUPS tracking),
    or raises JobError after reporting error.

    ``on_wait_tick`` is invoked periodically while waiting on CUPS completion
    (e.g. paper-out recovery) so the agent can keep reporting printer inventory.
    """
    store.ensure()
    job_id = job.id

    # 1. Already finished — idempotent success (drop any leftover queue file)
    if store.is_processed(job_id):
        log.info("job %s already processed — skip", job_id)
        store.delete_queue(job_id)
        try:
            report_state(job, "printed", "already_processed")
        except Exception:
            log.debug("report_state failed", exc_info=True)
        return "printed"

    # 2. Durable receive before any ack / print
    store.write_queue(job)

    # 3. Ack only after disk durability
    try:
        ack(job)
    except Exception as e:
        # Ack failure is non-fatal for local print; cloud can redeliver.
        log.warning("ack failed for job %s: %s", job_id, e)

    # 4–6. Materialize + submit + optional CUPS completion wait
    path: Path | None = None
    is_temp = False
    try:
        report_state(job, "printing", None)
    except Exception:
        pass

    try:
        path, is_temp = materialize_content(job, work_dir=work_dir, fetch_url=fetch_url)
        copies = int(job.options.get("copies") or 1)
        if copies < 1:
            copies = 1
        cups_id = lp(job.cups_name, path, title=job.title, copies=copies)
        cups_job = cups_id if isinstance(cups_id, str) and cups_id.strip() else None
        try:
            report_state(job, "delivered", cups_job)
        except Exception:
            log.debug("report_state delivered failed", exc_info=True)

        final = "delivered"
        if cups_job:
            outcome = wait_cups_job(cups_job, on_tick=on_wait_tick)
            if outcome == "printed":
                try:
                    report_state(job, "printed", cups_id)
                except Exception:
                    log.debug("report_state printed failed", exc_info=True)
                final = "printed"
            elif outcome == "error":
                raise JobError(
                    f"CUPS job {cups_id} failed", code="cups_job_failed"
                )
            else:
                log.info(
                    "job %s CUPS tracking timed out for %s — left delivered",
                    job_id,
                    cups_id,
                )
        else:
            log.info("job %s no CUPS request id — left delivered", job_id)

        store.mark_processed(job_id)
        store.delete_queue(job_id)
        log.info("job %s %s → %s", job_id, final, job.cups_name)
        return final
    except JobError as e:
        try:
            report_state(job, "error", e.message)
        except Exception:
            pass
        log.error("job %s error: %s", job_id, e.message)
        raise
    except Exception as e:
        msg = str(e)
        try:
            report_state(job, "error", msg)
        except Exception:
            pass
        log.error("job %s error: %s", job_id, msg)
        raise JobError(msg, code="job_error") from e
    finally:
        if is_temp and path is not None:
            try:
                path.unlink(missing_ok=True)
            except OSError:
                pass
            # clean work_dir if we created a single-file temp dir empty enough
            if work_dir is None and path is not None:
                parent = path.parent
                try:
                    if parent.name.startswith("vesyl-print-"):
                        parent.rmdir()
                except OSError:
                    pass


def drain_queue(
    store: JobStore,
    *,
    lp: LpRunner = default_lp,
    ack: AckFn = noop_ack,
    report_state: StateFn = noop_state,
    fetch_url: Callable[[str], bytes] | None = None,
    on_wait_tick: Callable[[], None] | None = None,
) -> list[tuple[str, str]]:
    """Process every queue/*.json (crash recovery). Returns [(job_id, result)]."""
    store.ensure()
    results: list[tuple[str, str]] = []
    for job_id in store.list_queued_ids():
        try:
            job = store.load_queued(job_id)
        except JobError as e:
            log.error("skip corrupt queue %s: %s", job_id, e.message)
            results.append((job_id, f"error:{e.code}"))
            continue
        try:
            state = process_job(
                job,
                store,
                lp=lp,
                ack=ack,
                report_state=report_state,
                fetch_url=fetch_url,
                on_wait_tick=on_wait_tick,
            )
            results.append((job_id, state))
        except JobError as e:
            results.append((job_id, f"error:{e.code}"))
        except Exception as e:
            results.append((job_id, f"error:{e}"))
    return results


def receive_job(
    job: PrintJob,
    store: JobStore,
    *,
    lp: LpRunner = default_lp,
    ack: AckFn = noop_ack,
    report_state: StateFn = noop_state,
    fetch_url: Callable[[str], bytes] | None = None,
    on_wait_tick: Callable[[], None] | None = None,
) -> str:
    """Entry point for a newly delivered job (pull/push later)."""
    return process_job(
        job,
        store,
        lp=lp,
        ack=ack,
        report_state=report_state,
        fetch_url=fetch_url,
        on_wait_tick=on_wait_tick,
    )


def job_from_local_file(
    path: Path | str,
    cups_name: str,
    *,
    job_id: str | None = None,
    title: str | None = None,
    copies: int = 1,
) -> PrintJob:
    """Build a PrintJob that prints an existing file (CLI print-test)."""
    p = Path(path).expanduser().resolve()
    if not p.is_file():
        raise JobError(f"file not found: {p}", code="content_missing")
    return PrintJob(
        id=job_id or str(uuid.uuid4()),
        cups_name=cups_name,
        content_type="local_path",
        content=str(p),
        title=title or f"vesyl-print test {p.name}",
        options={"copies": copies},
    )


def store_from_config(cfg: Any) -> JobStore:
    """Build JobStore from a Config-like object with queue_dir/processed_dir."""
    return JobStore(queue_dir=Path(cfg.queue_dir), processed_dir=Path(cfg.processed_dir))
