# j1939 connector — Step-0 validation

This is the first step of the `j1939` connector (built on the vendored MIT
[Open-SAE-J1939](https://github.com/DanielMartensson/Open-SAE-J1939) library). Before the
vtable connector (`connector_j1939.c`) is written, this validates the one integration
risk.

**Question it answers:** can the library hand our connector
`(source_address, PGN, payload, len)` for **arbitrary** PGNs — the thing a
config/DBC-driven `j1939` connector needs — or only for its predefined
application-layer structs?

## Answer (verified on real hardware — rpi4 aarch64 + vcan)

- **Single-frame PGNs (≤8 bytes): exposed. ✅** Under the `SOCKETCAN` platform the library
  reads frames **directly** via `socketcan_receive` — `CAN_Read_Message` does *not* invoke a
  registered read callback (that path is `INTERNAL_CALLBACK`-only). Each new frame is stored
  in `j1939.ID` / `j1939.data`, and `Open_SAE_J1939_Listen_For_Messages` returns
  non-`RX_MSG_NONE`; we capture the raw frame straight from the struct. **No callbacks to
  register, no library patch needed.** EEC1 (PGN 61444) → decoded **1000.0 rpm** on the Pi.
  - ⚠️ Gotcha proven wrong the hard way: `RX_MSG_UNKNOWN` is *also* Listen's idle/default
    return, so it is **not** the "a frame arrived" signal. Use `rx != RX_MSG_NONE`.
- **Multi-packet PGNs (>8 bytes, Transport Protocol): exposed via a small patch (Phase 2). ✅**
  `Transport_Protocol_Data_Transfer.c` reassembles into `j1939->from_other_ecu_tp_dt.data[]`,
  then `switch (PGN)` routes to predefined handlers with **no `default:` case**, then
  `memset`s the buffer. The patch adds a generic `on_raw_pgn` capture right after
  reassembly (see [The patch](#the-patch)). Verified on the Pi: a BAM message (PGN 65280,
  12 bytes) surfaces its full reassembled payload.

## Platform

SocketCAN / `vcan` / `cansend` are **Linux-only**. This will not run on the macOS dev
box. It has been run on a Linux host (rpi4) to clear the risk; re-run it there or via the
provided `Dockerfile` (see caveat below) — Docker Desktop's VM lacks the `vcan` module, so
its use is compile-only on macOS.

## Not a permanent CI job

This is a run-once library probe, not connector coverage — so it has no CI job of its own.
Once `connector_j1939.c` exists, j1939 plugs into the **existing** mechanisms exactly like
canbus/canopen (see [conformance-suite.md](../../../doc/conformance/conformance-suite.md)):

- **Decode correctness** → Layer-2 golden vectors in `crates/sdk/conformance/vectors.json`
  (SPN extraction is the same `decode_primitive` bit-math as a canbus signal; ×0.125 is the
  `transform` layer) plus a `connectors/j1939/conformance.toml` manifest, run via
  `just conformance j1939`. This replaces the ad-hoc "1000.0 rpm" check below.
- **Live-bus behaviour** → the **e2e suite** (Dockerised vcan), which is where CAN
  behavioural coverage lives — conformance Layer 3 is deliberately skipped for CAN
  (vcan is Linux-only).

## Connector (Phase 1)

[`connector_j1939.c`](connector_j1939.c) implements the SDK vtable, modelled on the
canopen connector (connection-level shared bus, since Open-SAE-J1939's SocketCAN backend
is a single-socket stack) and reusing the canbus DBC parser for SPN bit layout. It is
built by the main PoC CMake behind the Linux-only `TDOT_J1939` option, which fetches
Open-SAE-J1939 via `FetchContent`. Demo config: [`demo/config/j1939.toml`](../../../demo/config/j1939.toml).

**Status:** compiles + links (Docker `debian:bookworm`, x86_64) and the capture path is
**validated on real hardware** (rpi4 aarch64 / Debian trixie / real `vcan`: EEC1 → 1000.0
rpm). Phase 1 = passive read of single-frame broadcast PGNs, no library patch, read-only.
**Phase 2 (done):** multi-packet (Transport Protocol) PGNs are captured via the
auto-applied TP.DT patch, and the connector decodes SPNs at any byte offset in payloads up
to `J_MAX_PGN_BYTES` (256). Validated on the Pi (BAM PGN 65280).

**Phase 2b/2c (next):** on-request PGNs (needs a configured source address to send Request
PGN 59904 + address claiming) and DM1/DM2 diagnostics (the library already decodes these
into its struct — surfacing DTCs as tedge samples/events is the remaining work).

**Build note:** build with compiler extensions on (CMake default, `gnu11`). The vendored
library's SocketCAN backend relies on `_DEFAULT_SOURCE` (uses `usleep`, `struct timeval`,
`ifreq`); a strict `-std=c11` build fails on newer glibc (trixie) — the CMake build is fine.

## Run it

```sh
cd poc-c/connectors/j1939
./scripts/fetch-lib.sh                 # clones Open-SAE-J1939 into vendor/ (gitignored)
sudo ./scripts/setup-vcan.sh           # modprobe vcan + bring up vcan0
cmake -B build -S . -G Ninja           # SOCKETCAN platform selected in CMakeLists.txt
cmake --build build

./build/harness &                      # listens on vcan0, prints captured (SA, PGN, payload)
./scripts/send-vectors.sh              # injects the test frames with cansend
```

### Docker (from macOS)

```sh
docker build -t j1939-step0 poc-c/connectors/j1939
docker run --rm -it --cap-add=NET_ADMIN j1939-step0
```

> **Caveat:** `vcan` is a host-kernel module. Docker Desktop's Linux VM must have the
> `vcan` module available for `modprobe vcan` to succeed inside the container. If it
> doesn't, run it on a real Linux host / CI runner instead. This is the same constraint
> that keeps the canbus/canopen connectors Linux-gated.

## Test vectors

| Case | Frames (`cansend vcan0 …`) | Expected |
|---|---|---|
| **EEC1 single frame**, 1000 rpm. SPN 190 (engine speed) is bytes 4–5, 0.125 rpm/bit → raw 8000 = `40 1F` little-endian | `0CF00400#FFFFFF401FFFFFFF` | Captured **unpatched**; harness decodes **1000.0 rpm** |
| **Proprietary BAM**, 12-byte payload over 2 packets, PGN 65280 (`0xFF00`) | `1CECFF00#200C0002FF00FF00` then `1CEBFF00#01AABBCCDDEEFF00` and `1CEBFF00#0201020304050000` | **Dropped** unpatched; **captured** after the patch, payload = `AA BB CC DD EE FF 00 01 02 03 04 05` |

BAM TP.CM byte layout: `20`=BAM control, `0C 00`=total size 12 (LE), `02`=2 packets,
`FF`=reserved, `00 FF 00`=PGN 65280 (LE). TP.DT frames are `<seq>` + 7 payload bytes.

## The patch (multi-packet capture)

To capture **multi-packet** PGNs, the reassembled payload must be surfaced before the
library's built-in `switch (PGN)` consumes/clears it. This is automated:
[`patches/apply_tp_capture.py`](patches/apply_tp_capture.py) inserts a weak `on_raw_pgn`
call right after reassembly in
`Src/SAE_J1939/SAE_J1939-21_Transport_Layer/Transport_Protocol_Data_Transfer.c`, and CMake
runs it as the `FetchContent` `PATCH_COMMAND` (see [poc-c/CMakeLists.txt](../../CMakeLists.txt)).
The patcher is newline-agnostic (upstream is CRLF) and idempotent.

```c
/* inserted right after complete_data[] is built, before the built-in switch */
if (on_raw_pgn)
    on_raw_pgn(SA, PGN, complete_data, total_message_size);
```

`on_raw_pgn` is a **weak** extern: it resolves to the connector's PGN-cache sink (or the
harness's) when linked, and is a skipped no-op if the library is built standalone.

## Decision gate

- ✅ EEC1 captured + decodes to **1000.0 rpm** unpatched → **Phase 1 viable, library
  as-is**.
- ✅ BAM payload captured after the one-line patch → **Phase 2/3 viable** with a small,
  well-located documented fork.
- ❌ Patch invasive, or `from_other_ecu_tp_dt.data[]` / `MAX_TP_DT` too small for real
  PGNs → revisit the kernel `can-j1939` fallback.
