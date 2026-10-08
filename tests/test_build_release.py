"""scripts/build-release.sh: one-shot build+sign, the split CI modes, and the
packaged binary always being the one this run built.

The script runs from a throwaway copy of a tiny repo with a fake ``cargo``
(``metadata`` reports the target dir as cargo would; ``zigbuild`` records its
environment and writes a stand-in binary that prints ``vesyl-print <version>``)
and a fake ``qemu-aarch64``, and signs with a throwaway Ed25519 key.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import update as update_mod

SCRIPT = ROOT / "scripts" / "build-release.sh"
LAB_KEY = Path("/tmp/vesyl-print-update_private.pem")
VERSION = "0.0.1"
TARBALL_NAME = f"vesyl-print-{VERSION}-linux-aarch64.tar.gz"
MANIFEST_NAME = f"vesyl-print-{VERSION}.manifest.json"

FAKE_CARGO = r"""#!/bin/sh
case "$1" in
  metadata)
    td="${CARGO_TARGET_DIR:-$PWD/target}"
    case "$td" in /*) ;; *) td="$PWD/$td" ;; esac
    printf '{"packages": [], "target_directory": "%s"}\n' "$td"
    ;;
  zigbuild)
    env > "$FAKE_CARGO_ENV"
    [ -n "${FAKE_CARGO_NO_OUTPUT:-}" ] && exit 0
    out="${CARGO_TARGET_DIR:-$PWD/target}/aarch64-unknown-linux-gnu/release"
    mkdir -p "$out"
    printf '#!/bin/sh\necho "vesyl-print %s"\n' \
      "${FAKE_CARGO_VERSION:-$VESYL_PRINT_VERSION}" > "$out/vesyl-print"
    chmod 755 "$out/vesyl-print"
    ;;
  *)
    echo "fake cargo: unexpected: $*" >&2
    exit 99
    ;;
esac
"""

# qemu-aarch64 -L <sysroot> <binary> [args]: run the (shell script) binary.
FAKE_QEMU = """#!/bin/sh
[ "$1" = "-L" ] || exit 98
shift 2
exec "$@"
"""

# The only programs SIGN_ONLY may run (besides bash builtins).
SIGN_TOOLS = ("dirname", "mkdir", "mktemp", "rm", "mv", "sha256sum", "base64", "tr", "openssl")
# Build tooling that must never run next to the signing key.
TRIPWIRES = ("cargo", "cargo-zigbuild", "zig", "rsync", "tar", "gzip", "install", "pip", "pip3", "uname")


def _write_exe(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _openssl(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["openssl", *args], capture_output=True, text=True, check=False)


def _has_crypto() -> bool:
    try:
        import cryptography  # noqa: F401

        return True
    except ImportError:
        return False


def _tools_present() -> bool:
    return all(shutil.which(t) for t in ("bash", "rsync", "tar", "openssl", *SIGN_TOOLS))


@unittest.skipUnless(_tools_present(), "needs bash, rsync, tar, openssl and coreutils")
class BuildReleaseCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._keys = tempfile.TemporaryDirectory()
        keys = Path(cls._keys.name)
        cls.priv, cls.pub = keys / "priv.pem", keys / "pub.pem"
        other_priv, cls.other_pub = keys / "other.pem", keys / "other_pub.pem"
        for priv, pub in ((cls.priv, cls.pub), (other_priv, cls.other_pub)):
            gen = _openssl("genpkey", "-algorithm", "Ed25519", "-out", str(priv))
            if gen.returncode != 0:
                raise unittest.SkipTest(f"openssl cannot make Ed25519 keys: {gen.stderr}")
            _openssl("pkey", "-in", str(priv), "-pubout", "-out", str(pub)).check_returncode()

    @classmethod
    def tearDownClass(cls):
        cls._keys.cleanup()

    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.tmp = Path(td.name)
        self.repo = self.tmp / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.repo / "scripts" / "build-release.sh")
        (self.repo / "VERSION").write_text(f"{VERSION}\n", encoding="utf-8")
        (self.repo / "agent.py").write_text("# agent\n", encoding="utf-8")
        (self.repo / "rust").mkdir()
        (self.repo / "keys").mkdir()
        shutil.copy2(self.pub, self.repo / "keys" / "update_public.pem")
        self.out = self.tmp / "dist"
        self.tarball = self.out / TARBALL_NAME
        self.manifest_path = self.out / MANIFEST_NAME
        self.fakebin = self.tmp / "fakebin"
        self.fakebin.mkdir()
        _write_exe(self.fakebin / "cargo", FAKE_CARGO)
        _write_exe(self.fakebin / "cargo-zigbuild", "#!/bin/sh\nexit 0\n")
        _write_exe(self.fakebin / "qemu-aarch64", FAKE_QEMU)
        self.sysroot = self.tmp / "sysroot"
        (self.sysroot / "lib").mkdir(parents=True)
        (self.sysroot / "lib" / "ld-linux-aarch64.so.1").write_bytes(b"")
        self.cargo_env = self.tmp / "cargo.env"

    # -- running the script ---------------------------------------------------

    def env(self, path: str, extra: dict[str, str]) -> dict[str, str]:
        # Minimal on purpose: nothing from the caller's shell (tokens, a real
        # UPDATE_PRIVATE_KEY, CARGO_TARGET_DIR, ...) reaches the script.
        env = {
            "PATH": path,
            "HOME": os.environ.get("HOME", str(self.tmp)),
            "OUT_DIR": str(self.out),
            "AARCH64_SYSROOT": str(self.sysroot),
            "FAKE_CARGO_ENV": str(self.cargo_env),
            "TMPDIR": str(self.tmp),
        }
        env.update(extra)
        return env

    def run_script(self, *args: str, **extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [shutil.which("bash"), str(self.repo / "scripts" / "build-release.sh"), *args],
            env=self.env(f"{self.fakebin}{os.pathsep}{os.environ.get('PATH', '')}", extra),
            capture_output=True,
            text=True,
            timeout=120,
        )

    def run_sign_only(self, *args: str, **extra: str) -> subprocess.CompletedProcess:
        """SIGN_ONLY=1 with PATH holding only the signing tools (and tripwires)."""
        sandbox = self.tmp / "signbin"
        if not sandbox.exists():
            sandbox.mkdir()
            for tool in SIGN_TOOLS:
                os.symlink(shutil.which(tool), sandbox / tool)
            os.symlink(os.path.realpath(sys.executable), sandbox / "python3")
            for tool in TRIPWIRES:
                _write_exe(sandbox / tool, f'#!/bin/sh\necho "{tool} $*" >> "{self.tmp}/tripwire.log"\nexit 97\n')
        extra.setdefault("SIGN_ONLY", "1")
        p = subprocess.run(
            [shutil.which("bash"), str(self.repo / "scripts" / "build-release.sh"), *args],
            env=self.env(str(sandbox), extra),
            capture_output=True,
            text=True,
            timeout=120,
        )
        tripped = self.tmp / "tripwire.log"
        self.assertFalse(tripped.exists(), tripped.read_text() if tripped.exists() else "")
        return p

    # -- inspecting the output ------------------------------------------------

    def members(self) -> list[str]:
        with tarfile.open(self.tarball) as tf:
            return tf.getnames()

    def packaged_binary(self) -> bytes:
        with tarfile.open(self.tarball) as tf:
            f = tf.extractfile(f"vesyl-print-{VERSION}/vesyl-print")
            return f.read()

    def manifest(self) -> dict:
        return json.loads(self.manifest_path.read_text(encoding="utf-8"))

    def assert_signed(self, manifest: dict, public_key: Path) -> None:
        self.assertIn("signature", manifest)
        body = {k: v for k, v in manifest.items() if k != "signature" and v is not None}
        canonical = self.tmp / "canonical.json"
        canonical.write_bytes(json.dumps(body, sort_keys=True, separators=(",", ":")).encode())
        sig = self.tmp / "sig.bin"
        sig.write_bytes(base64.b64decode(manifest["signature"]))
        p = _openssl(
            "pkeyutl", "-verify", "-pubin", "-inkey", str(public_key),
            "-rawin", "-in", str(canonical), "-sigfile", str(sig),
        )
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        if _has_crypto():  # the device-side verifier
            update_mod.verify_manifest(
                update_mod.ReleaseManifest.from_dict(manifest),
                public_key_pem=public_key.read_bytes(),
            )

    def assert_manifest_matches_tarball(self) -> dict:
        m = self.manifest()
        self.assertEqual(m["version"], VERSION)
        self.assertEqual(m["artifact_sha256"], _sha256(self.tarball))
        self.assertEqual(
            m["artifact_url"],
            f"https://github.com/vesylapp/vesyl-print/releases/download/v{VERSION}/{TARBALL_NAME}",
        )
        return m

    def cargo_saw(self, name: str) -> str | None:
        """A variable from the environment the (fake) cargo build ran with."""
        for line in self.cargo_env.read_text(encoding="utf-8").splitlines():
            if line.startswith(f"{name}="):
                return line[len(name) + 1 :]
        return None

    def assert_key_never_reached_cargo(self) -> None:
        # Report variable names / PEM markers only, never values.
        leaked = [
            line.split("=", 1)[0]
            for line in self.cargo_env.read_text(encoding="utf-8").splitlines()
            if "UPDATE_PRIVATE_KEY" in line or "PRIVATE KEY" in line
        ]
        self.assertEqual(leaked, [], "the signing key reached the cargo build")


class TestOneShot(BuildReleaseCase):
    def test_builds_and_signs_in_one_go(self):
        p = self.run_script(VERSION, UPDATE_PRIVATE_KEY=self.priv.read_text())
        self.assertEqual(p.returncode, 0, p.stderr)
        names = self.members()
        for name in ("agent.py", "VERSION", "vesyl-print"):
            self.assertIn(f"vesyl-print-{VERSION}/{name}", names)
        self.assertNotIn(f"vesyl-print-{VERSION}/keys/update_private.pem", names)
        self.assertIn(f"vesyl-print {VERSION}".encode(), self.packaged_binary())
        self.assert_signed(self.assert_manifest_matches_tarball(), self.pub)
        self.assertIn(f"version: vesyl-print {VERSION}", p.stdout)
        self.assertIn("signature verifies against keys/update_public.pem", p.stdout)
        self.assert_key_never_reached_cargo()

    def test_key_file_signs(self):
        p = self.run_script(VERSION, UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assert_signed(self.assert_manifest_matches_tarball(), self.pub)
        self.assert_key_never_reached_cargo()

    def test_version_defaults_to_version_file(self):
        p = self.run_script(UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.manifest()["version"], VERSION)

    def test_missing_key_file_is_an_error(self):
        p = self.run_script(VERSION, UPDATE_PRIVATE_KEY_FILE=str(self.tmp / "nope.pem"))
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("UPDATE_PRIVATE_KEY_FILE not found", p.stderr)
        self.assertFalse(self.cargo_env.exists(), "built before checking the key")
        self.assertFalse(self.manifest_path.exists())

    @unittest.skipIf(LAB_KEY.exists(), f"{LAB_KEY} would be used as the signing key")
    def test_no_key_writes_unsigned_manifest_with_warning(self):
        p = self.run_script(VERSION)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("manifest unsigned", p.stderr)
        self.assertNotIn("signature", self.assert_manifest_matches_tarball())

    def test_lab_key_mismatch_only_warns(self):
        shutil.copy2(self.other_pub, self.repo / "keys" / "update_public.pem")
        p = self.run_script(VERSION, UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("does not verify against keys/update_public.pem", p.stderr)
        self.assert_signed(self.manifest(), self.pub)

    def test_skip_rust_binary(self):
        p = self.run_script(VERSION, SKIP_RUST_BINARY="1", UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertNotIn(f"vesyl-print-{VERSION}/vesyl-print", self.members())
        self.assertFalse(self.cargo_env.exists())


class TestPackagedBinary(BuildReleaseCase):
    """C34: the tarball gets the binary this run built, never an older one."""

    def plant_stale(self, target_dir: Path) -> Path:
        stale = target_dir / "aarch64-unknown-linux-gnu" / "release" / "vesyl-print"
        stale.parent.mkdir(parents=True, exist_ok=True)
        _write_exe(stale, '#!/bin/sh\necho "vesyl-print 0.0.0"\n')
        return stale

    def test_redirected_target_dir_ships_fresh_binary(self):
        self.plant_stale(self.repo / "rust" / "target")
        elsewhere = self.tmp / "elsewhere"
        p = self.run_script(
            VERSION, CARGO_TARGET_DIR=str(elsewhere), UPDATE_PRIVATE_KEY_FILE=str(self.priv)
        )
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(f"vesyl-print {VERSION}".encode(), self.packaged_binary())
        self.assertNotIn(b"vesyl-print 0.0.0", self.packaged_binary())
        self.assertEqual(self.cargo_saw("CARGO_TARGET_DIR"), str(elsewhere))

    def test_previous_artifact_deleted_before_build(self):
        stale = self.plant_stale(self.repo / "rust" / "target")
        p = self.run_script(
            VERSION, FAKE_CARGO_NO_OUTPUT="1", UPDATE_PRIVATE_KEY_FILE=str(self.priv)
        )
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("cargo did not produce", p.stderr)
        self.assertFalse(stale.exists())
        self.assertFalse(self.tarball.exists())
        self.assertFalse(self.manifest_path.exists())

    def test_binary_must_report_release_version(self):
        p = self.run_script(
            VERSION, FAKE_CARGO_VERSION="0.0.0", UPDATE_PRIVATE_KEY_FILE=str(self.priv)
        )
        self.assertNotEqual(p.returncode, 0)
        self.assertIn(f"reports 'vesyl-print 0.0.0', expected 'vesyl-print {VERSION}'", p.stderr)
        self.assertFalse(self.tarball.exists())
        self.assertFalse(self.manifest_path.exists())

    @unittest.skipIf(platform.machine() == "aarch64", "aarch64 hosts run the binary")
    def test_without_qemu_the_version_string_must_be_in_the_binary(self):
        no_sysroot = str(self.tmp / "no-sysroot")
        p = self.run_script(
            VERSION,
            AARCH64_SYSROOT=no_sysroot,
            FAKE_CARGO_VERSION="0.0.0",
            UPDATE_PRIVATE_KEY_FILE=str(self.priv),
        )
        self.assertNotEqual(p.returncode, 0)
        self.assertIn(f"does not contain version {VERSION}", p.stderr)
        p = self.run_script(VERSION, AARCH64_SYSROOT=no_sysroot, UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(f"contains {VERSION} (not run", p.stdout)


class TestSplitModes(BuildReleaseCase):
    """C13: CI builds without secrets, then signs with only openssl/python3/sha256sum."""

    def test_build_only_then_sign_only(self):
        p = self.run_script(VERSION, BUILD_ONLY="1", UPDATE_PRIVATE_KEY=self.priv.read_text())
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("ignoring UPDATE_PRIVATE_KEY", p.stdout)
        self.assertTrue(self.tarball.is_file())
        self.assertFalse(self.manifest_path.exists())
        self.assert_key_never_reached_cargo()
        built_sha = _sha256(self.tarball)

        p = self.run_sign_only(
            VERSION,
            UPDATE_PRIVATE_KEY_FILE=str(self.priv),
            UPDATE_PUBLIC_KEY_FILE=str(self.pub),
        )
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(_sha256(self.tarball), built_sha)
        m = self.assert_manifest_matches_tarball()
        self.assertEqual(m["artifact_sha256"], built_sha)
        self.assert_signed(m, self.pub)
        self.assertIn(f"signature verifies against {self.pub}", p.stdout)

    def test_sign_only_key_from_env(self):
        self.assertEqual(self.run_script(VERSION, BUILD_ONLY="1").returncode, 0)
        p = self.run_sign_only(VERSION, UPDATE_PRIVATE_KEY=self.priv.read_text())
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assert_signed(self.assert_manifest_matches_tarball(), self.pub)

    def test_build_only_removes_stale_manifest(self):
        self.out.mkdir()
        self.manifest_path.write_text('{"version": "0.0.1", "artifact_sha256": "old"}\n')
        p = self.run_script(VERSION, BUILD_ONLY="1")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.manifest_path.exists())

    @unittest.skipIf(LAB_KEY.exists(), f"{LAB_KEY} would be used as the signing key")
    def test_sign_only_needs_a_key(self):
        self.assertEqual(self.run_script(VERSION, BUILD_ONLY="1").returncode, 0)
        p = self.run_sign_only(VERSION)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("SIGN_ONLY=1 needs UPDATE_PRIVATE_KEY", p.stderr)
        self.assertFalse(self.manifest_path.exists())

    def test_sign_only_needs_the_tarball(self):
        p = self.run_sign_only(VERSION, UPDATE_PRIVATE_KEY_FILE=str(self.priv))
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("missing", p.stderr)
        self.assertFalse(self.manifest_path.exists())

    def test_sign_only_refuses_key_that_devices_would_reject(self):
        self.assertEqual(self.run_script(VERSION, BUILD_ONLY="1").returncode, 0)
        self.manifest_path.write_text("{}\n")  # left over from an earlier run
        p = self.run_sign_only(
            VERSION,
            UPDATE_PRIVATE_KEY_FILE=str(self.priv),
            UPDATE_PUBLIC_KEY_FILE=str(self.other_pub),
        )
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("does not verify against", p.stderr)
        self.assertFalse(self.manifest_path.exists())

    def test_modes_are_exclusive(self):
        p = self.run_script(VERSION, BUILD_ONLY="1", SIGN_ONLY="1")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("mutually exclusive", p.stderr)
        self.assertFalse(self.cargo_env.exists())


if __name__ == "__main__":
    unittest.main()
