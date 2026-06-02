#!/bin/sh
# Portable post-remove script for deb/rpm/apk.
set -e

systemctl_available() {
    [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1
}

if systemctl_available; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

tedge config remove c8y.smartrest.templates modbus || true
tedge refresh-bridges || true
