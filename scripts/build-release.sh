#!/usr/bin/env bash
#
# Build a signed vesyl-print OTA release (tarball + manifest).
#
# Usage:
#   ./scripts/build-release.sh
#   ./scripts/build-release.sh 0.4.0
#   UPDATE_PRIVATE_KEY_FILE=/path/to/key.pem ./scripts/build-release.sh
#
# Env:
#   UPDATE_PRIVATE_KEY       PEM private key contents (CI secret)
#   UPDATE_PRIVATE_KEY_FILE  Path to PEM private key
#   GITHUB_REPOSITORY        owner/repo (default: vesylapp/vesyl-print)
#   RELEASE_CHANNEL          stable|beta (default: stable)
#   OUT_DIR                  output directory (default: dist)
#
# Artifacts written to $OUT_DIR:
#   vesyl-print-X.Y.Z-linux-aarch64.tar.gz
#   vesyl-print-X.Y.Z.manifest.json
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(tr -d '[:space:]' < VERSION)"
fi
VERSION="${VERSION#v}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$ ]]; then
  echo "ERROR: invalid version: $VERSION" >&2
  exit 1
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

mkdir -p "$OUT_DIR"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/vesyl-print-release.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

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
  "$REPO_ROOT/" "$STAGE_TREE/"

# Ensure VERSION matches release
printf '%s\n' "$VERSION" >"$STAGE_TREE/VERSION"

TARBALL="${OUT_DIR}/${ASSET_NAME}"
tar -C "$STAGE" -czf "$TARBALL" "vesyl-print-${VERSION}"
SHA256="$(sha256sum "$TARBALL" | awk '{print $1}')"
echo "   tarball: $TARBALL"
echo "   sha256:  $SHA256"

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

# Resolve private key
KEY_FILE=""
if [[ -n "${UPDATE_PRIVATE_KEY_FILE:-}" && -f "$UPDATE_PRIVATE_KEY_FILE" ]]; then
  KEY_FILE="$UPDATE_PRIVATE_KEY_FILE"
elif [[ -n "${UPDATE_PRIVATE_KEY:-}" ]]; then
  KEY_FILE="${STAGE}/update_private.pem"
  # Preserve PEM newlines from multiline secrets
  printf '%s\n' "$UPDATE_PRIVATE_KEY" >"$KEY_FILE"
  chmod 600 "$KEY_FILE"
elif [[ -f /tmp/vesyl-print-update_private.pem ]]; then
  # Local lab key only (never commit)
  KEY_FILE=/tmp/vesyl-print-update_private.pem
fi

SIGNATURE=""
if [[ -n "$KEY_FILE" ]]; then
  echo "==> Signing manifest with Ed25519"
  SIG_BIN="${STAGE}/manifest.sig"
  openssl pkeyutl -sign -inkey "$KEY_FILE" -rawin -in "$CANONICAL" -out "$SIG_BIN"
  SIGNATURE="$(base64 -w0 <"$SIG_BIN" 2>/dev/null || base64 <"$SIG_BIN" | tr -d '\n')"
else
  echo "WARNING: no UPDATE_PRIVATE_KEY / UPDATE_PRIVATE_KEY_FILE — manifest unsigned" >&2
fi

MANIFEST="${OUT_DIR}/${MANIFEST_NAME}"
python3 - "$BODY_JSON" "$SIGNATURE" <<'PY' >"$MANIFEST"
import json, sys
from pathlib import Path
body = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
sig = sys.argv[2]
if sig:
    body["signature"] = sig
print(json.dumps(body, indent=2) + "\n")
PY

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
