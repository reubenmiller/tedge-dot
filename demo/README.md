# tedge-dot demo

Try every `tedge-dot` OT connector against Docker simulators. One set of
configs ([config/](config/)) supports two workflows:

1. **[Local exploration](#local-exploration-on-your-laptop)** — poke a real
   Modbus / OPC-UA server with the `read`/`write` CLI on your laptop. No
   broker, no cloud, no install. Works on macOS.
2. **[All-protocols demo](#all-protocols-demo-on-a-linux-device)** — install
   the package on a real Linux device and run every connector under one
   systemd service, fed by one simulator compose file.

---

## Local exploration on your laptop

Build the connector once:

```sh
cargo build
```

### Modbus

The Modbus simulator (pymodbus) runs in Docker and exposes port 502 as host
**5020**.

```sh
just sim modbus     # docker compose up the simulator on 127.0.0.1:5020

# read typed values (uint16 / float32 / bool)
cargo run -- read  -c demo/config/modbus.toml -d plc1 -p temp_u16 -p level_f32 -p coil_rw

# write and read back
cargo run -- write -c demo/config/modbus.toml -d plc1 -p coil_rw  --value true
cargo run -- write -c demo/config/modbus.toml -d plc1 -p temp_u16 --value 1234
cargo run -- read  -c demo/config/modbus.toml -d plc1 -p temp_u16 --json

# a point that returns a Modbus exception -> bad quality, exit code 1
cargo run -- read  -c demo/config/modbus.toml -d plc1 -p bad_point

just sim-down modbus
```

### OPC-UA

The OPC-UA simulator (python-asyncua) runs in Docker and advertises
`opc.tcp://127.0.0.1:4840/`.

```sh
just sim opcua      # docker compose up the simulator on 127.0.0.1:4840

# read typed values (float64 / uint32 / int32 / bool)
cargo run -- read  -c demo/config/opcua.toml -d opc1 -p temperature -p count_u32 -p setpoint -p running --json

# write and read back
cargo run -- write -c demo/config/opcua.toml -d opc1 -p setpoint --value 42
cargo run -- write -c demo/config/opcua.toml -d opc1 -p running  --value true
cargo run -- read  -c demo/config/opcua.toml -d opc1 -p setpoint

# a node that returns a Bad status -> bad quality, exit code 1
cargo run -- read  -c demo/config/opcua.toml -d opc1 -p bad_point

just sim-down opcua
```

The `Client is missing its application instance certificate` logs that
async-opcua emits for `security_policy = "None"` are silenced by default (no
client cert is needed for unencrypted, anonymous access). Set `RUST_LOG=info`
to see the full OPC-UA client logging.

### Full MQTT end-to-end stacks

To exercise the complete pipeline (connector + broker + simulator, publishing
samples over MQTT) use the Docker-based harnesses instead:

```sh
just test-e2e modbus   # bring stack up, run Robot suite, tear down
just test-e2e opcua    # same, against the asyncua simulator
just e2e-up modbus     # stack up for manual inspection
just e2e-down modbus
```

---

## All-protocols demo on a Linux device

One compose file ([docker-compose.yaml](docker-compose.yaml)) runs all the
simulators; the installed package runs all the connectors under **one**
systemd service.

```
┌──────────────────────────── Linux device ────────────────────────────┐
│                                                                       │
│  docker compose (simulators)              tedge-dot.service           │
│  ┌─────────────────────────┐              (one systemd unit)          │
│  │ modbus-sim    tcp :5020  │◄──────────── tedge-dot (modbus)         │
│  │ opcua-sim     tcp :4840  │◄──────────── tedge-dot (opcua)          │
│  │ canbus-sim    vcan0      │◄── socketcan─ tedge-dot (canbus)        │
│  │ canopen-sim   vcan0      │◄── socketcan─ tedge-dot (canopen)       │
│  │ profibus-sim  tcp :9200  │◄──── tcp ──── tedge-dot (profibus)      │
│  └─────────────────────────┘                     │                    │
│                                                   ▼                    │
│                              mosquitto :1883 (thin-edge.io broker)     │
└───────────────────────────────────────────────────────────────────────┘
```

The connectors publish samples to the thin-edge.io MQTT broker
(`127.0.0.1:1883`), so thin-edge.io must already be installed on the device.

> **PROFIBUS caveat:** the released package is built without the `profibus`
> cargo feature (its serial dependency does not cross-compile yet). To include
> the PROFIBUS connector in the demo, build the binary from source on the
> device (`cargo build --release --features profibus`) and copy
> [config/profibus.toml](config/profibus.toml) into `/etc/tedge/plugins/ot/` —
> the connector speaks serial-over-TCP to the simulator directly
> (`port = "tcp://127.0.0.1:9200"`). The other four protocols work out of the
> box.

### Requirements

- A real Linux host (not macOS Docker Desktop — its LinuxKit kernel has no
  CAN/vcan support). A Linux VM is fine.
- Docker Engine with privileged containers and host networking.
- Kernel CAN support for the canbus/canopen sims:
  `CONFIG_CAN`, `CONFIG_CAN_RAW`, `CONFIG_CAN_VCAN` (`sudo modprobe vcan`).
- thin-edge.io installed and running (mosquitto on `127.0.0.1:1883`).

### 1. Build the package

Cross-compiles the single `tedge-dot` binary and produces a
`.deb`/`.rpm`/`.apk`, entirely inside Docker:

```sh
just test-data-docker amd64      # or: arm64
# output: ../tests/data/*_linux_amd64.deb
```

Or, with a host Rust + goreleaser toolchain:

```sh
just build                       # writes packages to dist/
```

### 2. Install the package on the device

```sh
sudo apt install ./tedge-dot_*_linux_amd64.deb     # deb
# sudo dnf install ./tedge-dot_*_linux_amd64.rpm   # rpm
# sudo apk add --allow-untrusted tedge-dot_*.apk   # apk
```

Installing the package:

- drops one *empty* default config per protocol into `/etc/tedge/plugins/ot/`
  (`modbus.toml`, `opcua.toml`, `canbus.toml`, `canopen.toml`) — no devices
  are configured, so the service starts and idles;
- ships the demo configs from [config/](config/) (pre-wired to the simulators)
  in `/usr/share/tedge-dot/demo/`, plus the CAN database at
  `/usr/share/tedge-dot/demo/can/test.dbc`;
- installs and starts **one** service: `tedge-dot.service`, which runs every
  configured connector inside a single `tedge-dot` process.

### 3. Enable the demo configs

Replace the empty defaults with the demo configs that point at the simulators:

```sh
sudo cp /usr/share/tedge-dot/demo/*.toml /etc/tedge/plugins/ot/
```

### 4. Start the simulators

From a checkout of this repo on the device:

```sh
just demo-sims-up
# equivalently:
# docker compose -f demo/docker-compose.yaml up -d --build
```

Restart the connector service so every connector picks up its simulator:

```sh
sudo systemctl restart tedge-dot.service
```

### 5. Watch it work

```sh
# One service, all connectors:
systemctl status tedge-dot.service
journalctl -u tedge-dot.service -f

# Live samples on the thin-edge.io broker:
tedge mqtt sub 'te/+/+/+/+/m/+'
```

You should see telemetry from all four packaged protocols flowing in (five
with a source-built PROFIBUS binary, see the caveat above).

### How "one systemd service" runs every protocol

`tedge-dot run /etc/tedge/plugins/ot` (the service's `ExecStart`) discovers
every `*.toml` in the directory and runs one connector per config **inside a
single process**: each gets its own protocol module and SDK runtime instance
(own MQTT session, health topic and capability descriptor), supervised by an
in-process restart loop. A crashing or misconfigured connector is restarted
with a backoff without disturbing the others, and its config file is re-read
on every attempt — so fixing a bad config is picked up automatically.
`systemctl stop tedge-dot` shuts every connector down cleanly (each publishes
its final health status before exiting).

The `vcan0` interface the CAN connectors need is created by the canbus/canopen
simulator containers themselves.

To run only some protocols, remove the configs you don't want from
`/etc/tedge/plugins/ot/` and restart the service.

### Why PROFIBUS runs over TCP

PTY devices are per-container-namespace and can't be shared with the host, so a
native host connector cannot open a PTY created inside the simulator container.
The simulator therefore exposes its slave serial line over TCP (`:9200`), and
the connector's `tcp://` transport speaks to it directly — the same transport
covers real serial-over-TCP device servers (RS-485 ⇄ TCP gateways). For real
RS-485 hardware, set `port` to the serial device instead (e.g. `/dev/ttyUSB0`).

### Tunables

The service honours these environment variables (set them via a systemd
drop-in, e.g. `systemctl edit tedge-dot.service`):

| Variable                  | Default | Purpose                              |
|---------------------------|---------|--------------------------------------|
| `TEDGE_DOT_RESTART_DELAY` | `5`     | Per-connector restart backoff (sec)  |
| `RUST_LOG`                | per-config `log_level` | Log filter override   |

The config directory is the `ExecStart` argument in
[`tedge-dot.service`](../packaging/tedge-dot.service).

### Teardown

```sh
just demo-sims-down
sudo systemctl stop tedge-dot.service
```
