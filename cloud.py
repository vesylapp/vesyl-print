"""HTTP client for VESYL print/v1 REST API (stdlib urllib)."""

from __future__ import annotations

import json
import logging
import urllib.error
import urllib.request
from typing import Any
from urllib.parse import urljoin

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
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if token:
            headers["Authorization"] = f"Bearer {token}"

        req = urllib.request.Request(
            self._url(path), data=data, headers=headers, method=method
        )
        log.debug("%s %s", method, path)
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
    ) -> dict[str, Any]:
        body: dict[str, Any] = {}
        if agent_version is not None:
            body["agent_version"] = agent_version
        if hostname is not None:
            body["hostname"] = hostname
        if printers is not None:
            body["printers"] = printers
        return self._request(
            "POST", "print/v1/heartbeat", body=body or {}, token=device_token
        )

    def ws_ticket(self, device_token: str) -> dict[str, Any]:
        return self._request("POST", "print/v1/ws_ticket", body={}, token=device_token)
