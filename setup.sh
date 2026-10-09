#!/usr/bin/env bash
#
# VESYL Print — provisioning script for a Raspberry Pi with the MHS-3.5"
# (ILI9486 SPI) display. Idempotent: safe to run more than once.
#
# Run it from an extracted release tarball (vesyl-print-X.Y.Z-linux-aarch64.tar.gz):
# a release holds the vesyl-print binary (the Rust agent + CLI) next to the
# Python LCD display. A git checkout has no binary, so setup stops before
# changing anything and says how to get one. Running it on a device set up by
# an older, Python-agent release re-provisions that device: units, CLI wrapper
# and root helpers are rewritten and the old release slots are removed.
#
# It:
#   1. installs system packages: CUPS, poppler-utils, NetworkManager (agent);
#      python3, Pillow, numpy and DejaVu fonts (LCD display), and segno (the
#      LCD's Wi-Fi QR code) where the distro has it,
#   2. enables SPI + the mhs35 display overlay in the boot config,
#   3. installs the mhs35 device-tree overlay if the OS doesn't have it,
#   4. creates /etc/vesyl-print + /var/lib/vesyl-print,
#   5. installs the root helpers (OTA apply-update, Wi-Fi setup) + sudoers drop-in,
#   6. installs the release into /opt/vesyl-print/releases/<ver> + current symlink,
#   7. installs the vesyl-print CLI wrapper (runs current/vesyl-print, as the
#      service account when root runs it),
#   8. installs and enables the LCD + cloud agent systemd services,
#   9. installs Tailscale and joins the tailnet (auth key from keys/tailscale.key),
#  10. removes the extracted release it ran from (app runs from /opt); never
#      a git checkout or a directory not named vesyl-print-X.Y.Z.
#
# Usage:  sudo ./setup.sh
#
# Optional env (give it to sudo, which drops the caller's environment:
# sudo SKIP_TAILSCALE=1 ./setup.sh):
#   INSTALL_ROOT=/opt/vesyl-print   # dual-slot root (default); absolute, components
#                                   # of letters, digits and ._- only
#                                   # (no . or ..); trailing slashes dropped
#   SKIP_APP_INSTALL=1              # only deps/config/units; keep the installed release
#   SKIP_TAILSCALE=1                # skip Tailscale install / join
#   TAILSCALE_AUTH_KEY_FILE=...     # override path to auth key (default: keys/tailscale.key)
#   SKIP_SOURCE_CLEANUP=1           # keep source tree after install (lab/dev)
#
set -euo pipefail

die() {
    echo "!! $*" >&2
    exit 1
}

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$REPO_DIR/$(basename "${BASH_SOURCE[0]}")"

INSTALL_ROOT_GIVEN="${INSTALL_ROOT:-/opt/vesyl-print}"
INSTALL_ROOT="$INSTALL_ROOT_GIVEN"
# Written into the units, the CLI wrapper and both root helpers. apply-update
# compares it, as a string, with the paths the agent builds from it (Rust
# drops a trailing slash when it joins paths), so it must be in plain form:
# no trailing slash, and no empty, "." or ".." component.
while [[ "$INSTALL_ROOT" == */ ]]; do
    INSTALL_ROOT="${INSTALL_ROOT%/}"
done
if [[ ! "$INSTALL_ROOT" =~ ^(/[A-Za-z0-9._-]+)+$ ||
    "$INSTALL_ROOT/" == */./* || "$INSTALL_ROOT/" == */../* ]]; then
    die "INSTALL_ROOT must be an absolute path whose components are letters," \
        "digits and ._- (no '.', '..' or empty ones): '$INSTALL_ROOT_GIVEN'"
fi
DISPLAY_SERVICE="vesyl-print-display"
AGENT_SERVICE="vesyl-print-agent"
LEGACY_DISPLAY_SERVICE="printserve-display"
CLI_PATH="/usr/local/bin/vesyl-print"
APPLY_UPDATE="/usr/local/lib/vesyl-print/apply-update"
WIFI_SETUP="/usr/local/lib/vesyl-print/wifi-setup"
SUDOERS_DROPIN="/etc/sudoers.d/vesyl-print"
# A release version, checked as update.rs is_version (and scripts/apply-update,
# build-release.sh) checks it: this pattern, with a last dot-component of
# "staging" refused, since <version>.staging is the directory an interrupted
# extract leaves beside its slot.
VERSION_RE='^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$'
is_version() {
    [[ "$1" =~ $VERSION_RE && "$1" != *.staging ]]
}

if [[ -f "$REPO_DIR/VERSION" ]]; then
    APP_VERSION="$(tr -d '[:space:]' <"$REPO_DIR/VERSION")"
else
    APP_VERSION="0.0.0"
fi
APP_VERSION="${APP_VERSION#v}"
RELEASE_DIR="${INSTALL_ROOT}/releases/${APP_VERSION}"
CURRENT_LINK="${INSTALL_ROOT}/current"
APP_BIN="$REPO_DIR/vesyl-print"

# Each root helper fixes its install root on one line, which is rewritten for
# INSTALL_ROOT when it is installed: the line as an anchored sed pattern, and
# what it becomes.
APPLY_UPDATE_ROOT='^INSTALL_ROOT=/opt/vesyl-print$'
APPLY_UPDATE_ROOT_LINE="INSTALL_ROOT=$INSTALL_ROOT"
WIFI_SETUP_ROOT='^INSTALL_ROOT = Path("/opt/vesyl-print")$'
WIFI_SETUP_ROOT_LINE="INSTALL_ROOT = Path(\"$INSTALL_ROOT\")"

# with_install_root FILE PATTERN LINE: FILE on stdout, with the line matching
# PATTERN replaced by LINE. Exactly one line must match, so an edit to a
# helper cannot silently skip the substitution and leave the installed copy
# working on /opt/vesyl-print.
with_install_root() {
    local n
    n="$(grep -c -- "$2" "$1")" || true
    [[ "$n" == 1 ]] ||
        die "$1: ${n:-0} lines match '$2' (its install root), expected exactly 1"
    sed "s|$2|$3|" "$1"
}

# A file of the release being installed (with SKIP_APP_INSTALL=1, of the
# active one when this tree lacks it).
release_file() {
    local f
    for f in "$REPO_DIR/$1" "$CURRENT_LINK/$1"; do
        if [[ -f "$f" ]]; then
            printf '%s\n' "$f"
            return 0
        fi
    done
    return 1
}

# --- 0. preflight (before sudo or any change) ------------------------------
# The agent and CLI are the vesyl-print binary, which only release tarballs
# carry. Check it is here, runs, and belongs to this release.
check_release_binary() {
    if [[ ! -f "$APP_BIN" || ! -x "$APP_BIN" ]]; then
        cat >&2 <<EOF
!! $APP_BIN not found.
   The agent and CLI are the vesyl-print binary, which ships in release
   tarballs; a git checkout does not have it. Run setup.sh from an extracted
   release (X.Y.Z = the version to install):
     curl -fLO https://github.com/vesylapp/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z-linux-aarch64.tar.gz
     tar -xzf vesyl-print-X.Y.Z-linux-aarch64.tar.gz
     sudo ./vesyl-print-X.Y.Z/setup.sh
   or build a release from this checkout (needs cargo-zigbuild, binutils, jq
   and rsync) and run the setup.sh inside it; README.md ("Re-provisioning a
   Python-era device") says how to number X.Y.Z for a build that is not a
   release:
     BUILD_ONLY=1 ./scripts/build-release.sh X.Y.Z
     tar -xzf dist/vesyl-print-X.Y.Z-linux-aarch64.tar.gz
     sudo ./vesyl-print-X.Y.Z/setup.sh
EOF
        exit 1
    fi
    # As root this runs a file the tree's owner can change (the installed
    # binary the CLI wrapper never runs as root, see step 7). setup.sh is
    # read from that same tree as it runs, so this gives the owner nothing
    # more.
    local got
    got="$("$APP_BIN" --version 2>&1)" ||
        die "cannot run $APP_BIN ($got): is it built for $(uname -m)?"
    [[ "$got" == "vesyl-print $APP_VERSION" ]] ||
        die "$APP_BIN reports '$got' but VERSION is $APP_VERSION: use the binary from the same release"
}

if [[ "${SKIP_APP_INSTALL:-}" == "1" ]]; then
    [[ -x "$CURRENT_LINK/vesyl-print" ]] ||
        die "SKIP_APP_INSTALL=1 but $CURRENT_LINK/vesyl-print is missing: install a release first"
else
    is_version "$APP_VERSION" ||
        die "$REPO_DIR/VERSION ('$APP_VERSION') is not a release version"
    check_release_binary
fi
for f in "${DISPLAY_SERVICE}.service" "${AGENT_SERVICE}.service" scripts/apply-update; do
    [[ -f "$REPO_DIR/$f" ]] || die "$REPO_DIR/$f missing (not a vesyl-print release tree?)"
done
# The root helpers' install-root lines, which step 5 rewrites.
with_install_root "$REPO_DIR/scripts/apply-update" \
    "$APPLY_UPDATE_ROOT" "$APPLY_UPDATE_ROOT_LINE" >/dev/null
WIFI_SRC="$(release_file scripts/wifi-setup)" || WIFI_SRC=""
if [[ -n "$WIFI_SRC" ]]; then
    with_install_root "$WIFI_SRC" "$WIFI_SETUP_ROOT" "$WIFI_SETUP_ROOT_LINE" >/dev/null
fi

# --- must run as root ------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "This script must run as root. Re-running with sudo..." >&2
    # sudo drops the caller's environment; carry the documented knobs across.
    keep=()
    for var in INSTALL_ROOT SKIP_APP_INSTALL SKIP_TAILSCALE TAILSCALE_AUTH_KEY_FILE SKIP_SOURCE_CLEANUP; do
        if [[ -n "${!var+set}" ]]; then
            keep+=("$var=${!var}")
        fi
    done
    exec sudo -- env "${keep[@]}" bash "$SELF" "$@"
fi

# Account the services run as: the invoking sudo user, else the repo owner.
RUN_USER="${SUDO_USER:-}"
if [[ -z "$RUN_USER" || "$RUN_USER" == "root" ]]; then
    RUN_USER="$(stat -c '%U' "$REPO_DIR")"
fi
if [[ "$RUN_USER" == "root" ]] || ! id "$RUN_USER" >/dev/null 2>&1; then
    die "the services need a normal account (e.g. vesyl), not '$RUN_USER':" \
        "run 'sudo ./setup.sh' as that account, or chown the source tree to it"
fi
RUN_GROUP="$(id -gn "$RUN_USER")"

echo "==> Source tree:  $REPO_DIR"
echo "==> Install root: $INSTALL_ROOT (version $APP_VERSION)"
echo "==> Run as user:  $RUN_USER"

# --- 1. dependencies -------------------------------------------------------
# Agent (the vesyl-print binary): CUPS, pdftoppm, nmcli. LCD (Python):
# python3, Pillow, numpy, DejaVu fonts. apt-get installs none of the packages
# it is given when one of them is missing, so the optional one goes alone.
REQUIRED_PACKAGES=(cups poppler-utils network-manager rsync
    python3 python3-pil python3-numpy fonts-dejavu-core)
# pkg_installed PKG: dpkg has PKG installed. A failed apt-get does not say:
# offline, with the mirror blocked or the dpkg lock held, a device set up
# before still has its packages.
pkg_installed() {
    [[ "$(dpkg-query -W -f='${db:Status-Status}' "$1" 2>/dev/null)" == "installed" ]]
}
echo "==> Installing packages (CUPS, poppler-utils, NetworkManager, python3 + Pillow/numpy, fonts)..."
apt-get update || echo "   (apt-get update failed — continuing with cached lists)"
if ! apt-get install -y "${REQUIRED_PACKAGES[@]}"; then
    missing=()
    for pkg in "${REQUIRED_PACKAGES[@]}"; do
        pkg_installed "$pkg" || missing+=("$pkg")
    done
    ((${#missing[@]} == 0)) ||
        die "apt-get could not install required packages: ${missing[*]} (see its errors above)"
    echo "   (apt-get install failed — every required package is already installed, continuing)"
fi
# segno draws the Wi-Fi setup QR code; without it the LCD shows the network
# name and PIN as text. Not packaged everywhere, so best effort.
if ! apt-get install -y python3-segno; then
    if pkg_installed python3-segno; then
        echo "   (apt-get install python3-segno failed — keeping the installed one)"
    else
        echo "   WARNING: python3-segno not installed: the Wi-Fi setup screen shows" \
            "text instead of a QR code" >&2
    fi
fi

# The service user must be in 'video' to write /dev/fb1, 'lpadmin' to
# discover and add network printers to CUPS without sudo, and 'input' to
# read the MHS-3.5" resistive touchscreen (/dev/input/event*) for page cycle.
# lp: /dev/usb/lp* for USB printers on some images; lpadmin: manage CUPS queues
usermod -aG video,lpadmin,lp,input,netdev "$RUN_USER"

# Captive-portal DNS for the setup hotspot (phones auto-open the login page).
install -d -m 0755 /etc/NetworkManager/dnsmasq-shared.d
cat > /etc/NetworkManager/dnsmasq-shared.d/vesyl-captive.conf <<'DNS'
# vesyl-print setup hotspot — managed by setup.sh
address=/#/10.42.0.1
dhcp-option=114,http://10.42.0.1/
DNS
chmod 0644 /etc/NetworkManager/dnsmasq-shared.d/vesyl-captive.conf

# --- 2. locate the boot config + overlays dir ------------------------------
if [[ -f /boot/firmware/config.txt ]]; then
    CONFIG_TXT=/boot/firmware/config.txt
    OVERLAYS_DIR=/boot/firmware/overlays
elif [[ -f /boot/config.txt ]]; then
    CONFIG_TXT=/boot/config.txt
    OVERLAYS_DIR=/boot/overlays
else
    echo "!! Could not find config.txt in /boot or /boot/firmware" >&2
    exit 1
fi
echo "==> Boot config: $CONFIG_TXT"

# --- 3. display overlay ----------------------------------------------------
if [[ -f "$REPO_DIR/overlays/mhs35.dtbo" ]]; then
    if [[ ! -f "$OVERLAYS_DIR/mhs35.dtbo" ]]; then
        echo "==> Installing mhs35 overlay into $OVERLAYS_DIR"
        install -m 0755 "$REPO_DIR/overlays/mhs35.dtbo" "$OVERLAYS_DIR/mhs35.dtbo"
    else
        echo "==> mhs35 overlay already present"
    fi
else
    echo "==> No overlays/mhs35.dtbo in source tree — skip (LCD-show bootstrap may own this)"
fi

# Enable SPI + the display overlay (append only what's missing, once).
ensure_line() {
    local line="$1"
    if ! grep -qxF "$line" "$CONFIG_TXT"; then
        # back up config.txt the first time we touch it
        [[ -f "${CONFIG_TXT}.printserve.bak" ]] || cp "$CONFIG_TXT" "${CONFIG_TXT}.printserve.bak"
        if ! grep -qF "# printserve display" "$CONFIG_TXT"; then
            printf '\n# printserve display (added by setup.sh)\n' >> "$CONFIG_TXT"
        fi
        echo "$line" >> "$CONFIG_TXT"
        echo "   added to config.txt: $line"
    fi
}
echo "==> Ensuring SPI + mhs35 overlay in $CONFIG_TXT"
ensure_line "dtparam=spi=on"
ensure_line "dtoverlay=mhs35:rotate=90"

# --- 3b. boot splash (replace the Raspberry Pi Plymouth splash) ------------
PIX_THEME=/usr/share/plymouth/themes/pix
SPLASH_SRC=""
if [[ -f "$REPO_DIR/assets/plymouth-splash.png" ]]; then
    SPLASH_SRC="$REPO_DIR/assets/plymouth-splash.png"
elif [[ -f "$CURRENT_LINK/assets/plymouth-splash.png" ]]; then
    SPLASH_SRC="$CURRENT_LINK/assets/plymouth-splash.png"
fi
if [[ -d "$PIX_THEME" && -n "$SPLASH_SRC" ]]; then
    if ! cmp -s "$SPLASH_SRC" "$PIX_THEME/splash.png"; then
        echo "==> Installing VESYL Plymouth splash"
        [[ -f "$PIX_THEME/splash.png.rpi-orig" ]] || \
            cp "$PIX_THEME/splash.png" "$PIX_THEME/splash.png.rpi-orig"
        cp "$SPLASH_SRC" "$PIX_THEME/splash.png"
        echo "   Rebuilding initramfs so Plymouth picks it up (~1-2 min)..."
        update-initramfs -u
    else
        echo "==> VESYL Plymouth splash already installed"
    fi
else
    echo "==> Skipping splash (pix theme or splash asset not found)"
fi

# --- 4. config + state dirs ------------------------------------------------
echo "==> Creating /etc/vesyl-print and /var/lib/vesyl-print"
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /etc/vesyl-print
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print/queue
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print/processed

if [[ ! -f /etc/vesyl-print/config.json ]]; then
    cat > /etc/vesyl-print/config.json <<'CFG'
{
  "api_base_url": "https://wms-api.vesyl.dev",
  "cable_url": "wss://wms-api.vesyl.dev/print/cable",
  "heartbeat_seconds": 30,
  "pull_interval_seconds": 5,
  "pull_jobs_enabled": true,
  "cable_enabled": true,
  "auto_update_enabled": true,
  "update_channel": "stable",
  "releases_base_url": "https://github.com/vesylapp/vesyl-print/releases/download"
}
CFG
    chown "$RUN_USER:$RUN_GROUP" /etc/vesyl-print/config.json
    chmod 0644 /etc/vesyl-print/config.json
    echo "   wrote /etc/vesyl-print/config.json"
else
    echo "   config.json already present"
fi

# --- 5. root helpers + sudoers ---------------------------------------------
# Taken from the release being installed (see release_file). Their install
# root is fixed in the file, never taken from the caller.
# install_helper SRC DEST PATTERN LINE (see with_install_root)
install_helper() {
    local tmp
    tmp="$(mktemp)"
    with_install_root "$1" "$3" "$4" >"$tmp"
    install -o root -g root -m 0755 "$tmp" "$2"
    rm -f "$tmp"
}

install -d -m 0755 /usr/local/lib/vesyl-print
echo "==> Installing OTA helper: $APPLY_UPDATE"
install_helper "$REPO_DIR/scripts/apply-update" "$APPLY_UPDATE" \
    "$APPLY_UPDATE_ROOT" "$APPLY_UPDATE_ROOT_LINE"

if [[ -n "$WIFI_SRC" ]]; then
    echo "==> Installing Wi-Fi helper: $WIFI_SETUP"
    install_helper "$WIFI_SRC" "$WIFI_SETUP" "$WIFI_SETUP_ROOT" "$WIFI_SETUP_ROOT_LINE"
else
    echo "   WARNING: scripts/wifi-setup missing — skip helper" >&2
fi

echo "==> Installing sudoers drop-in: $SUDOERS_DROPIN"
tmp_sudoers="$(mktemp)"
{
    echo "# vesyl-print helpers — managed by setup.sh (do not edit by hand)"
    echo "$RUN_USER ALL=(root) NOPASSWD: $APPLY_UPDATE"
    if [[ -x "$WIFI_SETUP" ]]; then
        echo "$RUN_USER ALL=(root) NOPASSWD: $WIFI_SETUP"
    fi
} > "$tmp_sudoers"
if visudo -cf "$tmp_sudoers" >/dev/null 2>&1; then
    install -m 0440 "$tmp_sudoers" "$SUDOERS_DROPIN"
    echo "   $RUN_USER may run: sudo -n $APPLY_UPDATE / $WIFI_SETUP"
else
    echo "   WARNING: sudoers snippet failed visudo -cf — not installed" >&2
    cat "$tmp_sudoers" >&2
fi
rm -f "$tmp_sudoers"

# Public key for manifest signature verify (the binary also has it built in).
if KEY_SRC="$(release_file keys/update_public.pem)"; then
    install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /etc/vesyl-print/keys
    install -m 0644 -o "$RUN_USER" -g "$RUN_GROUP" \
        "$KEY_SRC" /etc/vesyl-print/keys/update_public.pem
    echo "   installed /etc/vesyl-print/keys/update_public.pem"
fi

# --- 6. Install the release into /opt/vesyl-print (dual-slot) --------------
if [[ "${SKIP_APP_INSTALL:-}" == "1" ]]; then
    echo "==> SKIP_APP_INSTALL=1 — keeping $CURRENT_LINK → $(readlink -f "$CURRENT_LINK")"
else
    echo "==> Installing app → $RELEASE_DIR"
    install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 \
        "$INSTALL_ROOT" \
        "$INSTALL_ROOT/releases" \
        "$INSTALL_ROOT/update"

    # Refresh this version slot from the source tree (normally an extracted
    # release). Do not wipe other releases/ (OTA history).
    if [[ -L "$RELEASE_DIR" || ( -e "$RELEASE_DIR" && ! -d "$RELEASE_DIR" ) ]]; then
        rm -f "$RELEASE_DIR"
    fi
    mkdir -p "$RELEASE_DIR"
    # Through an install root that is a symlink: newer rsync (3.5 here)
    # refuses one on its way to the destination (ELOOP).
    RELEASE_DIR_REAL="$(cd "$RELEASE_DIR" && pwd -P)"

    # Never copied into a slot: VCS/CI/dev trees, Rust sources, tests, secrets.
    if command -v rsync >/dev/null 2>&1; then
        rsync -a --delete \
            --exclude='.git/' \
            --exclude='.github/' \
            --exclude='.claude/' \
            --exclude='rust/' \
            --exclude='tests/' \
            --exclude='dist/' \
            --exclude='__pycache__/' \
            --exclude='*.py[cod]' \
            --exclude='.pytest_cache/' \
            --exclude='*.egg-info/' \
            --exclude='.env' \
            --exclude='credentials.json' \
            --exclude='lcd-screenshot.png' \
            --exclude='update_private.pem' \
            --exclude='tailscale.key' \
            "$REPO_DIR/" "$RELEASE_DIR_REAL/"
    else
        # Fallback without rsync (same exclusions)
        find "$RELEASE_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
        tar -C "$REPO_DIR" \
            --exclude='.git' \
            --exclude='.github' \
            --exclude='.claude' \
            --exclude='rust' \
            --exclude='tests' \
            --exclude='dist' \
            --exclude='__pycache__' \
            --exclude='.env' \
            --exclude='credentials.json' \
            --exclude='update_private.pem' \
            --exclude='tailscale.key' \
            -cf - . | tar -C "$RELEASE_DIR" -xf -
    fi

    printf '%s\n' "$APP_VERSION" >"$RELEASE_DIR/VERSION"
    # The trailing slash makes chown -R go through an install root that is
    # a symlink (to a data disk, say) and hand over the tree it points at:
    # without it, chown -R changes the link itself and nothing under it.
    chown -R "$RUN_USER:$RUN_GROUP" "$INSTALL_ROOT/"
    # Such a link stays root's (an older setup.sh handed it over): root
    # follows a link into the service account's trees only when root owns
    # it (README, App stack).
    if [[ -L "$INSTALL_ROOT" ]]; then
        chown -h root:root "$INSTALL_ROOT"
    fi

    # Atomic current → this version, through the helper OTA uses (it refuses
    # a slot without the executable binary).
    echo "==> Activating $APP_VERSION as current"
    "$APPLY_UPDATE" activate "$RELEASE_DIR" "$CURRENT_LINK"
    [[ -x "$CURRENT_LINK/vesyl-print" ]] ||
        die "Activate failed: $CURRENT_LINK/vesyl-print missing"
    echo "   current → $(readlink -f "$CURRENT_LINK" 2>/dev/null || readlink "$CURRENT_LINK")"

    # Slots without the binary (Python-era releases) cannot run under these
    # units. Remove them so `vesyl-print update rollback` never picks one,
    # and the <version>.staging dirs an interrupted update leaves behind.
    for slot in "$INSTALL_ROOT"/releases/*; do
        name="${slot##*/}"
        if [[ ! -d "$slot" || -L "$slot" || "$slot" == "$RELEASE_DIR" ]]; then
            continue
        elif [[ "$name" == *.staging ]] && is_version "${name%.staging}"; then
            echo "   removing $name: left by an interrupted update"
        elif is_version "$name" && [[ ! -x "$slot/vesyl-print" ]]; then
            echo "   removing release $name: no vesyl-print binary"
        else
            continue
        fi
        rm -rf -- "$slot"
    done
fi

# --- 7. CLI wrapper (always follows current) -------------------------------
# The service account owns every release slot (OTA writes them as that
# account), so the binary is its code: run as root (sudo vesyl-print ...),
# the wrapper runs it as that account, as the units do. Every privileged
# step the CLI takes (activate, restart) goes through the apply-update
# helper that account may run with sudo -n.
echo "==> Installing CLI: $CLI_PATH"
tmp="$(mktemp)"
cat > "$tmp" <<WRAP
#!/usr/bin/env bash
# vesyl-print CLI: runs the active release's binary. Managed by setup.sh.
# As root, it runs it as the service account, which owns it.
BIN="$CURRENT_LINK/vesyl-print"
if [[ ! -x "\$BIN" ]]; then
    echo "vesyl-print: \$BIN not found; re-run setup.sh from a release" >&2
    exit 127
fi
export VESYL_PRINT_INSTALL_ROOT="\${VESYL_PRINT_INSTALL_ROOT:-$INSTALL_ROOT}"
if [[ \$EUID -eq 0 ]]; then
    exec runuser -u "$RUN_USER" -- "\$BIN" "\$@"
fi
exec "\$BIN" "\$@"
WRAP
install -o root -g root -m 0755 "$tmp" "$CLI_PATH"
rm -f "$tmp"

# --- 8. systemd services (run from current) --------------------------------
# The release's own unit files, with the service account and install root
# filled in.
install_unit() {
    local name="$1" tmp
    echo "==> Installing systemd unit: /etc/systemd/system/$name"
    tmp="$(mktemp)"
    sed -e "s|^User=.*|User=$RUN_USER|" -e "s|/opt/vesyl-print|$INSTALL_ROOT|g" \
        "$REPO_DIR/$name" >"$tmp"
    install -o root -g root -m 0644 "$tmp" "/etc/systemd/system/$name"
    rm -f "$tmp"
}
install_unit "${DISPLAY_SERVICE}.service"
install_unit "${AGENT_SERVICE}.service"

systemctl daemon-reload

# Migrate off legacy unit name if present
if [[ -f "/etc/systemd/system/${LEGACY_DISPLAY_SERVICE}.service" ]]; then
    echo "==> Migrating ${LEGACY_DISPLAY_SERVICE} → ${DISPLAY_SERVICE}"
    systemctl disable --now "${LEGACY_DISPLAY_SERVICE}.service" 2>/dev/null || true
    rm -f "/etc/systemd/system/${LEGACY_DISPLAY_SERVICE}.service"
    systemctl daemon-reload
fi

systemctl enable "$DISPLAY_SERVICE"
systemctl enable "$AGENT_SERVICE"

# --- 9. Tailscale (optional auth key) --------------------------------------
# Auth key is factory-only: never copied into /opt release slots (rsync excludes it).
if [[ "${SKIP_TAILSCALE:-}" == "1" ]]; then
    echo "==> SKIP_TAILSCALE=1 — not installing/joining Tailscale"
else
    TS_KEY_FILE="${TAILSCALE_AUTH_KEY_FILE:-$REPO_DIR/keys/tailscale.key}"

    if [[ ! -f "$TS_KEY_FILE" ]]; then
        echo "==> No Tailscale auth key ($TS_KEY_FILE) — skip Tailscale"
    else
        echo "==> Tailscale: ensure installed"
        if ! command -v tailscale >/dev/null 2>&1; then
            # Official installer works on Raspberry Pi OS / Debian aarch64.
            if ! command -v curl >/dev/null 2>&1; then
                apt-get install -y curl || true
            fi
            curl -fsSL https://tailscale.com/install.sh | sh
        else
            echo "   tailscale already present: $(command -v tailscale)"
        fi

        if ! command -v tailscale >/dev/null 2>&1; then
            echo "   WARNING: tailscale install failed — skip join" >&2
        else
            # Idempotent: already logged in → leave alone.
            if tailscale status >/dev/null 2>&1; then
                echo "   Tailscale already joined — skip tailscale up"
                tailscale status 2>/dev/null | head -5 || true
            else
                echo "==> Tailscale: joining network (hostname=$(hostname))"
                # Read key without printing it; strip whitespace/newlines.
                AUTHKEY="$(tr -d '[:space:]' <"$TS_KEY_FILE")"
                if [[ -z "$AUTHKEY" ]]; then
                    echo "   WARNING: $TS_KEY_FILE is empty — skip tailscale up" >&2
                else
                    if tailscale up \
                        --hostname="$(hostname)" \
                        --auth-key="$AUTHKEY"; then
                        echo "   Tailscale up OK"
                        tailscale status 2>/dev/null | head -8 || true
                        # One-time auth keys: delete only after successful join.
                        rm -f "$TS_KEY_FILE"
                        echo "   removed one-time auth key: $TS_KEY_FILE"
                    else
                        echo "   WARNING: tailscale up failed (auth key left in place for retry)" >&2
                    fi
                fi
                unset AUTHKEY
            fi
        fi
    fi
fi

echo
echo "==> Done."
echo "   App:      $INSTALL_ROOT/current → $(readlink -f "$CURRENT_LINK" 2>/dev/null || echo "$CURRENT_LINK")"
echo "   Services: $DISPLAY_SERVICE, $AGENT_SERVICE"
echo "   CLI:      $CLI_PATH"
echo "   OTA:      $APPLY_UPDATE (+ $SUDOERS_DROPIN)"
if command -v tailscale >/dev/null 2>&1; then
    echo "   Tailscale: $(tailscale ip -4 2>/dev/null || echo 'installed')"
fi
echo "   Pair:     vesyl-print claim <CODE>"
echo "   Status:   vesyl-print status --check"
if [[ -e /dev/fb1 ]]; then
    echo "   /dev/fb1 present — starting services now."
    systemctl restart "$DISPLAY_SERVICE"
    systemctl restart "$AGENT_SERVICE"
else
    echo "   /dev/fb1 not present yet — REBOOT to load the display driver:"
    echo "     sudo reboot"
    systemctl restart "$AGENT_SERVICE" || true
fi

# --- 10. Remove factory source tree ----------------------------------------
# App + CLI run from /opt/vesyl-print/current. The tree used for setup (an
# extracted release, e.g. ~/vesyl-print-X.Y.Z) may hold one-time secrets
# (Tailscale key) and is not needed. Only that tree is removed: never the
# install root or a slot, a git checkout, or a directory with another name.
if [[ "${SKIP_SOURCE_CLEANUP:-}" == "1" ]]; then
    echo "==> SKIP_SOURCE_CLEANUP=1 — keeping source tree $REPO_DIR"
elif [[ "${SKIP_APP_INSTALL:-}" == "1" ]]; then
    echo "==> SKIP_APP_INSTALL=1 — not removing source tree"
else
    REPO_RESOLVED="$(readlink -f "$REPO_DIR" 2>/dev/null || echo "$REPO_DIR")"
    INSTALL_RESOLVED="$(readlink -f "$INSTALL_ROOT" 2>/dev/null || echo "$INSTALL_ROOT")"
    CURRENT_RESOLVED="$(readlink -f "$CURRENT_LINK" 2>/dev/null || true)"

    safe_to_remove=1
    if [[ ! -x "$CURRENT_LINK/vesyl-print" ]]; then
        echo "==> Source cleanup skipped: $CURRENT_LINK incomplete"
        safe_to_remove=0
    fi
    # Never delete the live install tree or anything under it.
    if [[ $safe_to_remove -eq 1 ]]; then
        if [[ "$REPO_RESOLVED" == "$INSTALL_RESOLVED" || "$REPO_RESOLVED" == "$INSTALL_RESOLVED"/* ]]; then
            echo "==> Source cleanup skipped: source is under install root ($REPO_RESOLVED)"
            safe_to_remove=0
        elif [[ -n "$CURRENT_RESOLVED" && ( "$REPO_RESOLVED" == "$CURRENT_RESOLVED" || "$REPO_RESOLVED" == "$CURRENT_RESOLVED"/* ) ]]; then
            echo "==> Source cleanup skipped: source is the active slot ($REPO_RESOLVED)"
            safe_to_remove=0
        elif [[ -z "$REPO_RESOLVED" || "$REPO_RESOLVED" == "/" || "$REPO_RESOLVED" == "/home" || "$REPO_RESOLVED" == "/root" || "$REPO_RESOLVED" == "/opt" ]]; then
            echo "==> Source cleanup skipped: refusing path $REPO_RESOLVED"
            safe_to_remove=0
        elif [[ ! -f "$REPO_RESOLVED/setup.sh" || ! -f "$REPO_RESOLVED/VERSION" ]]; then
            echo "==> Source cleanup skipped: $REPO_RESOLVED does not look like vesyl-print source"
            safe_to_remove=0
        elif [[ -e "$REPO_RESOLVED/.git" ]]; then
            echo "==> Source cleanup skipped: $REPO_RESOLVED is a git checkout"
            safe_to_remove=0
        elif [[ "${REPO_RESOLVED##*/}" != vesyl-print-* ]]; then
            # Release tarballs unpack into vesyl-print-X.Y.Z/; anything else
            # (a home directory, say) is not ours to delete.
            echo "==> Source cleanup skipped: $REPO_RESOLVED is not an extracted release (vesyl-print-X.Y.Z)"
            safe_to_remove=0
        fi
    fi

    if [[ $safe_to_remove -eq 1 ]]; then
        echo "==> Removing factory source tree: $REPO_RESOLVED"
        # After this, only /opt/vesyl-print/current remains for the app.
        rm -rf "$REPO_RESOLVED"
        echo "   source tree removed (runtime is $INSTALL_ROOT/current)"
    fi
fi
