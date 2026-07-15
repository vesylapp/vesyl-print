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
#   5. installs and enables the LCD + cloud agent systemd services,
#   6. installs the vesyl-print CLI wrapper.
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
RUN_GROUP="$(id -gn "$RUN_USER")"

DISPLAY_SERVICE="printserve-display"
AGENT_SERVICE="vesyl-print-agent"
DISPLAY_UNIT="/etc/systemd/system/${DISPLAY_SERVICE}.service"
AGENT_UNIT="/etc/systemd/system/${AGENT_SERVICE}.service"
CLI_PATH="/usr/local/bin/vesyl-print"

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

# --- 4. config + state dirs ------------------------------------------------
echo "==> Creating /etc/vesyl-print and /var/lib/vesyl-print"
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /etc/vesyl-print
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print/queue
install -d -o "$RUN_USER" -g "$RUN_GROUP" -m 0755 /var/lib/vesyl-print/processed

if [[ ! -f /etc/vesyl-print/config.json ]]; then
    cat > /etc/vesyl-print/config.json <<'CFG'
{
  "api_base_url": "https://wms.api.staging.vesyl.com",
  "cable_url": "wss://wms.api.staging.vesyl.com/print/cable",
  "heartbeat_seconds": 30,
  "pull_interval_seconds": 5,
  "pull_jobs_enabled": false
}
CFG
    chown "$RUN_USER:$RUN_GROUP" /etc/vesyl-print/config.json
    chmod 0644 /etc/vesyl-print/config.json
    echo "   wrote /etc/vesyl-print/config.json"
else
    echo "   config.json already present"
fi

# --- 5. CLI wrapper --------------------------------------------------------
echo "==> Installing CLI: $CLI_PATH"
cat > "$CLI_PATH" <<WRAP
#!/usr/bin/env bash
exec /usr/bin/python3 "$REPO_DIR/cli.py" "\$@"
WRAP
chmod 0755 "$CLI_PATH"

# --- 6. systemd services ---------------------------------------------------
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
WorkingDirectory=$REPO_DIR
ExecStart=/usr/bin/python3 $REPO_DIR/main.py
ExecStopPost=/usr/bin/python3 $REPO_DIR/main.py --offline
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

echo "==> Installing systemd unit: $AGENT_UNIT"
cat > "$AGENT_UNIT" <<UNIT
[Unit]
Description=VESYL Print — cloud agent (heartbeat / pairing)
After=network-online.target cups.service
Wants=network-online.target

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$REPO_DIR
ExecStart=/usr/bin/python3 $REPO_DIR/agent.py
Restart=on-failure
RestartSec=5
Environment=PYTHONUNBUFFERED=1

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable "$DISPLAY_SERVICE"
systemctl enable "$AGENT_SERVICE"

echo
echo "==> Done."
echo "   Services enabled: $DISPLAY_SERVICE, $AGENT_SERVICE"
echo "   CLI: $CLI_PATH"
echo "   Pair with:  vesyl-print claim <CODE>"
echo "   Status:     vesyl-print status --check"
if [[ -e /dev/fb1 ]]; then
    echo "   /dev/fb1 present — starting services now."
    systemctl restart "$DISPLAY_SERVICE"
    systemctl restart "$AGENT_SERVICE"
else
    echo "   /dev/fb1 not present yet — REBOOT to load the display driver:"
    echo "     sudo reboot"
    systemctl restart "$AGENT_SERVICE" || true
fi
