#!/usr/bin/env bash
#
# One-time first-boot provisioning for a fresh Raspberry Pi print appliance.
#
#   1. Ensure SSH host keys exist
#   2. Generate UUID → /etc/appliance-id
#   3. Hostname → VESYL-PRINT-<last 6 hex of UUID>
#   4. Clone goodtft/LCD-show and run MHS35-show
#   5. Reboot
#
# Usage (as root on a fresh Pi with network):
#   curl -fsSL … | sudo bash
#   # or copy the script over:
#   sudo ./bootstrap-fresh-pi.sh
#
# Safe to re-run only if you understand it may re-apply LCD overlay config.
# Appliance ID is kept if already present.
#
set -euo pipefail

if [[ ${EUID:-0} -ne 0 ]]; then
  echo "This script must run as root (try: sudo $0)" >&2
  exit 1
fi

APPLIANCE_ID_FILE=/etc/appliance-id
LCD_REPO_URL="${LCD_REPO_URL:-https://github.com/goodtft/LCD-show.git}"
LCD_CLONE_DIR="${LCD_CLONE_DIR:-/tmp/LCD-show}"

log() { echo "==> $*"; }

# --- 1. SSH host keys -------------------------------------------------------
log "Ensuring SSH host keys"
if [[ ! -f /etc/ssh/ssh_host_rsa_key && ! -f /etc/ssh/ssh_host_ed25519_key ]]; then
  # Generate the standard host key set (rsa, ecdsa, ed25519 as configured)
  ssh-keygen -A
  log "Generated SSH host keys via ssh-keygen -A"
else
  # Still fill any missing types
  ssh-keygen -A
  log "SSH host keys present (filled any missing types)"
fi
# Ensure sshd is enabled for post-reboot access
if systemctl list-unit-files ssh.service &>/dev/null; then
  systemctl enable ssh.service 2>/dev/null || systemctl enable sshd.service 2>/dev/null || true
elif systemctl list-unit-files sshd.service &>/dev/null; then
  systemctl enable sshd.service 2>/dev/null || true
fi

# --- 2. Appliance UUID ------------------------------------------------------
if [[ -f "$APPLIANCE_ID_FILE" ]]; then
  APPLIANCE_ID="$(tr -d '[:space:]' <"$APPLIANCE_ID_FILE")"
  if [[ -z "$APPLIANCE_ID" ]]; then
    echo "ERROR: $APPLIANCE_ID_FILE exists but is empty" >&2
    exit 1
  fi
  log "Using existing appliance id: $APPLIANCE_ID"
else
  if command -v uuidgen >/dev/null 2>&1; then
    APPLIANCE_ID="$(uuidgen)"
  elif [[ -r /proc/sys/kernel/random/uuid ]]; then
    APPLIANCE_ID="$(cat /proc/sys/kernel/random/uuid)"
  else
    APPLIANCE_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
  fi
  # Normalize to lowercase UUID with hyphens
  APPLIANCE_ID="$(echo "$APPLIANCE_ID" | tr '[:upper:]' '[:lower:]')"
  printf '%s\n' "$APPLIANCE_ID" >"$APPLIANCE_ID_FILE"
  chmod 0644 "$APPLIANCE_ID_FILE"
  log "Wrote $APPLIANCE_ID_FILE = $APPLIANCE_ID"
fi

# --- 3. Hostname from last 6 hex of UUID ------------------------------------
# Strip hyphens, take last 6 chars, uppercase (e.g. VESYL-PRINT-C580F1)
UUID_HEX="$(echo "$APPLIANCE_ID" | tr -d '-' | tr '[:lower:]' '[:upper:]')"
SUFFIX="${UUID_HEX: -6}"
if [[ ${#SUFFIX} -ne 6 ]]; then
  echo "ERROR: could not derive 6-char suffix from appliance id" >&2
  exit 1
fi
NEW_HOSTNAME="VESYL-PRINT-${SUFFIX}"
log "Setting hostname to $NEW_HOSTNAME"

if command -v hostnamectl >/dev/null 2>&1; then
  hostnamectl set-hostname "$NEW_HOSTNAME"
else
  echo "$NEW_HOSTNAME" >/etc/hostname
  hostname "$NEW_HOSTNAME" || true
fi

# Keep 127.0.1.1 mapping in sync (Debian/Raspberry Pi OS style)
if grep -qE '^127\.0\.1\.1\b' /etc/hosts 2>/dev/null; then
  sed -i -E "s/^127\\.0\\.1\\.1.*/127.0.1.1\t${NEW_HOSTNAME}/" /etc/hosts
else
  printf '127.0.1.1\t%s\n' "$NEW_HOSTNAME" >>/etc/hosts
fi

# --- 4. LCD driver (goodtft LCD-show / MHS35) --------------------------------
log "Installing git if needed"
if ! command -v git >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -y
  apt-get install -y git
fi

log "Cloning $LCD_REPO_URL → $LCD_CLONE_DIR"
rm -rf "$LCD_CLONE_DIR"
git clone --depth 1 "$LCD_REPO_URL" "$LCD_CLONE_DIR"

log "Running MHS35-show (LCD overlay / calibration from vendor script)"
cd "$LCD_CLONE_DIR"
chmod +x MHS35-show
# Vendor script often reboots on its own; we still reboot below if we return.
# Disable interactive prompts if any env vars are respected by forks.
./MHS35-show

# --- 5. Reboot --------------------------------------------------------------
log "Rebooting into new hostname + display config…"
# Brief delay so the log line flushes to serial/SSH clients
sync
sleep 2
systemctl reboot || reboot
