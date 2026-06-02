#!/bin/sh
# Portable pre-remove script for deb/rpm/apk.
set -e

systemctl_available() {
    [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1
}

if systemctl_available; then
    systemctl stop tedge-dot.service >/dev/null 2>&1 || true
    systemctl disable tedge-dot.service >/dev/null 2>&1 || true
fi
