#!/bin/sh
# Bring up a virtual CAN interface vcan0. Linux-only; needs root (sudo).
set -eu

modprobe vcan
ip link show vcan0 >/dev/null 2>&1 || ip link add dev vcan0 type vcan
ip link set up vcan0
echo "vcan0 is up"
