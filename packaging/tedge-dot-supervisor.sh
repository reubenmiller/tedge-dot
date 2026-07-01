#!/bin/sh
# tedge-dot supervisor: run every configured OT connector under a single
# systemd service (tedge-dot.service).
#
# The tedge-dot binary runs exactly one protocol per process (the protocol is
# selected by `connector.protocol` in its config file).  To demo all OT
# connectors from one service, this supervisor:
#
#   1. Prepares the runtime environment shared with the simulators:
#        * ensures the vcan0 virtual CAN interface exists (canbus + canopen)
#        * starts a socat serial<->TCP bridge for PROFIBUS, materialising the
#          containerised slave's serial line as /dev/ttyPROFIBUS0
#   2. Launches one `tedge-dot <config>` process per *.toml in the config dir,
#      each wrapped in a restart loop so a single connector crash doesn't take
#      the others down.
#
# systemd owns the process tree (KillMode=control-group), so on stop every
# child — connectors and bridges alike — is cleaned up automatically.
#
# Tunables (override via systemd Environment= or the shell environment):
#   TEDGE_DOT_CONF_DIR      config directory          (default /etc/tedge/plugins/ot)
#   TEDGE_DOT_BIN           connector binary          (default /usr/bin/tedge-dot)
#   TEDGE_DOT_PROFIBUS_TCP  sim TCP endpoint          (default 127.0.0.1:9200)
#   TEDGE_DOT_PROFIBUS_PTY  host serial device to make(default /dev/ttyPROFIBUS0)
#   TEDGE_DOT_VCAN_IF       virtual CAN interface     (default vcan0)
#   TEDGE_DOT_RESTART_DELAY restart backoff seconds   (default 5)
set -u

CONF_DIR="${TEDGE_DOT_CONF_DIR:-/etc/tedge/plugins/ot}"
BIN="${TEDGE_DOT_BIN:-/usr/bin/tedge-dot}"
PROFIBUS_TCP="${TEDGE_DOT_PROFIBUS_TCP:-127.0.0.1:9200}"
PROFIBUS_PTY="${TEDGE_DOT_PROFIBUS_PTY:-/dev/ttyPROFIBUS0}"
VCAN_IF="${TEDGE_DOT_VCAN_IF:-vcan0}"
RESTART_DELAY="${TEDGE_DOT_RESTART_DELAY:-5}"

log() { echo "[tedge-dot-supervisor] $*"; }

# --- environment setup ------------------------------------------------------

setup_vcan() {
    # Idempotent: the canbus/canopen simulator containers may also create it.
    command -v modprobe >/dev/null 2>&1 && modprobe vcan 2>/dev/null || true
    if command -v ip >/dev/null 2>&1; then
        if ! ip link show "$VCAN_IF" >/dev/null 2>&1; then
            ip link add dev "$VCAN_IF" type vcan 2>/dev/null \
                && log "created virtual CAN interface $VCAN_IF" \
                || log "could not create $VCAN_IF (need root + vcan kernel module)"
        fi
        ip link set up "$VCAN_IF" 2>/dev/null || true
    else
        log "iproute2 (ip) not found; cannot set up $VCAN_IF"
    fi
}

start_profibus_bridge() {
    if ! command -v socat >/dev/null 2>&1; then
        log "socat not found; PROFIBUS serial<->tcp bridge unavailable"
        return
    fi
    (
        while true; do
            socat "TCP:${PROFIBUS_TCP}" \
                  "pty,rawer,echo=0,b19200,link=${PROFIBUS_PTY}" 2>/dev/null || true
            sleep "$RESTART_DELAY"
        done
    ) &
    log "PROFIBUS bridge ${PROFIBUS_PTY} <-> tcp:${PROFIBUS_TCP} (pid $!)"
}

# --- connector supervision --------------------------------------------------

run_connector() {
    cfg="$1"
    name=$(basename "$cfg" .toml)
    (
        while true; do
            log "starting connector '$name' ($cfg)"
            "$BIN" "$cfg"
            rc=$?
            log "connector '$name' exited (rc=$rc); restarting in ${RESTART_DELAY}s"
            sleep "$RESTART_DELAY"
        done
    ) &
    log "supervising connector '$name' (pid $!)"
}

config_uses_profibus() {
    for cfg in "$CONF_DIR"/*.toml; do
        [ -e "$cfg" ] || continue
        if grep -Eq '^[[:space:]]*protocol[[:space:]]*=[[:space:]]*"profibus"' "$cfg"; then
            return 0
        fi
    done
    return 1
}

config_uses_can() {
    for cfg in "$CONF_DIR"/*.toml; do
        [ -e "$cfg" ] || continue
        if grep -Eq '^[[:space:]]*protocol[[:space:]]*=[[:space:]]*"can(bus|open)"' "$cfg"; then
            return 0
        fi
    done
    return 1
}

# --- main -------------------------------------------------------------------

trap 'log "received stop signal"; kill 0 2>/dev/null; exit 0' TERM INT

log "config dir: $CONF_DIR"

if config_uses_can; then
    setup_vcan
fi

if config_uses_profibus; then
    start_profibus_bridge
fi

started=0
for cfg in "$CONF_DIR"/*.toml; do
    [ -e "$cfg" ] || continue
    run_connector "$cfg"
    started=$((started + 1))
done

if [ "$started" -eq 0 ]; then
    log "no connector configs found in $CONF_DIR — nothing to run"
    # Stay alive so the service is 'active' and picks up configs on restart.
    while true; do sleep 3600; done
fi

log "$started connector(s) running"
wait
