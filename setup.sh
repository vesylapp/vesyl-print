#!/usr/bin/env bash
#
# VESYL Print — provisioning script for a Raspberry Pi with the MHS-3.5"
# (ILI9486 SPI) display. Idempotent: safe to run more than once.
#
# It:
#   1. installs the Python/font dependencies the app needs,
#   2. enables SPI + the mhs35 display overlay in the boot config,
#   3. installs the mhs35 device-tree overlay if the OS doesn't have it,
#   4. installs and enables the systemd service that drives the display.
#
# Usage:  sudo ./setup.sh
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

SERVICE_NAME="printserve-display"
UNIT_PATH="/etc/systemd/system/${SERVICE_NAME}.service"

echo "==> Repo:        $REPO_DIR"
echo "==> Run as user: $RUN_USER"

# --- 1. dependencies -------------------------------------------------------
echo "==> Installing dependencies (python3, Pillow, numpy, fonts, CUPS)..."
apt-get update || echo "   (apt-get update failed — continuing with cached lists)"
apt-get install -y python3 python3-pil python3-numpy fonts-dejavu-core cups

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
if [[ ! -f "$OVERLAYS_DIR/mhs35.dtbo" ]]; then
    echo "==> Installing mhs35 overlay into $OVERLAYS_DIR"
    install -m 0755 "$REPO_DIR/overlays/mhs35.dtbo" "$OVERLAYS_DIR/mhs35.dtbo"
else
    echo "==> mhs35 overlay already present"
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
# The LCD's SPI driver loads ~15-20s into boot; until then the panel can't
# show anything. Once it appears, Plymouth's splash is what's on screen until
# the app starts, so we swap in a VESYL splash. Plymouth caches theme assets
# from the initramfs, so the image change only takes effect after rebuilding.
PIX_THEME=/usr/share/plymouth/themes/pix
if [[ -d "$PIX_THEME" && -f "$REPO_DIR/assets/plymouth-splash.png" ]]; then
    if ! cmp -s "$REPO_DIR/assets/plymouth-splash.png" "$PIX_THEME/splash.png"; then
        echo "==> Installing VESYL Plymouth splash"
        [[ -f "$PIX_THEME/splash.png.rpi-orig" ]] || \
            cp "$PIX_THEME/splash.png" "$PIX_THEME/splash.png.rpi-orig"
        cp "$REPO_DIR/assets/plymouth-splash.png" "$PIX_THEME/splash.png"
        echo "   Rebuilding initramfs so Plymouth picks it up (~1-2 min)..."
        update-initramfs -u
    else
        echo "==> VESYL Plymouth splash already installed"
    fi
else
    echo "==> Skipping splash (pix theme or splash asset not found)"
fi

# --- 4. systemd service ----------------------------------------------------
echo "==> Installing systemd unit: $UNIT_PATH"
cat > "$UNIT_PATH" <<UNIT
[Unit]
Description=VESYL Print — LCD system info display
# Wait until Plymouth has quit before painting, so the app doesn't fight the
# boot splash over the LCD (which causes flicker). Clean hand-off: splash
# through boot, then the app takes over.
After=plymouth-quit-wait.service

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$REPO_DIR
ExecStart=/usr/bin/python3 $REPO_DIR/main.py
ExecStopPost=/usr/bin/python3 $REPO_DIR/main.py --offline
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

echo
echo "==> Done."
echo "   Service '$SERVICE_NAME' is enabled and will start on boot."
if [[ -e /dev/fb1 ]]; then
    echo "   /dev/fb1 present — starting now."
    systemctl restart "$SERVICE_NAME"
else
    echo "   /dev/fb1 not present yet — REBOOT to load the display driver:"
    echo "     sudo reboot"
fi
