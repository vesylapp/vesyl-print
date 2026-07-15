"""Phase A unit tests — mocked HTTP, no network."""

from __future__ import annotations

import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Repo root on path
import sys

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import auth
import cloud
import config as config_mod
import statusio
from agent import run_once
from config import Config, load_config


CLAIM_RESPONSE = {
    "node_id": "node-uuid-1",
    "device_token": "secret-device-token-do-not-log",
    "name": "Pack station 1",
    "hostname": "vesyl-print-01",
    "status": "offline",
    "warehouse": {
        "id": "wh-1",
        "name": "Main Warehouse",
        "code": "MAIN",
    },
    "organization": {
        "id": "org-1",
        "name": "Acme Corp",
        "slug": "acme",
    },
}


class TestCredentials(unittest.TestCase):
    def test_parse_claim_response(self):
        creds = auth.credentials_from_pair_response(CLAIM_RESPONSE)
        self.assertEqual(creds.node_id, "node-uuid-1")
        self.assertEqual(creds.device_token, "secret-device-token-do-not-log")
        self.assertEqual(creds.organization_name, "Acme Corp")
        self.assertEqual(creds.warehouse_name, "Main Warehouse")
        self.assertEqual(creds.warehouse_code, "MAIN")
        pub = creds.public_dict()
        self.assertNotIn("device_token", pub)

    def test_save_mode_0600(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "credentials.json"
            creds = auth.credentials_from_pair_response(CLAIM_RESPONSE)
            auth.save_credentials(path, creds)
            mode = auth.credentials_mode(path)
            self.assertEqual(mode, 0o600)
            loaded = auth.load_credentials(path)
            assert loaded is not None
            self.assertEqual(loaded.device_token, creds.device_token)
            self.assertEqual(loaded.organization_name, "Acme Corp")

    def test_clear_credentials(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "credentials.json"
            auth.save_credentials(
                path, auth.credentials_from_pair_response(CLAIM_RESPONSE)
            )
            self.assertTrue(auth.clear_credentials(path))
            self.assertIsNone(auth.load_credentials(path))
            self.assertFalse(auth.clear_credentials(path))


class TestConfig(unittest.TestCase):
    def test_env_api_url_override(self):
        with tempfile.TemporaryDirectory() as td:
            cdir = Path(td) / "cfg"
            sdir = Path(td) / "state"
            cdir.mkdir()
            sdir.mkdir()
            (cdir / "config.json").write_text(
                json.dumps(
                    {
                        "api_base_url": "https://file.example/api",
                        "heartbeat_seconds": 15,
                    }
                ),
                encoding="utf-8",
            )
            with mock.patch.dict(
                os.environ,
                {
                    "VESYL_PRINT_API_URL": "https://wms.api.staging.vesyl.com",
                    "VESYL_PRINT_CONFIG_DIR": str(cdir),
                    "VESYL_PRINT_STATE_DIR": str(sdir),
                },
                clear=False,
            ):
                cfg = load_config()
            self.assertEqual(
                cfg.api_base_url, "https://wms.api.staging.vesyl.com"
            )
            self.assertEqual(cfg.heartbeat_seconds, 15)
            self.assertTrue(cfg.cable_url.startswith("wss://"))

    def test_direct_api_default_cable(self):
        cfg = Config(api_base_url="https://wms.api.vesyl.com", cable_url="")
        self.assertEqual(
            cfg.cable_url, "wss://wms.api.vesyl.com/print/cable"
        )

    def test_edge_api_prefix_cable(self):
        cfg = Config(
            api_base_url="https://wms.staging.vesyl.com/api", cable_url=""
        )
        self.assertEqual(
            cfg.cable_url, "wss://wms.staging.vesyl.com/print/cable"
        )


class TestCloudClient(unittest.TestCase):
    def test_claim_parses_201(self):
        client = cloud.CloudClient("https://example.test")
        raw = json.dumps(CLAIM_RESPONSE).encode()

        class Resp:
            status = 201

            def read(self):
                return raw

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        with mock.patch("urllib.request.urlopen", return_value=Resp()):
            data = client.claim(
                "AB7K2Q9M",
                hostname="h",
                agent_version="0.3.0",
                platform="linux-arm64",
            )
        self.assertEqual(data["device_token"], CLAIM_RESPONSE["device_token"])
        self.assertEqual(data["node_id"], "node-uuid-1")

    def test_401_raises_unauthorized(self):
        client = cloud.CloudClient("https://example.test")
        err_body = json.dumps(
            {"error": {"code": "unauthorized", "message": "bad token"}}
        ).encode()

        def boom(*a, **k):
            fp = __import__("io").BytesIO(err_body)
            raise cloud.urllib.error.HTTPError(
                "https://example.test/print/v1/whoami",
                401,
                "Unauthorized",
                hdrs=None,
                fp=fp,
            )

        with mock.patch("urllib.request.urlopen", side_effect=boom):
            with self.assertRaises(cloud.CloudError) as cm:
                client.whoami("bad-token")
        self.assertTrue(cm.exception.unauthorized)
        self.assertEqual(cm.exception.code, "unauthorized")

    def test_request_does_not_embed_token_in_exception_message(self):
        client = cloud.CloudClient("https://example.test")
        secret = "super-secret-token-xyz"

        def boom(*a, **k):
            fp = __import__("io").BytesIO(
                b'{"error":{"code":"unauthorized","message":"invalid"}}'
            )
            raise cloud.urllib.error.HTTPError(
                "https://example.test/print/v1/heartbeat",
                401,
                "Unauthorized",
                hdrs=None,
                fp=fp,
            )

        with mock.patch("urllib.request.urlopen", side_effect=boom):
            with self.assertRaises(cloud.CloudError) as cm:
                client.heartbeat(secret)
        self.assertNotIn(secret, str(cm.exception))
        self.assertNotIn(secret, cm.exception.message)


class TestAgent(unittest.TestCase):
    def _cfg(self, td: str) -> Config:
        cdir = Path(td) / "cfg"
        sdir = Path(td) / "state"
        cdir.mkdir()
        sdir.mkdir()
        return Config(
            api_base_url="https://example.test",
            config_dir=cdir,
            state_dir=sdir,
            heartbeat_seconds=30,
        )

    def test_unpaired_writes_status(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            st = run_once(cfg, client=mock.Mock())
            self.assertEqual(st.pairing, "unpaired")
            loaded = statusio.read_status(cfg.status_path)
            assert loaded is not None
            self.assertEqual(loaded.pairing, "unpaired")

    def test_heartbeat_online(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            creds = auth.credentials_from_pair_response(CLAIM_RESPONSE)
            auth.save_credentials(cfg.credentials_path, creds)
            client = mock.Mock()
            client.whoami.return_value = {
                "node_id": creds.node_id,
                "name": creds.name,
                "organization": {"name": "Acme Corp"},
                "warehouse": {"name": "Main Warehouse"},
            }
            client.heartbeat.return_value = {
                "ok": True,
                "node_id": creds.node_id,
                "status": "online",
                "last_seen_at": "2026-07-15T12:00:00Z",
            }
            with mock.patch("printers.inventory_payload", return_value=[]):
                st = run_once(cfg, client=client)
            self.assertEqual(st.pairing, "paired")
            self.assertEqual(st.cloud, "online")
            self.assertEqual(st.organization_name, "Acme Corp")
            client.heartbeat.assert_called_once()

    def test_401_clears_credentials_and_marks_revoked(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            creds = auth.credentials_from_pair_response(CLAIM_RESPONSE)
            auth.save_credentials(cfg.credentials_path, creds)
            client = mock.Mock()
            client.whoami.side_effect = cloud.CloudError(
                "invalid", status=401, code="unauthorized"
            )
            st = run_once(cfg, client=client)
            self.assertEqual(st.pairing, "revoked")
            self.assertIsNone(auth.load_credentials(cfg.credentials_path))
            loaded = statusio.read_status(cfg.status_path)
            assert loaded is not None
            self.assertEqual(loaded.pairing, "revoked")


class TestStatusIO(unittest.TestCase):
    def test_roundtrip(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "status.json"
            statusio.write_status(
                path,
                statusio.AgentStatus(
                    pairing="paired",
                    cloud="online",
                    organization_name="Acme",
                ),
            )
            st = statusio.read_status(path)
            assert st is not None
            self.assertEqual(st.pairing, "paired")
            self.assertEqual(st.organization_name, "Acme")
            self.assertIsNotNone(st.updated_at)


if __name__ == "__main__":
    unittest.main()
