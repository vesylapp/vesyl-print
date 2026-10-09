#!/usr/bin/env bash
#
# Build a signed vesyl-print OTA release (tarball + manifest).
#
# Usage:
#   ./scripts/build-release.sh [VERSION]            # default: the VERSION file
#   UPDATE_PRIVATE_KEY_FILE=/path/to/key.pem ./scripts/build-release.sh 0.5.0
#
# By default this builds the tarball and signs its manifest in one go. CI runs
# the steps in separate jobs, so the signing key never shares a runner with
# cargo build scripts, proc-macros or the zig toolchain:
#   BUILD_ONLY=1   build and package the tarball only. Writes no manifest and
#                  never reads a key (one left in the environment is dropped).
#   SIGN_ONLY=1    sign the tarball already in $OUT_DIR: hash it and write its
#                  signed manifest. Runs only jq, openssl, sha256sum and
#                  coreutils: no cargo, rsync, tar or Python. Needs a key.
#   VERIFY_ONLY=1  check the tarball + manifest already in $OUT_DIR before they
#                  are published: the manifest names this version, artifact URL
#                  and tarball sha256, and its signature verifies against
#                  UPDATE_PUBLIC_KEY_FILE (default keys/update_public.pem).
#                  Same tools as SIGN_ONLY; never reads a private key.
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
#   RELEASE_CHANGELOG        optional release notes, signed into the manifest
#   MIN_AGENT_VERSION        oldest agent that may install this release over
#                            OTA (default: 0.4.0). Python-era 0.3.x agents
#                            refuse it: their systemd units still run python3,
#                            so those devices are re-provisioned with setup.sh.
#   OUT_DIR                  output directory (default: dist)
#   AARCH64_SYSROOT          aarch64 glibc root for qemu-aarch64
#                            (default: /usr/aarch64-linux-gnu)
#   CI                       set on CI runners: the packaged binary must run
#                            (no fallback to finding the version string)
#
# The tarball holds the vesyl-print binary (Rust agent + CLI) at its root, the
# Python LCD display (*.py), assets, and the files setup.sh needs to provision
# a device from the extracted tree. It never holds rust/, tests/ or secrets.
# The binary is cross-compiled for aarch64 (glibc >= 2.31) with cargo-zigbuild
# from cargo's own target directory (CARGO_TARGET_DIR and build.target-dir are
# honoured), after any previous build there is deleted. It must need no glibc
# symbol version newer than 2.31 (readelf -V, from binutils), and it must
# report $VERSION: it is run on an aarch64 host, or under qemu-aarch64 when an
# aarch64 sysroot is installed (Debian/Ubuntu: qemu-user + libc6-arm64-cross);
# elsewhere, outside CI, the version string must at least be in it.
#
# The signature is Ed25519 over the manifest's canonical JSON: every field but
# "signature" and nulls, keys sorted, compact, non-ASCII escaped as \uXXXX
# (jq -S -c -a). Devices rebuild the same bytes in update.rs
# ReleaseManifest::canonical_bytes(); the two must never diverge.
#
# Artifacts written to $OUT_DIR:
#   vesyl-print-X.Y.Z-linux-aarch64.tar.gz   (not with SIGN_ONLY / VERIFY_ONLY)
#   vesyl-print-X.Y.Z.manifest.json          (not with BUILD_ONLY / VERIFY_ONLY)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

die() {
  echo "ERROR: $*" >&2
  exit 1
}

VERSION="${1:-}"
VERSION_FROM="argument"
if [[ -z "$VERSION" ]]; then
  VERSION="$(tr -d '[:space:]' < VERSION)"
  VERSION_FROM="VERSION file"
fi
VERSION="${VERSION#v}"
# A release version, checked as update.rs is_version (and scripts/apply-update,
# setup.sh) checks it: this pattern, with a last dot-component of "staging"
# refused, since <version>.staging is the directory an interrupted extract
# leaves beside its slot.
VERSION_RE='^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$'
is_version() {
  [[ "$1" =~ $VERSION_RE && "$1" != *.staging ]]
}
is_version "$VERSION" || die "invalid version: $VERSION"

BUILD_ONLY="${BUILD_ONLY:-}"
SIGN_ONLY="${SIGN_ONLY:-}"
VERIFY_ONLY="${VERIFY_ONLY:-}"
modes=0
for mode in "$BUILD_ONLY" "$SIGN_ONLY" "$VERIFY_ONLY"; do
  if [[ "$mode" == "1" ]]; then
    modes=$((modes + 1))
  fi
done
if ((modes > 1)); then
  die "BUILD_ONLY=1, SIGN_ONLY=1 and VERIFY_ONLY=1 are mutually exclusive"
fi

command -v jq >/dev/null 2>&1 || die "jq not found (Debian/Ubuntu: apt install jq)"

CHANNEL="${RELEASE_CHANNEL:-stable}"
CHANGELOG="${RELEASE_CHANGELOG:-}"
MIN_AGENT_VERSION="${MIN_AGENT_VERSION:-0.4.0}"
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

# version_core_ge A B: A's major.minor.patch >= B's (suffixes ignored).
version_core_ge() {
  local -a a b
  local i
  IFS=.- read -r -a a <<<"$1"
  IFS=.- read -r -a b <<<"$2"
  for i in 0 1 2; do
    if ((10#${a[i]} != 10#${b[i]})); then
      ((10#${a[i]} > 10#${b[i]}))
      return
    fi
  done
}

if [[ "$VERIFY_ONLY" != "1" ]]; then
  is_version "$MIN_AGENT_VERSION" ||
    die "invalid MIN_AGENT_VERSION: $MIN_AGENT_VERSION"
  # A device running this release must be able to install the next one.
  # Never suggest bumping VERSION: done before the release is tagged, that is
  # a trap of its own (OTA_UPDATES.md §4.8).
  if ! version_core_ge "$VERSION" "$MIN_AGENT_VERSION"; then
    [[ "$VERSION_FROM" == "VERSION file" ]] ||
      die "version $VERSION is below MIN_AGENT_VERSION $MIN_AGENT_VERSION: devices" \
        "running it could never update over OTA again (see OTA_UPDATES.md §4.8" \
        "for how to number a build)"
    die "version $VERSION from the VERSION file is below MIN_AGENT_VERSION" \
      "$MIN_AGENT_VERSION: devices running it could never update over OTA again." \
      "Give the version to build as the argument (see OTA_UPDATES.md §4.8)"
  fi
fi

mkdir -p "$OUT_DIR"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/vesyl-print-release.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

# The manifest's canonical JSON: the exact bytes that are signed and verified.
canonical_json() {
  jq -S -c -a -j 'del(.signature) | with_entries(select(.value != null))' "$1"
}

# manifest_verifies <manifest> <public key>: its signature is good.
manifest_verifies() {
  local canonical="${STAGE}/verify.canonical.json" sig="${STAGE}/verify.sig"
  canonical_json "$1" >"$canonical" || return 1
  jq -r '.signature // ""' "$1" | tr -d '[:space:]' | base64 -d >"$sig" 2>/dev/null || return 1
  openssl pkeyutl -verify -pubin -inkey "$2" -rawin -in "$canonical" \
    -sigfile "$sig" >/dev/null 2>&1
}

sha256_of() {
  local sum
  sum="$(sha256sum "$1")"
  printf '%s\n' "${sum%% *}"
}

if [[ "$VERIFY_ONLY" == "1" ]]; then
  if [[ -n "${UPDATE_PRIVATE_KEY:-}${UPDATE_PRIVATE_KEY_FILE:-}" ]]; then
    echo "   VERIFY_ONLY=1: ignoring UPDATE_PRIVATE_KEY / UPDATE_PRIVATE_KEY_FILE"
  fi
  unset UPDATE_PRIVATE_KEY UPDATE_PRIVATE_KEY_FILE
  PUBLIC_KEY="${UPDATE_PUBLIC_KEY_FILE:-$REPO_ROOT/keys/update_public.pem}"
  [[ -f "$PUBLIC_KEY" ]] || die "public key not found: $PUBLIC_KEY"
  [[ -f "$TARBALL" ]] || die "VERIFY_ONLY=1: missing $TARBALL"
  [[ -f "$MANIFEST" ]] || die "VERIFY_ONLY=1: missing $MANIFEST"
  echo "==> Verifying $MANIFEST"
  jq -e 'type == "object"' "$MANIFEST" >/dev/null 2>&1 || die "$MANIFEST is not a JSON object"
  field() { jq -r --arg k "$1" '.[$k] // "" | tostring' "$MANIFEST"; }
  problems=()
  got="$(field version)"
  [[ "$got" == "$VERSION" ]] || problems+=("version '$got' != '$VERSION'")
  [[ "$(field artifact_sha256)" == "$(sha256_of "$TARBALL")" ]] ||
    problems+=("artifact_sha256 does not match $ASSET_NAME")
  got="$(field artifact_url)"
  [[ "$got" == "$ARTIFACT_URL" ]] || problems+=("artifact_url '$got' != '$ARTIFACT_URL'")
  [[ -n "$(field signature)" ]] || problems+=("manifest is not signed")
  if ((${#problems[@]})); then
    printf -v joined '%s; ' "${problems[@]}"
    die "refusing to publish: ${joined%; }"
  fi
  manifest_verifies "$MANIFEST" "$PUBLIC_KEY" ||
    die "refusing to publish: signature does not verify against $PUBLIC_KEY"
  echo "   $MANIFEST_NAME describes $ASSET_NAME and verifies against $PUBLIC_KEY"
  exit 0
fi

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
if [[ -n "$KEY_FILE" && -n "${UPDATE_PUBLIC_KEY_FILE:-}" && ! -f "$UPDATE_PUBLIC_KEY_FILE" ]]; then
  die "UPDATE_PUBLIC_KEY_FILE not found: $UPDATE_PUBLIC_KEY_FILE"
fi

RUST_TARGET="aarch64-unknown-linux-gnu"
# The newest glibc the binary may need (Debian bullseye's): cargo-zigbuild
# links against this version's symbols, and the packaged binary is checked
# against it.
GLIBC_FLOOR="2.31"

# Runtime files, relative to the repo root (rsync filter rules, first match
# wins). Everything else stays out: rust/, tests/, .github/, keys other than
# the public key, requirements.txt, dist/, and any untracked local files.
PACKAGE_FILTER=(
  --exclude='__pycache__/'
  --exclude='*.py[cod]'
  --exclude='update_private.pem'
  --exclude='tailscale.key'
  --exclude='credentials.json'
  --exclude='.env'
  --exclude='/scripts/build-release.sh'
  --include='/VERSION'
  --include='/*.py'
  --include='/*.service'
  --include='/*.md'
  --include='/setup.sh'
  --include='/base.jpg'
  --include='/assets/***'
  --include='/overlays/***'
  --include='/scripts/***'
  --include='/keys/'
  --include='/keys/update_public.pem'
  --exclude='*'
)
# What a device needs from the tree: the units run vesyl-print and main.py,
# setup.sh provisions from it (with both root helpers), test-print sends the
# test labels, and base.jpg is the sample image for
# `vesyl-print print-test --file /opt/vesyl-print/current/base.jpg`.
REQUIRED_FILES=(
  vesyl-print
  VERSION
  main.py
  setup.sh
  vesyl-print-agent.service
  vesyl-print-display.service
  scripts/apply-update
  scripts/wifi-setup
  keys/update_public.pem
  base.jpg
  assets/test-labels/vesyl-roadrunner-4x6.pdf
  assets/test-labels/vesyl-roadrunner-4x6.zpl
)

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
    # CI runners install both, so there a binary that was not run fails.
    [[ -z "${CI:-}" ]] ||
      die "cannot run the packaged binary: CI needs qemu-aarch64 and an aarch64" \
        "sysroot at $sysroot (Debian/Ubuntu: qemu-user + libc6-arm64-cross)"
    grep -aqF -- "$VERSION" "$bin" ||
      die "packaged binary does not contain version $VERSION (stale build?)"
    echo "   version: contains $VERSION (not run: needs an aarch64 host, or qemu-aarch64 + $sysroot)"
  fi
}

# glibc_le A B: glibc version A (2.17, 2.3.4, ...) is not newer than B.
glibc_le() {
  local -a a b
  local i
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for i in 0 1 2; do
    if ((10#${a[i]:-0} != 10#${b[i]:-0})); then
      ((10#${a[i]:-0} < 10#${b[i]:-0}))
      return
    fi
  done
}

# The packaged binary must need no glibc symbol version newer than
# $GLIBC_FLOOR. The qemu run cannot show that: the build host's aarch64
# sysroot is usually newer than the oldest supported device.
check_glibc_floor() {
  local bin="$1" needs v newest=""
  needs="$(readelf -V "$bin")" || die "readelf -V $bin failed"
  while read -r v; do
    v="${v#GLIBC_}"
    if [[ -z "$newest" ]] || ! glibc_le "$v" "$newest"; then
      newest="$v"
    fi
  done < <(grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' <<<"$needs")
  [[ -n "$newest" ]] ||
    die "packaged binary needs no GLIBC_ symbol version (not linked against glibc?)"
  glibc_le "$newest" "$GLIBC_FLOOR" ||
    die "packaged binary needs glibc $newest, newer than the $GLIBC_FLOOR floor" \
      "(not built for ${RUST_TARGET}.${GLIBC_FLOOR}?)"
  echo "   glibc:   needs at most $newest (floor $GLIBC_FLOOR)"
}

build_rust_binary() {
  command -v cargo-zigbuild >/dev/null 2>&1 ||
    die "cargo-zigbuild not found: install it with zig (the versions CI pins:" \
      "pip install --require-hashes -r .github/zigbuild-requirements.txt; or" \
      "cargo install --locked cargo-zigbuild plus zig on PATH)"
  # Needed only once the binary is built, but checked before the build,
  # which takes minutes.
  command -v readelf >/dev/null 2>&1 ||
    die "readelf not found (binutils): it checks the binary's glibc floor"
  # Package exactly what this build produces: ask cargo where its target dir
  # is, pin that for the build, and delete the previous artifact there.
  local target_dir built
  target_dir="$(cd "$REPO_ROOT/rust" && cargo metadata --format-version 1 --no-deps |
    jq -r '.target_directory // empty')"
  [[ -n "$target_dir" ]] || die "cargo metadata reported no target directory"
  export CARGO_TARGET_DIR="$target_dir"
  built="$CARGO_TARGET_DIR/$RUST_TARGET/release/vesyl-print"
  rm -f "$built"
  echo "==> Building vesyl-print binary ($RUST_TARGET, glibc $GLIBC_FLOOR) in $CARGO_TARGET_DIR"
  (
    cd "$REPO_ROOT/rust"
    VESYL_PRINT_VERSION="$VERSION" cargo zigbuild --release --locked \
      --target "${RUST_TARGET}.${GLIBC_FLOOR}"
  )
  [[ -f "$built" ]] || die "cargo did not produce $built"
  install -m 0755 "$built" "$STAGE_TREE/vesyl-print"
  echo "   binary: $(du -h "$STAGE_TREE/vesyl-print" | cut -f1)"
  check_glibc_floor "$STAGE_TREE/vesyl-print"
  check_binary_version "$STAGE_TREE/vesyl-print"
}

if [[ "$SIGN_ONLY" == "1" ]]; then
  [[ -f "$TARBALL" ]] || die "SIGN_ONLY=1: missing $TARBALL (build it with BUILD_ONLY=1)"
  echo "==> Signing $TARBALL"
else
  [[ -z "${SKIP_RUST_BINARY:-}" ]] ||
    die "SKIP_RUST_BINARY is no longer supported: the agent and CLI are the" \
      "vesyl-print binary, so every release ships it"
  rm -f "$TARBALL"
  STAGE_TREE="${STAGE}/vesyl-print-${VERSION}"
  mkdir -p "$STAGE_TREE"

  echo "==> Packaging version $VERSION ($ARCH)"
  rsync -a "${PACKAGE_FILTER[@]}" "$REPO_ROOT/" "$STAGE_TREE/"

  # Rust agent/CLI binary (version baked in from the release tag).
  build_rust_binary

  # Ensure VERSION matches release
  printf '%s\n' "$VERSION" >"$STAGE_TREE/VERSION"

  for f in "${REQUIRED_FILES[@]}"; do
    [[ -f "$STAGE_TREE/$f" ]] || die "release tree is missing $f"
  done

  # Never leave a partial tarball behind for a later SIGN_ONLY run to sign.
  # Owned by root in the archive: extracting it as root must not hand the tree
  # to whichever local account has the build machine's uid.
  tar --owner=0 --group=0 --numeric-owner \
    -C "$STAGE" -czf "${TARBALL}.partial" "vesyl-print-${VERSION}"
  mv -f "${TARBALL}.partial" "$TARBALL"
  echo "   tarball: $TARBALL"
fi

SHA256="$(sha256_of "$TARBALL")"
echo "   sha256:  $SHA256"

if [[ "$BUILD_ONLY" == "1" ]]; then
  echo "==> BUILD_ONLY=1: wrote $TARBALL (sign it with SIGN_ONLY=1)"
  exit 0
fi

# Manifest body first (everything except the signature), then sign its
# canonical form.
BODY_JSON="${STAGE}/manifest.body.json"
jq -n -a \
  --arg version "$VERSION" \
  --arg channel "$CHANNEL" \
  --arg artifact_url "$ARTIFACT_URL" \
  --arg artifact_sha256 "$SHA256" \
  --arg min_agent_version "$MIN_AGENT_VERSION" \
  --arg released_at "$(date -u +%Y-%m-%dT%H:%M:%S+00:00)" \
  --arg changelog "$CHANGELOG" \
  '{version: $version, channel: $channel, artifact_url: $artifact_url,
    artifact_sha256: $artifact_sha256, min_agent_version: $min_agent_version,
    released_at: $released_at}
   + (if $changelog == "" then {} else {changelog: $changelog} end)' \
  >"$BODY_JSON"

CANONICAL="${STAGE}/manifest.canonical.json"
canonical_json "$BODY_JSON" >"$CANONICAL"

SIGNATURE=""
if [[ -n "$KEY_FILE" ]]; then
  echo "==> Signing manifest with Ed25519"
  SIG_BIN="${STAGE}/manifest.sig"
  openssl pkeyutl -sign -inkey "$KEY_FILE" -rawin -in "$CANONICAL" -out "$SIG_BIN"
  SIGNATURE="$(base64 -w0 <"$SIG_BIN" 2>/dev/null || base64 <"$SIG_BIN" | tr -d '\n')"
else
  echo "WARNING: no UPDATE_PRIVATE_KEY / UPDATE_PRIVATE_KEY_FILE — manifest unsigned" >&2
fi

jq -a --arg signature "$SIGNATURE" \
  'if $signature == "" then . else . + {signature: $signature} end' \
  "$BODY_JSON" >"${MANIFEST}.partial"

# Check the manifest as written, the way the publish job and devices see it.
if [[ -n "$SIGNATURE" ]]; then
  if [[ -n "${UPDATE_PUBLIC_KEY_FILE:-}" ]]; then
    manifest_verifies "${MANIFEST}.partial" "$UPDATE_PUBLIC_KEY_FILE" || {
      rm -f "${MANIFEST}.partial"
      die "signature does not verify against $UPDATE_PUBLIC_KEY_FILE (wrong signing key?)"
    }
    echo "   signature verifies against $UPDATE_PUBLIC_KEY_FILE"
  elif [[ -f "$REPO_ROOT/keys/update_public.pem" ]]; then
    if manifest_verifies "${MANIFEST}.partial" "$REPO_ROOT/keys/update_public.pem"; then
      echo "   signature verifies against keys/update_public.pem"
    else
      echo "WARNING: signature does not verify against keys/update_public.pem;" \
        "devices with that key will reject this manifest (expected for a lab key)" >&2
    fi
  fi
fi
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
