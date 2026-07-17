#!/usr/bin/env bash
#
# VESYL Print — provisioning script for a Raspberry Pi with the MHS-3.5"
# (ILI9486 SPI) display. Idempotent: safe to run more than once.
#
# It:
#   1. installs the Python/font dependencies the app needs,
#   2. enables SPI + the mhs35 display overlay in the boot config,
#   3. installs the mhs35 device-tree overlay if the OS doesn't have it,
#   4. creates /etc/vesyl-print + /var/lib/vesyl-print,
#   5. installs the app into /opt/vesyl-print/releases/<ver> + current symlink,
#   6. installs the OTA apply-update helper + sudoers drop-in,
#   7. installs and enables the LCD + cloud agent systemd services,
#   8. installs the vesyl-print CLI wrapper (points at current).
#
# Usage:  sudo ./setup.sh
#
# Optional env:
#   INSTALL_ROOT=/opt/vesyl-print   # dual-slot root (default)
#   SKIP_APP_INSTALL=1              # only deps/config/units; don't rsync app tree
#
set -euo pipefail

# --- must run as root ------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "This script must run as root. Re-running with sudo..." >&2
    exec sudo -- "$0" "$@"
fi

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Account the service runs as: the invoking sudo user, else the repo owner.
RUN_USER="${SUDO_USER:-}"
if [[ -z "$RUN_USER" || "$RUN_USER" == "root" ]]; then
    RUN_USER="$(stat -c '%U' "$REPO_DIR")"
fi
RUN_GROUP="$(id -gn "$RUN_USER")"

INSTALL_ROOT="${INSTALL_ROOT:-/opt/vesyl-print}"
DISPLAY_SERVICE="vesyl-print-display"
AGENT_SERVICE="vesyl-print-agent"
LEGACY_DISPLAY_SERVICE="printserve-display"
DISPLAY_UNIT="/etc/systemd/system/${DISPLAY_SERVICE}.service"
AGENT_UNIT="/etc/systemd/system/${AGENT_SERVICE}.service"
CLI_PATH="/usr/local/bin/vesyl-print"
APPLY_UPDATE="/usr/local/lib/vesyl-print/apply-update"
SUDOERS_DROPIN="/etc/sudoers.d/vesyl-print"

if [[ -f "$REPO_DIR/VERSION" ]]; then
    APP_VERSION="$(tr -d '[:space:]' <"$REPO_DIR/VERSION")"
else
    APP_VERSION="0.0.0"
fi
APP_VERSION="${APP_VERSION#v}"
RELEASE_DIR="${INSTALL_ROOT}/releases/${APP_VERSION}"
CURRENT_LINK="${INSTALL_ROOT}/current"

echo "==> Source tree:  $REPO_DIR"
echo "==> Install root: $INSTALL_ROOT (version $APP_VERSION)"
echo "==> Run as user:  $RUN_USER"

# --- 1. dependencies -------------------------------------------------------
echo "==> Installing dependencies (python3, Pillow, numpy, fonts, CUPS, websocket, cryptography)..."
apt-get update || echo "   (apt-get update failed — continuing with cached lists)"
apt-get install -y python3 python3-pil python3-numpy fonts-dejavu-core cups \
    python3-websocket python3-cryptography rsync || true
# Fallback if distro package missing — ActionCable push needs websocket-client.
if ! python3 -c "import websocket" 2>/dev/null; then
    echo "==> Installing websocket-client via pip"
    pip3 install --break-system-packages websocket-client || \
        pip3 install websocket-client || \
        echo "   WARNING: websocket-client install failed — cable push disabled (pull still works)"
fi

# The service user must be in 'video' to write /dev/fb1, and 'lpadmin' to
# discover and add network printers to CUPS without sudo.
usermod -aG video,lpadmin "$RUN_USER"

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
  "releases_base_url": "https://github.com/benwyrosdick/vesyl-print/releases/download"
}
CFG
    chown "$RUN_USER:$RUN_GROUP" /etc/vesyl-print/config.json
    chmod 0644 /etc/vesyl-print/config.json
    echo "   wrote /etc/vesyl-print/config.json"
else
    echo "   config.json already present"
fi

# --- 5. Install app into /opt/vesyl-print (dual-slot) ----------------------
if [[ "${SKIP_APP_INSTALL:-}" == "1" ]]; then
    echo "==> SKIP_APP_INSTALL=1 — not copying app tree"
else
    echo "==> Installing app → $RELEASE_DIR"
    install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 \
        "$INSTALL_ROOT" \
        "$INSTALL_ROOT/releases" \
        "$INSTALL_ROOT/update"

    # Refresh this version slot from the source tree (git checkout or extracted release).
    # Do not wipe other releases/ (OTA history).
    if [[ -e "$RELEASE_DIR" && ! -d "$RELEASE_DIR" ]]; then
        rm -f "$RELEASE_DIR"
    fi
    mkdir -p "$RELEASE_DIR"

    if command -v rsync >/dev/null 2>&1; then
        rsync -a --delete \
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
            --exclude='keys/update_private.pem' \
            --exclude='**/update_private.pem' \
            "$REPO_DIR/" "$RELEASE_DIR/"
    else
        # Fallback without rsync (still excludes .git / private key)
        find "$RELEASE_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
        tar -C "$REPO_DIR" \
            --exclude='.git' \
            --exclude='__pycache__' \
            --exclude='tests' \
            --exclude='dist' \
            --exclude='keys/update_private.pem' \
            -cf - . | tar -C "$RELEASE_DIR" -xf -
    fi

    printf '%s\n' "$APP_VERSION" >"$RELEASE_DIR/VERSION"
    chown -R "$RUN_USER:$RUN_GROUP" "$INSTALL_ROOT"

    # Atomic current → this version (root helper if available)
    echo "==> Activating $APP_VERSION as current"
    if [[ -x "$APPLY_UPDATE" ]] || [[ -f "$REPO_DIR/scripts/apply-update" ]]; then
        # Prefer installed helper; fall back to repo copy for first install order
        HELPER="$APPLY_UPDATE"
        if [[ ! -x "$HELPER" ]]; then
            HELPER="$REPO_DIR/scripts/apply-update"
            chmod +x "$HELPER"
        fi
        "$HELPER" activate "$RELEASE_DIR" "$CURRENT_LINK"
    else
        ln -sfn "releases/${APP_VERSION}" "$CURRENT_LINK"
    fi
    # Ensure current resolves
    if [[ ! -e "$CURRENT_LINK/agent.py" && ! -e "$CURRENT_LINK/main.py" ]]; then
        echo "!! Activate failed: $CURRENT_LINK missing app entrypoints" >&2
        exit 1
    fi
    echo "   current → $(readlink -f "$CURRENT_LINK" 2>/dev/null || readlink "$CURRENT_LINK")"
fi

APP_ROOT="$CURRENT_LINK"
if [[ ! -e "$APP_ROOT/agent.py" && -e "$REPO_DIR/agent.py" ]]; then
    echo "   WARNING: $CURRENT_LINK incomplete — units will fall back to source tree $REPO_DIR"
    APP_ROOT="$REPO_DIR"
fi

# --- 6. OTA apply-update helper + sudoers ----------------------------------
echo "==> Installing OTA helper: $APPLY_UPDATE"
install -d -m 0755 /usr/local/lib/vesyl-print
HELPER_SRC=""
if [[ -f "$APP_ROOT/scripts/apply-update" ]]; then
    HELPER_SRC="$APP_ROOT/scripts/apply-update"
elif [[ -f "$REPO_DIR/scripts/apply-update" ]]; then
    HELPER_SRC="$REPO_DIR/scripts/apply-update"
fi
if [[ -n "$HELPER_SRC" ]]; then
    install -m 0755 "$HELPER_SRC" "$APPLY_UPDATE"
else
    echo "   WARNING: scripts/apply-update missing — skip helper" >&2
fi

if [[ -x "$APPLY_UPDATE" ]]; then
    echo "==> Installing sudoers drop-in: $SUDOERS_DROPIN"
    tmp_sudoers="$(mktemp)"
    cat > "$tmp_sudoers" <<SUDO
# vesyl-print OTA — managed by setup.sh (do not edit by hand)
# Allows the service user to activate releases and restart units only.
$RUN_USER ALL=(root) NOPASSWD: $APPLY_UPDATE
SUDO
    if visudo -cf "$tmp_sudoers" >/dev/null 2>&1; then
        install -m 0440 "$tmp_sudoers" "$SUDOERS_DROPIN"
        echo "   $RUN_USER may run: sudo -n $APPLY_UPDATE …"
    else
        echo "   WARNING: sudoers snippet failed visudo -cf — not installed" >&2
        cat "$tmp_sudoers" >&2
    fi
    rm -f "$tmp_sudoers"
fi

# Optional: public key for manifest signature verify
KEY_SRC=""
if [[ -f "$APP_ROOT/keys/update_public.pem" ]]; then
    KEY_SRC="$APP_ROOT/keys/update_public.pem"
elif [[ -f "$REPO_DIR/keys/update_public.pem" ]]; then
    KEY_SRC="$REPO_DIR/keys/update_public.pem"
fi
if [[ -n "$KEY_SRC" ]]; then
    install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /etc/vesyl-print/keys
    install -m 0644 -o "$RUN_USER" -g "$RUN_GROUP" \
        "$KEY_SRC" /etc/vesyl-print/keys/update_public.pem
    echo "   installed /etc/vesyl-print/keys/update_public.pem"
fi

# --- 7. CLI wrapper (always follows current) -------------------------------
echo "==> Installing CLI: $CLI_PATH"
cat > "$CLI_PATH" <<WRAP
#!/usr/bin/env bash
# Prefer dual-slot install; fall back to legacy git checkout path.
APP="${INSTALL_ROOT}/current"
if [[ ! -f "\$APP/cli.py" ]]; then
  APP="$REPO_DIR"
fi
exec /usr/bin/python3 "\$APP/cli.py" "\$@"
WRAP
chmod 0755 "$CLI_PATH"

# --- 8. systemd services (run from current) --------------------------------
echo "==> Installing systemd unit: $DISPLAY_UNIT"
cat > "$DISPLAY_UNIT" <<UNIT
[Unit]
Description=VESYL Print — LCD system info display
# Wait until Plymouth has quit before painting, so the app doesn't fight the
# boot splash over the LCD (which causes flicker). Clean hand-off: splash
# through boot, then the app takes over.
After=plymouth-quit-wait.service

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$INSTALL_ROOT/current
ExecStart=/usr/bin/python3 $INSTALL_ROOT/current/main.py
ExecStopPost=/usr/bin/python3 $INSTALL_ROOT/current/main.py --offline
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

echo "==> Installing systemd unit: $AGENT_UNIT"
cat > "$AGENT_UNIT" <<UNIT
[Unit]
Description=VESYL Print — cloud agent (heartbeat / pairing / OTA)
After=network-online.target cups.service
Wants=network-online.target

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$INSTALL_ROOT/current
ExecStart=/usr/bin/python3 $INSTALL_ROOT/current/agent.py
Restart=on-failure
RestartSec=5
Environment=PYTHONUNBUFFERED=1
Environment=VESYL_PRINT_INSTALL_ROOT=$INSTALL_ROOT

[Install]
WantedBy=multi-user.target
UNIT

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

echo
echo "==> Done."
echo "   App:      $INSTALL_ROOT/current → $(readlink -f "$CURRENT_LINK" 2>/dev/null || echo "$CURRENT_LINK")"
echo "   Services: $DISPLAY_SERVICE, $AGENT_SERVICE"
echo "   CLI:      $CLI_PATH"
echo "   OTA:      $APPLY_UPDATE (+ $SUDOERS_DROPIN)"
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
