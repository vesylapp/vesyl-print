"""HTTP client for VESYL print/v1 REST API (stdlib urllib)."""

from __future__ import annotations

import json
import logging
import urllib.error
import urllib.request
from typing import Any
from urllib.parse import quote, urljoin

log = logging.getLogger("vesyl-print.cloud")


class CloudError(Exception):
    """API or transport error. status is HTTP code or 0 for network failure."""

    def __init__(
        self,
        message: str,
        *,
        status: int = 0,
        code: str | None = None,
        body: Any = None,
    ):
        super().__init__(message)
        self.message = message
        self.status = status
        self.code = code
        self.body = body

    @property
    def unauthorized(self) -> bool:
        return self.status == 401

    @property
    def service_disabled(self) -> bool:
        return self.status == 503

    @property
    def not_found(self) -> bool:
        return self.status == 404


def _parse_error_body(raw: bytes, status: int) -> CloudError:
    code = None
    message = f"HTTP {status}"
    body: Any = None
    try:
        body = json.loads(raw.decode("utf-8"))
        if isinstance(body, dict):
            err = body.get("error")
            if isinstance(err, dict):
                code = err.get("code")
                message = str(err.get("message") or message)
            elif isinstance(err, str):
                message = err
            elif body.get("message"):
                message = str(body["message"])
    except (UnicodeDecodeError, json.JSONDecodeError):
        if raw:
            message = raw.decode("utf-8", errors="replace")[:200]
    return CloudError(message, status=status, code=code, body=body)


class CloudClient:
    """Thin REST client. Callers must never log Authorization headers or tokens."""

    def __init__(self, api_base_url: str, timeout: float = 30.0):
        self.api_base_url = api_base_url.rstrip("/") + "/"
        self.timeout = timeout

    def _url(self, path: str) -> str:
        path = path.lstrip("/")
        return urljoin(self.api_base_url, path)

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: dict[str, Any] | None = None,
        token: str | None = None,
    ) -> dict[str, Any]:
        data = None
        headers = {
            "Accept": "application/json",
            "User-Agent": "vesyl-print-agent",
        }
        # Always send a body for POST/PUT/PATCH so urllib never downgrades to GET
        # (a GET on /heartbeat yields Rails RoutingError "Not Found").
        method_u = method.upper()
        if body is not None or method_u in ("POST", "PUT", "PATCH"):
            data = json.dumps(body if body is not None else {}).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if token:
            headers["Authorization"] = f"Bearer {token}"

        req = urllib.request.Request(
            self._url(path), data=data, headers=headers, method=method_u
        )
        log.debug("%s %s", method_u, path)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
                status = getattr(resp, "status", 200) or 200
        except urllib.error.HTTPError as e:
            raw = e.read() if e.fp else b""
            raise _parse_error_body(raw, e.code) from None
        except urllib.error.URLError as e:
            raise CloudError(f"network error: {e.reason}", status=0) from e
        except TimeoutError as e:
            raise CloudError("request timed out", status=0) from e

        if not raw:
            return {}
        try:
            parsed = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            raise CloudError(
                f"invalid JSON response (HTTP {status})", status=status
            ) from e
        if not isinstance(parsed, dict):
            raise CloudError(
                f"expected JSON object (HTTP {status})", status=status, body=parsed
            )
        return parsed

    def claim(
        self,
        code: str,
        *,
        hostname: str,
        agent_version: str,
        platform: str,
        name: str | None = None,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "code": code,
            "hostname": hostname,
            "agent_version": agent_version,
            "platform": platform,
        }
        if name:
            payload["name"] = name
        return self._request("POST", "print/v1/claim", body=payload)

    def enroll(
        self,
        enrollment_token: str,
        *,
        hostname: str,
        agent_version: str,
        platform: str | None = None,
        name: str | None = None,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "enrollment_token": enrollment_token,
            "hostname": hostname,
            "agent_version": agent_version,
        }
        if platform:
            payload["platform"] = platform
        if name:
            payload["name"] = name
        return self._request("POST", "print/v1/enroll", body=payload)

    def whoami(self, device_token: str) -> dict[str, Any]:
        return self._request("GET", "print/v1/whoami", token=device_token)

    def heartbeat(
        self,
        device_token: str,
        *,
        agent_version: str | None = None,
        hostname: str | None = None,
        printers: list[Any] | None = None,
        platform: str | None = None,
        update: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """POST /print/v1/heartbeat.

        Response may include OTA control fields (plan A)::

            desired_agent_version, update_url
        """
        body: dict[str, Any] = {}
        if agent_version is not None:
            body["agent_version"] = agent_version
        if hostname is not None:
            body["hostname"] = hostname
        if printers is not None:
            body["printers"] = printers
        if platform is not None:
            body["platform"] = platform
        if update is not None:
            body["update"] = update
        return self._request(
            "POST", "print/v1/heartbeat", body=body or {}, token=device_token
        )

    def ws_ticket(self, device_token: str) -> dict[str, Any]:
        return self._request("POST", "print/v1/ws_ticket", body={}, token=device_token)

    # --- jobs (pull path; production until ActionCable push) ----------------

    def pending_jobs(self, device_token: str) -> list[dict[str, Any]]:
        """GET /print/v1/jobs/pending — eligible jobs, marked sent server-side.

        Returns a list of job payloads (JobSerializer.for_node shape).
        """
        data = self._request("GET", "print/v1/jobs/pending", token=device_token)
        jobs = data.get("jobs")
        if jobs is None:
            return []
        if not isinstance(jobs, list):
            raise CloudError("jobs/pending: expected jobs array", status=200, body=data)
        out: list[dict[str, Any]] = []
        for item in jobs:
            if isinstance(item, dict):
                out.append(item)
        return out

    def ack_job(self, device_token: str, job_id: str) -> dict[str, Any]:
        """POST /print/v1/jobs/:id/ack — durable receive ACK (after queue fsync)."""
        path = f"print/v1/jobs/{quote(str(job_id), safe='')}/ack"
        return self._request("POST", path, body={}, token=device_token)

    def report_job_state(
        self,
        device_token: str,
        job_id: str,
        state: str,
        *,
        message: str | None = None,
    ) -> dict[str, Any]:
        """POST /print/v1/jobs/:id/state — agent may only send done|error."""
        path = f"print/v1/jobs/{quote(str(job_id), safe='')}/state"
        body: dict[str, Any] = {"state": state}
        if message is not None:
            body["message"] = message
        return self._request("POST", path, body=body, token=device_token)
