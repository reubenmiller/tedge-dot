#!/usr/bin/env python3
"""Patch the vendored Open-SAE-J1939 Transport Protocol data-transfer handler to
surface every reassembled multi-packet PGN through a weak `on_raw_pgn` sink,
before the library's built-in PGN switch consumes/clears the buffer.

Applied at FetchContent populate time (see poc-c/CMakeLists.txt). Newline-agnostic
(the upstream file is CRLF) and idempotent (a no-op if already patched).

Usage: apply_tp_capture.py <open-sae-j1939 source root>
"""
import os
import sys

root = sys.argv[1]
path = os.path.join(
    root, "Src", "SAE_J1939", "SAE_J1939-21_Transport_Layer",
    "Transport_Protocol_Data_Transfer.c")

data = open(path, "rb").read()
if b"on_raw_pgn" in data:
    print("apply_tp_capture: already patched")
    sys.exit(0)

nl = b"\r\n" if b"\r\n" in data else b"\n"

inc = b'#include "../SAE_J1939-71_Application_Layer/Application_Layer.h"' + nl
decl = nl.join([
    b"",
    b"/* tedge-dot: weak generic-capture sink for reassembled multi-packet PGNs.",
    b" * Resolved by connector_j1939.c / the Step-0 harness when linked; NULL",
    b" * (call skipped) otherwise. */",
    b"extern void on_raw_pgn(uint8_t sa, uint32_t pgn, const uint8_t *data, uint32_t len) __attribute__((weak));",
    b"",
]) + nl

ack = b"\t/* Send an end of message ACK back */"
call = nl.join([
    b"\t/* tedge-dot: surface every reassembled multi-packet payload generically,",
    b"\t * before the built-in PGN switch consumes/clears it. */",
    b"\tif (on_raw_pgn)",
    b"\t\ton_raw_pgn(SA, PGN, complete_data, total_message_size);",
    b"",
    b"",
]) + ack

for anchor in (inc, ack):
    if data.count(anchor) != 1:
        sys.exit(f"apply_tp_capture: anchor not found uniquely: {anchor!r}")

data = data.replace(inc, inc + decl, 1)
data = data.replace(ack, call, 1)
open(path, "wb").write(data)
print("apply_tp_capture: patched", path)
