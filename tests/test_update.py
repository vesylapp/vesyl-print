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
        base = "https://github.com/vesylapp/vesyl-print/releases/download"
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


class TestHealthGate(unittest.TestCase):
    def _two_slots(self, td_path: Path) -> Path:
        install_root = td_path / "opt"
        for ver in ("0.3.0", "0.4.0"):
            tarball = _build_release_tree(td_path / ver, ver)
            release_dir = install_root / "releases" / ver
            update_mod.extract_tarball(tarball, release_dir)
            update_mod.write_version_file(release_dir, ver)
        update_mod.flip_current(install_root, "0.4.0")
        return install_root

    def test_pending_health_success_whoami_ok(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            cfg = Config(
                api_base_url="https://example.test",
                state_dir=td_path / "state",
                update_health_gate_seconds=60,
            )
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_PENDING_HEALTH,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                health_deadline_at=update_mod._utc_now_plus(60),
            )
            out = update_mod.process_pending_health(
                st,
                cfg=cfg,
                whoami_result="ok",
                install_root=install_root,
                restart_on_rollback=False,
            )
            self.assertEqual(out.status, update_mod.STATUS_IDLE)
            self.assertIsNone(out.previous_version)
            self.assertIsNone(out.health_deadline_at)
            self.assertEqual((install_root / "current").resolve().name, "0.4.0")

    def test_pending_health_retries_before_deadline(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            cfg = Config(api_base_url="https://example.test", state_dir=td_path / "state")
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_PENDING_HEALTH,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                health_deadline_at=update_mod._utc_now_plus(120),
            )
            out = update_mod.process_pending_health(
                st,
                cfg=cfg,
                whoami_result="error",
                whoami_error="connection refused",
                install_root=install_root,
                restart_on_rollback=False,
            )
            self.assertEqual(out.status, update_mod.STATUS_PENDING_HEALTH)
            self.assertEqual(out.health_attempts, 1)
            self.assertEqual((install_root / "current").resolve().name, "0.4.0")

    def test_pending_health_rollback_after_deadline(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            cfg = Config(api_base_url="https://example.test", state_dir=td_path / "state")
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_PENDING_HEALTH,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                health_deadline_at="2000-01-01T00:00:00+00:00",  # long past
            )
            with mock.patch.object(update_mod, "restart_services"):
                out = update_mod.process_pending_health(
                    st,
                    cfg=cfg,
                    whoami_result="error",
                    whoami_error="timeout",
                    install_root=install_root,
                    restart_on_rollback=True,
                )
            self.assertEqual(out.status, update_mod.STATUS_ROLLED_BACK)
            self.assertEqual((install_root / "current").resolve().name, "0.3.0")
            self.assertIn("rolled back", out.last_error or "")

    def test_hard_local_fail_rolls_back_immediately(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            # Break the active slot entrypoints
            cur = (install_root / "current").resolve()
            (cur / "agent.py").unlink()
            (cur / "main.py").unlink()
            cfg = Config(api_base_url="https://example.test", state_dir=td_path / "state")
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_PENDING_HEALTH,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                health_deadline_at=update_mod._utc_now_plus(120),
            )
            out = update_mod.process_pending_health(
                st,
                cfg=cfg,
                whoami_result="ok",
                install_root=install_root,
                restart_on_rollback=False,
            )
            self.assertEqual(out.status, update_mod.STATUS_ROLLED_BACK)
            self.assertEqual((install_root / "current").resolve().name, "0.3.0")

    def test_unpaired_skipped_whoami_succeeds_local(self):
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            cfg = Config(api_base_url="https://example.test", state_dir=td_path / "state")
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_PENDING_HEALTH,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                health_deadline_at=update_mod._utc_now_plus(60),
            )
            out = update_mod.process_pending_health(
                st,
                cfg=cfg,
                whoami_result="skipped",
                install_root=install_root,
                restart_on_rollback=False,
            )
            self.assertEqual(out.status, update_mod.STATUS_IDLE)

    def test_maybe_update_defers_while_pending_health(self):
        cfg = Config(
            api_base_url="https://example.test",
            auto_update_enabled=True,
            releases_base_url="https://releases.example/print",
        )
        st = update_mod.UpdateStatus(
            status=update_mod.STATUS_PENDING_HEALTH,
            current_version="0.4.0",
            target_version="0.4.0",
            previous_version="0.3.0",
        )
        out = update_mod.maybe_update_from_heartbeat(
            {"desired_agent_version": "9.9.9"},
            cfg=cfg,
            status=st,
            auto_apply=True,
        )
        self.assertEqual(out.status, update_mod.STATUS_PENDING_HEALTH)
        self.assertEqual(out.target_version, "0.4.0")

    def test_mark_pending_health_fields(self):
        st = update_mod.UpdateStatus()
        update_mod.mark_pending_health(
            st,
            target_version="0.5.0",
            previous_version="0.4.0",
            gate_seconds=90,
            channel="stable",
        )
        self.assertEqual(st.status, update_mod.STATUS_PENDING_HEALTH)
        self.assertEqual(st.previous_version, "0.4.0")
        self.assertEqual(st.target_version, "0.5.0")
        self.assertIsNotNone(st.health_deadline_at)
        self.assertEqual(st.channel, "stable")

    def test_recover_false_failed_then_health_ok(self):
        """Sticky failed after self-restart SIGTERM → recover → idle on whoami."""
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            install_root = self._two_slots(td_path)
            cfg = Config(
                api_base_url="https://example.test",
                state_dir=td_path / "state",
            )
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_FAILED,
                current_version="0.4.0",
                target_version="0.4.0",
                previous_version="0.3.0",
                last_error="died with <Signals.SIGTERM: 15>",
            )
            with mock.patch.object(update_mod, "package_version", return_value="0.4.0"):
                out = update_mod.process_pending_health(
                    st,
                    cfg=cfg,
                    whoami_result="ok",
                    install_root=install_root,
                    restart_on_rollback=False,
                )
            self.assertEqual(out.status, update_mod.STATUS_IDLE)
            self.assertIsNone(out.last_error)
            self.assertEqual((install_root / "current").resolve().name, "0.4.0")

    def test_restart_services_detaches(self):
        with mock.patch("subprocess.Popen") as popen:
            helper = Path("/usr/local/lib/vesyl-print/apply-update")
            with mock.patch.object(Path, "is_file", return_value=True):
                update_mod.restart_services(helper)
            popen.assert_called_once()
            args, kwargs = popen.call_args
            self.assertEqual(args[0][0:3], ["sudo", "-n", str(helper)])
            self.assertTrue(kwargs.get("start_new_session"))


class TestJobPauseAndDefer(unittest.TestCase):
    def test_should_pause_jobs_statuses(self):
        self.assertFalse(update_mod.should_pause_jobs(None))
        self.assertFalse(
            update_mod.should_pause_jobs(
                update_mod.UpdateStatus(status=update_mod.STATUS_IDLE)
            )
        )
        self.assertFalse(
            update_mod.should_pause_jobs(
                update_mod.UpdateStatus(status=update_mod.STATUS_FAILED)
            )
        )
        for s in (
            update_mod.STATUS_DOWNLOADING,
            update_mod.STATUS_INSTALLING,
            update_mod.STATUS_PENDING_HEALTH,
        ):
            self.assertTrue(
                update_mod.should_pause_jobs(update_mod.UpdateStatus(status=s)),
                msg=s,
            )

    def test_should_pause_jobs_from_path(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "update_status.json"
            self.assertFalse(update_mod.should_pause_jobs_from_path(path))
            st = update_mod.UpdateStatus(
                status=update_mod.STATUS_INSTALLING,
                current_version="0.3.0",
                target_version="0.4.0",
            )
            update_mod.write_update_status(path, st)
            self.assertTrue(update_mod.should_pause_jobs_from_path(path))

    def test_maybe_update_defers_when_jobs_busy(self):
        cfg = Config(
            api_base_url="https://example.test",
            auto_update_enabled=True,
            releases_base_url="https://releases.example/print",
        )
        with mock.patch.object(update_mod, "fetch_manifest") as fetch:
            out = update_mod.maybe_update_from_heartbeat(
                {"desired_agent_version": "9.9.9"},
                cfg=cfg,
                auto_apply=True,
                jobs_busy=True,
            )
            fetch.assert_not_called()
        self.assertEqual(out.target_version, "9.9.9")
        self.assertEqual(out.status, update_mod.STATUS_IDLE)
        self.assertNotEqual(out.status, update_mod.STATUS_DOWNLOADING)


if __name__ == "__main__":
    unittest.main()
