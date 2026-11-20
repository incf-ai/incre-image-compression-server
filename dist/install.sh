#!/bin/sh
# Installs incre-image-compression-server from this tarball: the binaries, a system user, a token and two
# systemd services. Run it again to upgrade; it keeps your token and settings.
#
#   sudo ./install.sh            install or upgrade, then start
#   PREFIX=/opt/incre-image-compression-server sudo -E ./install.sh
set -eu

PREFIX=${PREFIX:-/usr/local}
ETC=/etc/incre-image-compression-server
UNITS=/etc/systemd/system
HERE=$(cd "$(dirname "$0")" && pwd)

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: run it as root (sudo ./install.sh)" >&2
    exit 1
fi
command -v systemctl >/dev/null || { echo "install.sh: needs systemd" >&2; exit 1; }

upgrade=no
systemctl is-active --quiet incre-image-compression-server.service && upgrade=yes

echo "binaries -> $PREFIX/bin"
install -d "$PREFIX/bin"
install -m 0755 "$HERE/bin/incre-image-compression-server" "$PREFIX/bin/incre-image-compression-server"
install -m 0755 "$HERE/bin/incre-compress" "$PREFIX/bin/incre-compress"

if ! id incre-image-compression-server >/dev/null 2>&1; then
    echo "user incre-image-compression-server"
    useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin incre-image-compression-server
fi

install -d -m 0750 -o root -g incre-image-compression-server "$ETC"
if [ ! -s "$ETC/token" ]; then
    echo "token -> $ETC/token"
    umask 027
    "$PREFIX/bin/incre-image-compression-server" token > "$ETC/token"
    chown root:incre-image-compression-server "$ETC/token"
    chmod 0640 "$ETC/token"
fi
if [ ! -e "$ETC/incre-image-compression-server.env" ]; then
    install -m 0640 -o root -g incre-image-compression-server "$HERE/systemd/incre-image-compression-server.env" "$ETC/incre-image-compression-server.env"
fi

for unit in incre-image-compression-server.service incre-image-compression-worker.service; do
    sed "s|/usr/local/bin/|$PREFIX/bin/|g" "$HERE/systemd/$unit" > "$UNITS/$unit"
    chmod 0644 "$UNITS/$unit"
done

# CPUWeight=idle (the cgroup's cpu.idle) came with systemd 252; older ones get the
# lowest plain weight instead. SCHED_IDLE and nice 19 apply either way.
sdver=$(systemctl --version | awk 'NR==1 {print $2}' | tr -cd '0-9')
dropin="$UNITS/incre-image-compression-worker.service.d"
if [ "${sdver:-0}" -lt 252 ]; then
    echo "systemd $sdver: CPUWeight=1 instead of idle"
    install -d "$dropin"
    printf '[Service]\nCPUWeight=\nCPUWeight=1\n' > "$dropin/10-old-systemd.conf"
else
    rm -f "$dropin/10-old-systemd.conf"
    rmdir "$dropin" 2>/dev/null || true
fi

systemctl daemon-reload
systemctl enable incre-image-compression-server.service incre-image-compression-worker.service >/dev/null
if [ "$upgrade" = yes ]; then
    echo "restarting (queued jobs are dropped; clients resubmit them)"
    systemctl restart incre-image-compression-server.service incre-image-compression-worker.service
else
    systemctl start incre-image-compression-server.service incre-image-compression-worker.service
fi

port=8790
listen=$(sed -n 's/^INCRE_COMPRESS_LISTEN=//p' "$ETC/incre-image-compression-server.env" | tail -n 1)
[ -n "$listen" ] && port=${listen##*:}
host=$(hostname)
cat <<MSG

incre-image-compression-server $("$PREFIX/bin/incre-image-compression-server" version) is running.

  URL:       http://$host:$port
  token:     $ETC/token (copy it to each client; INCRE_COMPRESS_TOKEN or --token-file)
  settings:  $ETC/incre-image-compression-server.env
  logs:      journalctl -u incre-image-compression-server -u incre-image-compression-worker -f
  check:     INCRE_COMPRESS_URL=http://$host:$port INCRE_COMPRESS_TOKEN_FILE=$ETC/token incre-compress status

The token travels in the clear: open port $port to your LAN only, e.g.
  sudo ufw allow from 192.168.0.0/16 to any port $port proto tcp
MSG
