"""OTA unit tests — manifest verify, atomic install, heartbeat desired version."""

from __future__ import annotations

import base64
import hashlib
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import update as update_mod
from config import Config


def _has_crypto() -> bool:
    try:
        import cryptography  # noqa: F401

        return True
    except ImportError:
        return False


def _make_keypair():
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    priv = Ed25519PrivateKey.generate()
    pub = priv.public_key()
    pub_pem = pub.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )
    return priv, pub_pem


def _sign(priv, message: bytes) -> str:
    return base64.b64encode(priv.sign(message)).decode("ascii")


def _build_release_tree(root: Path, version: str = "0.4.0") -> Path:
    """Create a tiny fake release directory and tar it."""
    src = root / "src"
    src.mkdir(parents=True, exist_ok=True)
    (src / "agent.py").write_text("# fake agent\n", encoding="utf-8")
    (src / "main.py").write_text("# fake main\n", encoding="utf-8")
    (src / "VERSION").write_text(version + "\n", encoding="utf-8")
    tarball = root / f"vesyl-print-{version}.tar.gz"
    with tarfile.open(tarball, "w:gz") as tar:
        tar.add(src, arcname=f"vesyl-print-{version}")
    return tarball


class TestVersion(unittest.TestCase):
    def test_version_cmp(self):
        self.assertEqual(update_mod.version_cmp("0.3.0", "0.3.0"), 0)
        self.assertEqual(update_mod.version_cmp("0.3.0", "0.4.0"), -1)
        self.assertEqual(update_mod.version_cmp("1.0.0", "0.9.9"), 1)

    def test_github_manifest_url(self):
        base = "https://github.com/benwyrosdick/vesyl-print/releases/download"
        self.assertEqual(
            update_mod.default_manifest_url(base, "0.4.0", "stable"),
            f"{base}/v0.4.0/vesyl-print-0.4.0.manifest.json",
        )
        self.assertEqual(
            update_mod.default_artifact_url(base, "0.4.0"),
            f"{base}/v0.4.0/vesyl-print-0.4.0-linux-aarch64.tar.gz",
        )


class TestManifest(unittest.TestCase):
    def test_parse_and_canonical(self):
        data = {
            "version": "0.4.0",
            "channel": "stable",
            "artifact_url": "https://example/a.tar.gz",
            "artifact_sha256": "a" * 64,
            "signature": "ignored-in-canonical",
        }
        m = update_mod.ReleaseManifest.from_dict(data)
        raw = m.canonical_bytes()
        self.assertNotIn(b"signature", raw)
        self.assertIn(b"0.4.0", raw)

    def test_bad_sha_rejected(self):
        with self.assertRaises(update_mod.UpdateError):
            update_mod.ReleaseManifest.from_dict(
                {
                    "version": "0.4.0",
                    "artifact_url": "https://x",
                    "artifact_sha256": "deadbeef",
                }
            )


@unittest.skipUnless(_has_crypto(), "cryptography not installed")
class TestSignature(unittest.TestCase):
    def test_verify_ok_and_bad(self):
        priv, pub_pem = _make_keypair()
        data = {
            "version": "0.4.0",
            "channel": "stable",
            "artifact_url": "https://example/a.tar.gz",
            "artifact_sha256": "b" * 64,
        }
        m = update_mod.ReleaseManifest.from_dict(data)
        sig = _sign(priv, m.canonical_bytes())
        m.signature = sig
        m.raw["signature"] = sig
        update_mod.verify_manifest(m, public_key_pem=pub_pem, require_signature=True)

        m.signature = base64.b64encode(b"\x00" * 64).decode()
        with self.assertRaises(update_mod.UpdateError) as cm:
            update_mod.verify_manifest(m, public_key_pem=pub_pem, require_signature=True)
        self.assertEqual(cm.exception.code, "bad_signature")


class TestInstallSlots(unittest.TestCase):
    def test_extract_flip_rollback(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            tarball = _build_release_tree(td_path, "0.4.0")
            install_root = td_path / "opt"
            release_dir = install_root / "releases" / "0.4.0"
            update_mod.extract_tarball(tarball, release_dir)
            self.assertTrue((release_dir / "agent.py").is_file())
            update_mod.write_version_file(release_dir, "0.4.0")
            update_mod.flip_current(install_root, "0.4.0")
            cur = (install_root / "current").resolve()
            self.assertEqual(cur.name, "0.4.0")

            # second release
            tarball2 = _build_release_tree(td_path / "b", "0.4.1")
            r2 = install_root / "releases" / "0.4.1"
            update_mod.extract_tarball(tarball2, r2)
            update_mod.flip_current(install_root, "0.4.1")
            self.assertEqual((install_root / "current").resolve().name, "0.4.1")

            rolled = update_mod.rollback(install_root)
            self.assertEqual(rolled, "0.4.0")
            self.assertEqual((install_root / "current").resolve().name, "0.4.0")

    def test_path_escape_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            evil = td_path / "evil.tar.gz"
            with tarfile.open(evil, "w:gz") as tar:
                info = tarfile.TarInfo(name="../escape.py")
                data = b"x"
                info.size = len(data)
                import io

                tar.addfile(info, io.BytesIO(data))
            with self.assertRaises(update_mod.UpdateError) as cm:
                update_mod.extract_tarball(evil, td_path / "out")
            self.assertEqual(cm.exception.code, "bad_archive")


class TestHeartbeatDesired(unittest.TestCase):
    def test_idle_when_no_desired(self):
        cfg = Config(
            api_base_url="https://example.test",
            auto_update_enabled=True,
            releases_base_url="https://releases.example/print",
        )
        st = update_mod.maybe_update_from_heartbeat({"ok": True}, cfg=cfg, auto_apply=True)
        self.assertEqual(st.status, "idle")
        self.assertIsNone(st.target_version)

    def test_idle_when_already_current(self):
        cfg = Config(api_base_url="https://example.test", auto_update_enabled=True)
        cur = update_mod.package_version()
        st = update_mod.maybe_update_from_heartbeat(
            {"desired_agent_version": cur}, cfg=cfg, auto_apply=True
        )
        self.assertEqual(st.status, "idle")
        self.assertEqual(st.target_version, cur)

    def test_skips_apply_when_auto_disabled(self):
        cfg = Config(
            api_base_url="https://example.test",
            auto_update_enabled=False,
            releases_base_url="https://releases.example/print",
        )
        st = update_mod.maybe_update_from_heartbeat(
            {"desired_agent_version": "9.9.9"}, cfg=cfg, auto_apply=False
        )
        self.assertEqual(st.target_version, "9.9.9")
        self.assertEqual(st.status, "idle")

    def test_apply_from_heartbeat_with_local_artifact(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            tarball = _build_release_tree(td_path, "0.5.0")
            sha = update_mod.sha256_file(tarball)
            install_root = td_path / "install"
            # file:// URL for download
            url = tarball.resolve().as_uri()
            manifest = update_mod.ReleaseManifest.from_dict(
                {
                    "version": "0.5.0",
                    "channel": "stable",
                    "artifact_url": url,
                    "artifact_sha256": sha,
                }
            )
            cfg = Config(
                api_base_url="https://example.test",
                auto_update_enabled=True,
                update_require_signature=False,
                state_dir=td_path / "state",
            )
            # Force install root via env
            with mock.patch.dict("os.environ", {"VESYL_PRINT_INSTALL_ROOT": str(install_root)}):
                update_mod.apply_release(
                    manifest,
                    install_root=install_root,
                    require_signature=False,
                    restart=False,
                )
            self.assertEqual((install_root / "current").resolve().name, "0.5.0")
            self.assertTrue((install_root / "releases" / "0.5.0" / "agent.py").is_file())


class TestChecksum(unittest.TestCase):
    def test_download_checksum(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            src = td_path / "a.bin"
            payload = b"hello-ota"
            src.write_bytes(payload)
            sha = hashlib.sha256(payload).hexdigest()
            dest = td_path / "out.bin"
            update_mod.http_download_to_file(src.resolve().as_uri(), dest, expected_sha256=sha)
            self.assertEqual(dest.read_bytes(), payload)
            with self.assertRaises(update_mod.UpdateError):
                update_mod.http_download_to_file(
                    src.resolve().as_uri(),
                    td_path / "bad.bin",
                    expected_sha256="0" * 64,
                )


if __name__ == "__main__":
    unittest.main()
