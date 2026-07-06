#!/bin/sh
# Inject the spike test vectors onto vcan0. Requires can-utils (cansend).
# Run ./harness in another shell first.
set -eu

IF="${1:-vcan0}"

echo "-> EEC1 single frame, engine speed 1000 rpm (PGN 61444)"
cansend "$IF" 0CF00400#FFFFFF401FFFFFFF

sleep 1

echo "-> Proprietary BAM, 12 bytes over 2 packets (PGN 65280)"
cansend "$IF" 1CECFF00#200C0002FF00FF00   # TP.CM BAM: size=12, packets=2, PGN=65280
cansend "$IF" 1CEBFF00#01AABBCCDDEEFF00   # TP.DT seq 1: 7 payload bytes
cansend "$IF" 1CEBFF00#0201020304050000   # TP.DT seq 2: last 5 payload bytes + pad

echo "done"
