#!/usr/bin/env bash
#
# Build a signed vesyl-print OTA release (tarball + manifest).
#
# Usage:
#   ./scripts/build-release.sh
#   ./scripts/build-release.sh 0.4.0
#   UPDATE_PRIVATE_KEY_FILE=/path/to/key.pem ./scripts/build-release.sh
#
# By default this builds the tarball and signs its manifest in one go. CI runs
# the two halves in separate jobs, so the signing key never shares a runner
# with cargo build scripts, proc-macros or the zig toolchain:
#   BUILD_ONLY=1  build and package the tarball only. Writes no manifest and
#                 never reads a key (one left in the environment is dropped).
#   SIGN_ONLY=1   sign the tarball already in $OUT_DIR: hash it and write its
#                 signed manifest. Runs only sha256sum, python3 (stdlib) and
#                 openssl: no cargo, rsync, tar or third-party tools. Needs a key.
#
# Env:
#   UPDATE_PRIVATE_KEY       PEM private key contents (CI secret)
#   UPDATE_PRIVATE_KEY_FILE  Path to PEM private key
#   UPDATE_PUBLIC_KEY_FILE   Public key the signature must verify against (CI:
#                            keys/update_public.pem). Unset: a signature that
#                            does not match keys/update_public.pem only warns
#                            (lab keys).
#   GITHUB_REPOSITORY        owner/repo (default: vesylapp/vesyl-print)
#   RELEASE_CHANNEL          stable|beta (default: stable)
#   OUT_DIR                  output directory (default: dist)
#   SKIP_RUST_BINARY=1       Python-only tarball (no vesyl-print binary)
#   AARCH64_SYSROOT          aarch64 glibc root for qemu-aarch64
#                            (default: /usr/aarch64-linux-gnu)
#
# The Rust agent/CLI binary is cross-compiled for aarch64 (glibc >= 2.31) with
# cargo-zigbuild and placed at the tarball root as ./vesyl-print. agent.py and
# cli.py hand off to it when present, so existing systemd units keep working.
# It is packaged from cargo's own target directory (CARGO_TARGET_DIR and
# build.target-dir are honoured) after any previous build there is deleted,
# and it must report $VERSION: it is run on an aarch64 host, or under
# qemu-aarch64 when an aarch64 sysroot is installed (Debian/Ubuntu: qemu-user
# + libc6-arm64-cross); elsewhere the version string must at least be in it.
#
# Artifacts written to $OUT_DIR:
#   vesyl-print-X.Y.Z-linux-aarch64.tar.gz
#   vesyl-print-X.Y.Z.manifest.json   (not with BUILD_ONLY=1)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

die() {
  echo "ERROR: $*" >&2
  exit 1
}

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(tr -d '[:space:]' < VERSION)"
fi
VERSION="${VERSION#v}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$ ]]; then
  echo "ERROR: invalid version: $VERSION" >&2
  exit 1
fi

BUILD_ONLY="${BUILD_ONLY:-}"
SIGN_ONLY="${SIGN_ONLY:-}"
if [[ "$BUILD_ONLY" == "1" && "$SIGN_ONLY" == "1" ]]; then
  die "BUILD_ONLY=1 and SIGN_ONLY=1 are mutually exclusive"
fi

CHANNEL="${RELEASE_CHANNEL:-stable}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
ARCH="linux-aarch64"
GITHUB_REPOSITORY="${GITHUB_REPOSITORY:-vesylapp/vesyl-print}"
TAG="v${VERSION}"
ASSET_NAME="vesyl-print-${VERSION}-${ARCH}.tar.gz"
MANIFEST_NAME="vesyl-print-${VERSION}.manifest.json"
# GitHub Releases download base (CDN)
DOWNLOAD_BASE="https://github.com/${GITHUB_REPOSITORY}/releases/download/${TAG}"
ARTIFACT_URL="${DOWNLOAD_BASE}/${ASSET_NAME}"
TARBALL="${OUT_DIR}/${ASSET_NAME}"
MANIFEST="${OUT_DIR}/${MANIFEST_NAME}"

mkdir -p "$OUT_DIR"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/vesyl-print-release.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

# A manifest left from an earlier run never describes what this run builds.
rm -f "$MANIFEST"

# Resolve the signing key before anything is built, then drop it from the
# environment so no child process (cargo, build scripts, zig) inherits it.
KEY_FILE=""
if [[ "$BUILD_ONLY" == "1" ]]; then
  if [[ -n "${UPDATE_PRIVATE_KEY:-}${UPDATE_PRIVATE_KEY_FILE:-}" ]]; then
    echo "   BUILD_ONLY=1: ignoring UPDATE_PRIVATE_KEY / UPDATE_PRIVATE_KEY_FILE"
  fi
elif [[ -n "${UPDATE_PRIVATE_KEY_FILE:-}" ]]; then
  [[ -f "$UPDATE_PRIVATE_KEY_FILE" ]] ||
    die "UPDATE_PRIVATE_KEY_FILE not found: $UPDATE_PRIVATE_KEY_FILE"
  KEY_FILE="$UPDATE_PRIVATE_KEY_FILE"
elif [[ -n "${UPDATE_PRIVATE_KEY:-}" ]]; then
  KEY_FILE="${STAGE}/update_private.pem"
  # Preserve PEM newlines from multiline secrets; mode 0600 from creation.
  (umask 077 && printf '%s\n' "$UPDATE_PRIVATE_KEY" >"$KEY_FILE")
elif [[ -f /tmp/vesyl-print-update_private.pem ]]; then
  # Local lab key only (never commit)
  KEY_FILE=/tmp/vesyl-print-update_private.pem
fi
unset UPDATE_PRIVATE_KEY UPDATE_PRIVATE_KEY_FILE
if [[ "$SIGN_ONLY" == "1" && -z "$KEY_FILE" ]]; then
  die "SIGN_ONLY=1 needs UPDATE_PRIVATE_KEY or UPDATE_PRIVATE_KEY_FILE"
fi

RUST_TARGET="aarch64-unknown-linux-gnu"

# The packaged binary must report $VERSION.
check_binary_version() {
  local bin="$1" want="vesyl-print $VERSION" out qemu
  local sysroot="${AARCH64_SYSROOT:-/usr/aarch64-linux-gnu}"
  local -a run=()
  if [[ "$(uname -m)" == "aarch64" ]]; then
    run=(env)
  elif qemu="$(command -v qemu-aarch64 || command -v qemu-aarch64-static)" &&
    [[ -e "$sysroot/lib/ld-linux-aarch64.so.1" ]]; then
    run=("$qemu" -L "$sysroot")
  fi
  if ((${#run[@]})); then
    out="$("${run[@]}" "$bin" --version)" ||
      die "packaged binary does not run: ${run[*]} $bin --version"
    [[ "$out" == "$want" ]] ||
      die "packaged binary reports '$out', expected '$want' (stale build?)"
    echo "   version: $out"
  else
    grep -aqF -- "$VERSION" "$bin" ||
      die "packaged binary does not contain version $VERSION (stale build?)"
    echo "   version: contains $VERSION (not run: needs an aarch64 host, or qemu-aarch64 + $sysroot)"
  fi
}

build_rust_binary() {
  if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    echo "ERROR: cargo-zigbuild not found (pip install ziglang cargo-zigbuild)" >&2
    echo "       or set SKIP_RUST_BINARY=1 for a Python-only release" >&2
    exit 1
  fi
  # Package exactly what this build produces: ask cargo where its target dir
  # is, pin that for the build, and delete the previous artifact there.
  local target_dir built
  target_dir="$(cd "$REPO_ROOT/rust" && cargo metadata --format-version 1 --no-deps |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
  [[ -n "$target_dir" ]] || die "cargo metadata reported no target directory"
  export CARGO_TARGET_DIR="$target_dir"
  built="$CARGO_TARGET_DIR/$RUST_TARGET/release/vesyl-print"
  rm -f "$built"
  echo "==> Building vesyl-print binary ($RUST_TARGET, glibc 2.31) in $CARGO_TARGET_DIR"
  (
    cd "$REPO_ROOT/rust"
    VESYL_PRINT_VERSION="$VERSION" cargo zigbuild --release --locked \
      --target "${RUST_TARGET}.2.31"
  )
  [[ -f "$built" ]] || die "cargo did not produce $built"
  install -m 0755 "$built" "$STAGE_TREE/vesyl-print"
  echo "   binary: $(du -h "$STAGE_TREE/vesyl-print" | cut -f1)"
  check_binary_version "$STAGE_TREE/vesyl-print"
}

if [[ "$SIGN_ONLY" == "1" ]]; then
  [[ -f "$TARBALL" ]] || die "SIGN_ONLY=1: missing $TARBALL (build it with BUILD_ONLY=1)"
  echo "==> Signing $TARBALL"
else
  rm -f "$TARBALL"
  STAGE_TREE="${STAGE}/vesyl-print-${VERSION}"
  mkdir -p "$STAGE_TREE"

  echo "==> Packaging version $VERSION ($ARCH)"

  # App runtime files (no git, tests, secrets, pyc)
  rsync -a \
    --exclude='.git/' \
    --exclude='__pycache__/' \
    --exclude='*.py[cod]' \
    --exclude='.pytest_cache/' \
    --exclude='tests/' \
    --exclude='dist/' \
    --exclude='*.egg-info/' \
    --exclude='.env' \
    --exclude='credentials.json' \
    --exclude='lcd-screenshot.png' \
    --exclude='.gitignore' \
    --exclude='keys/update_private.pem' \
    --exclude='**/update_private.pem' \
    --exclude='keys/tailscale.key' \
    --exclude='**/tailscale.key' \
    --exclude='rust/' \
    --exclude='/vesyl-print' \
    "$REPO_ROOT/" "$STAGE_TREE/"

  # Rust agent/CLI binary (version baked in from the release tag).
  if [[ "${SKIP_RUST_BINARY:-}" == "1" ]]; then
    echo "   SKIP_RUST_BINARY=1 — Python-only release"
  else
    build_rust_binary
  fi

  # Ensure VERSION matches release
  printf '%s\n' "$VERSION" >"$STAGE_TREE/VERSION"

  # Never leave a partial tarball behind for a later SIGN_ONLY run to sign.
  tar -C "$STAGE" -czf "${TARBALL}.partial" "vesyl-print-${VERSION}"
  mv -f "${TARBALL}.partial" "$TARBALL"
  echo "   tarball: $TARBALL"
fi

SHA256="$(sha256sum "$TARBALL")"
SHA256="${SHA256%% *}"
echo "   sha256:  $SHA256"

if [[ "$BUILD_ONLY" == "1" ]]; then
  echo "==> BUILD_ONLY=1: wrote $TARBALL (sign it with SIGN_ONLY=1)"
  exit 0
fi

# Manifest body first (everything except signature), then sign that exact canonical form.
BODY_JSON="${STAGE}/manifest.body.json"
python3 - "$VERSION" "$CHANNEL" "$ARTIFACT_URL" "$SHA256" <<'PY' >"$BODY_JSON"
import json, sys
from datetime import datetime, timezone
version, channel, url, sha = sys.argv[1:5]
body = {
    "version": version,
    "channel": channel,
    "artifact_url": url,
    "artifact_sha256": sha,
    "min_agent_version": "0.3.0",
    "released_at": datetime.now(timezone.utc).replace(microsecond=0).isoformat(),
}
print(json.dumps(body, indent=2))
PY

CANONICAL="${STAGE}/manifest.canonical.json"
python3 - "$BODY_JSON" "$CANONICAL" <<'PY'
import json, sys
from pathlib import Path
body = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
# No trailing newline — must match update.ReleaseManifest.canonical_bytes()
Path(sys.argv[2]).write_bytes(
    json.dumps(body, sort_keys=True, separators=(",", ":")).encode("utf-8")
)
PY

SIGNATURE=""
if [[ -n "$KEY_FILE" ]]; then
  echo "==> Signing manifest with Ed25519"
  SIG_BIN="${STAGE}/manifest.sig"
  openssl pkeyutl -sign -inkey "$KEY_FILE" -rawin -in "$CANONICAL" -out "$SIG_BIN"
  verifies_with() {
    openssl pkeyutl -verify -pubin -inkey "$1" -rawin -in "$CANONICAL" \
      -sigfile "$SIG_BIN" >/dev/null 2>&1
  }
  if [[ -n "${UPDATE_PUBLIC_KEY_FILE:-}" ]]; then
    [[ -f "$UPDATE_PUBLIC_KEY_FILE" ]] ||
      die "UPDATE_PUBLIC_KEY_FILE not found: $UPDATE_PUBLIC_KEY_FILE"
    verifies_with "$UPDATE_PUBLIC_KEY_FILE" ||
      die "signature does not verify against $UPDATE_PUBLIC_KEY_FILE (wrong signing key?)"
    echo "   signature verifies against $UPDATE_PUBLIC_KEY_FILE"
  elif [[ -f "$REPO_ROOT/keys/update_public.pem" ]]; then
    if verifies_with "$REPO_ROOT/keys/update_public.pem"; then
      echo "   signature verifies against keys/update_public.pem"
    else
      echo "WARNING: signature does not verify against keys/update_public.pem;" \
        "devices with that key will reject this manifest (expected for a lab key)" >&2
    fi
  fi
  SIGNATURE="$(base64 -w0 <"$SIG_BIN" 2>/dev/null || base64 <"$SIG_BIN" | tr -d '\n')"
else
  echo "WARNING: no UPDATE_PRIVATE_KEY / UPDATE_PRIVATE_KEY_FILE — manifest unsigned" >&2
fi

python3 - "$BODY_JSON" "$SIGNATURE" <<'PY' >"${MANIFEST}.partial"
import json, sys
from pathlib import Path
body = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
sig = sys.argv[2]
if sig:
    body["signature"] = sig
print(json.dumps(body, indent=2) + "\n")
PY
mv -f "${MANIFEST}.partial" "$MANIFEST"

echo "==> Wrote $MANIFEST"
echo
echo "Upload to GitHub release ${TAG}:"
echo "  gh release create ${TAG} \\"
echo "    ${TARBALL} \\"
echo "    ${MANIFEST} \\"
echo "    --title \"vesyl-print ${VERSION}\" \\"
echo "    --generate-notes"
echo
echo "Device manifest URL:"
echo "  ${DOWNLOAD_BASE}/${MANIFEST_NAME}"
