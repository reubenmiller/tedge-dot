#!/bin/sh
# Portable post-install script for deb/rpm/apk.
set -e

systemctl_available() {
    [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1
}

tedge refresh-bridges || true

if systemctl_available; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    systemctl enable tedge-dot.service >/dev/null 2>&1 || true
    systemctl restart tedge-dot.service >/dev/null 2>&1 || true
fi
