#!/bin/sh
# Removes incre-image-compression-server's services and binaries. --purge also removes /etc/incre-image-compression-server (the token
# and settings) and the incre-image-compression-server user.
set -eu

PREFIX=${PREFIX:-/usr/local}
UNITS=/etc/systemd/system

if [ "$(id -u)" -ne 0 ]; then
    echo "uninstall.sh: run it as root" >&2
    exit 1
fi

systemctl disable --now incre-image-compression-worker.service incre-image-compression-server.service 2>/dev/null || true
rm -f "$UNITS/incre-image-compression-server.service" "$UNITS/incre-image-compression-worker.service"
rm -rf "$UNITS/incre-image-compression-worker.service.d"
systemctl daemon-reload
rm -f "$PREFIX/bin/incre-image-compression-server" "$PREFIX/bin/incre-compress"
rm -rf /run/incre-image-compression-server

if [ "${1:-}" = "--purge" ]; then
    rm -rf /etc/incre-image-compression-server
    userdel incre-image-compression-server 2>/dev/null || true
    echo "removed, with the token, settings and user"
else
    echo "removed; /etc/incre-image-compression-server and the incre-image-compression-server user stay (--purge removes them)"
fi
