#!/usr/bin/env bash
# Install Umbra as a systemd service on Linux.
#
#   sudo ./packaging/install.sh                       # uses target/release/umbra
#   sudo ./packaging/install.sh dist/umbra-x86_64-linux
#
# Safe to run again for upgrades: it keeps your config, database and secrets.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${1:-target/release/umbra}"

say() { printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }
ask() { local a; read -r -p "$1 [y/N] " a </dev/tty || true; [[ "${a,,}" == y* ]]; }

[[ $EUID -eq 0 ]] || { echo "Run this with sudo." >&2; exit 1; }
[[ -x "$BIN" ]] || { echo "No binary at $BIN. Build it with 'cargo build --release' or pass a path." >&2; exit 1; }

say "Packages: python3 + venv, curl (alerts), traceroute, ffmpeg (Blink live view)"
if command -v apt-get >/dev/null; then
  apt-get update -qq
  apt-get install -y -qq python3 python3-venv curl traceroute
  apt-get install -y -qq ffmpeg || echo "ffmpeg not installed; Blink live view will be unavailable."
elif command -v dnf >/dev/null; then
  dnf install -y -q python3 curl traceroute
  dnf install -y -q ffmpeg || echo "ffmpeg not installed (it may need RPM Fusion); Blink live view will be unavailable."
elif command -v pacman >/dev/null; then
  pacman -S --needed --noconfirm python curl traceroute ffmpeg
else
  echo "Unknown package manager: make sure python3 (with venv), curl, traceroute and ffmpeg are installed."
fi

say "Service user and folders"
id umbra >/dev/null 2>&1 || useradd --system --home-dir /var/lib/umbra --shell /usr/sbin/nologin umbra
install -Dm755 "$BIN" /usr/local/bin/umbra
install -d -o umbra -g umbra -m 750 /etc/umbra /var/lib/umbra
install -d -m 755 /opt/umbra/helpers

say "Python helpers for Ring and Blink (their own venv in /opt/umbra/helpers/.venv)"
install -m 644 helpers/common.py helpers/ring_helper.py helpers/blink_helper.py helpers/requirements.txt /opt/umbra/helpers/
python3 -m venv /opt/umbra/helpers/.venv
/opt/umbra/helpers/.venv/bin/pip install --quiet --disable-pip-version-check --upgrade pip
/opt/umbra/helpers/.venv/bin/pip install --quiet --disable-pip-version-check -r /opt/umbra/helpers/requirements.txt
install -m 644 README.md /opt/umbra/README.md 2>/dev/null || true

say "Config"
if [[ ! -f /etc/umbra/umbra.toml ]]; then
  runuser -u umbra -- /usr/local/bin/umbra check-config --config /etc/umbra/umbra.toml >/dev/null
  echo "Created /etc/umbra/umbra.toml with defaults."
else
  runuser -u umbra -- /usr/local/bin/umbra check-config --config /etc/umbra/umbra.toml
fi

say "Secrets key (sealed with systemd-creds)"
if command -v systemd-creds >/dev/null && systemd-creds --version >/dev/null 2>&1; then
  if [[ ! -f /etc/credstore.encrypted/umbra-key ]]; then
    install -d -m 700 /etc/credstore.encrypted
    head -c 32 /dev/urandom | systemd-creds encrypt --name=umbra-key - /etc/credstore.encrypted/umbra-key
    echo "Created /etc/credstore.encrypted/umbra-key (bound to this machine's TPM or host key)."
  fi
  install -d -m 755 /etc/systemd/system/umbra.service.d
  printf '[Service]\nLoadCredentialEncrypted=umbra-key:/etc/credstore.encrypted/umbra-key\n' > /etc/systemd/system/umbra.service.d/credentials.conf
else
  echo "systemd-creds isn't available (systemd < 250). Secrets fall back to a key file readable only by the umbra user."
fi

say "systemd unit"
install -m 644 packaging/umbra.service /etc/systemd/system/umbra.service
systemctl daemon-reload

read -r lo hi <<<"$(sysctl -n net.ipv4.ping_group_range 2>/dev/null || echo '0 0')"
if (( lo > hi )); then
  echo "Unprivileged ping is off on this distro; the service uses CAP_NET_RAW instead (already granted in the unit)."
fi

if ask "Keep the laptop running with the lid closed and never suspend?"; then
  install -d -m 755 /etc/systemd/logind.conf.d
  cat > /etc/systemd/logind.conf.d/umbra-lid.conf <<'EOF'
[Login]
HandleLidSwitch=ignore
HandleLidSwitchExternalPower=ignore
HandleLidSwitchDocked=ignore
IdleAction=ignore
EOF
  systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target >/dev/null
  echo "Done. logind picks this up after a reboot (or: systemctl restart systemd-logind, which ends desktop sessions)."
fi

for t in /sys/class/power_supply/BAT*/charge_control_end_threshold; do
  [[ -e "$t" ]] && echo "This battery supports a charge limit: echo 80 > $t (see the README to make it stick)."
done

if ! runuser -u umbra -- /usr/local/bin/umbra has-password --config /etc/umbra/umbra.toml 2>/dev/null; then
  say "Choose the password for the web UI"
  runuser -u umbra -- /usr/local/bin/umbra set-password --config /etc/umbra/umbra.toml </dev/tty || echo "Skipped. Run later: sudo -u umbra umbra set-password -c /etc/umbra/umbra.toml"
fi

systemctl enable --now umbra
systemctl restart umbra
sleep 1
systemctl --no-pager --lines=5 status umbra || true

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
say "Umbra is running: http://${IP:-this-laptop}:8770"
echo "Edit /etc/umbra/umbra.toml or use Settings in the web UI. Logs: journalctl -u umbra -f"
