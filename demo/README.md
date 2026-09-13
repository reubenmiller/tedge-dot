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

Build the connector once, and say where the demo point lists are:

```sh
cargo build --manifest-path impl/rust/Cargo.toml
export TEDGE_DOT_POINT_LIBRARY_PATH=demo/points.d
```

> **Why the export.** Each config in [config/](config/) holds only what is
> per-instance — the address of the simulator — and gets its points from a
> **point library** in [points.d/](points.d/) (contract §3.4), one per
> simulator. A package installs those libraries to
> `/usr/share/tedge-dot/points.d/`, where the connector finds them with no
> configuration at all; a checkout has not installed them, so the search path
> has to be pointed at the copies in this repo. Every command below assumes
> the export above.
>
> That is also the quickest way to experiment with a device of your own: give
> a `[[device]]` its address and `points_from = ["demo-sim"]`, and you have a
> working config without typing a single point definition.

### Modbus

The Modbus simulator (pymodbus) runs in Docker and exposes port 502 as host
**5020**.

```sh
just sim modbus     # docker compose up the simulator on 127.0.0.1:5020

# read typed values (uint16 / float32 / bool)
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml -d plc1 -p temp_u16 -p level_f32 -p coil_rw

# read everything: -d/-p default to '*' (all devices, all readable points)
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml

# keep polling matching points (config interval; --interval/--count override)
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml -p 'temp_*' --poll
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml --interval 500ms --count 5

# write and read back (a wildcard -p writes the value to every matching writable point)
cargo run --manifest-path impl/rust/Cargo.toml -- write -c demo/config/modbus.toml -d plc1 -p coil_rw  --value true
cargo run --manifest-path impl/rust/Cargo.toml -- write -c demo/config/modbus.toml -d plc1 -p temp_u16 --value 1234
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml -d plc1 -p temp_u16 --json

# a point that returns a Modbus exception -> bad quality, exit code 1
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/modbus.toml -d plc1 -p bad_point

# run the connector without a broker: sample envelopes as JSON lines on stdout
cargo run --manifest-path impl/rust/Cargo.toml -- run   -c demo/config/modbus.toml --output stdout --duration 10s

just sim-down modbus
```

### OPC-UA

The OPC-UA simulator (python-asyncua) runs in Docker and advertises
`opc.tcp://127.0.0.1:4840/`.

```sh
just sim opcua      # docker compose up the simulator on 127.0.0.1:4840

# read typed values (float64 / uint32 / int32 / bool)
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/opcua.toml -d opc1 -p temperature -p count_u32 -p setpoint -p running --json

# write and read back
cargo run --manifest-path impl/rust/Cargo.toml -- write -c demo/config/opcua.toml -d opc1 -p setpoint --value 42
cargo run --manifest-path impl/rust/Cargo.toml -- write -c demo/config/opcua.toml -d opc1 -p running  --value true
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/opcua.toml -d opc1 -p setpoint

# a node that returns a Bad status -> bad quality, exit code 1
cargo run --manifest-path impl/rust/Cargo.toml -- read  -c demo/config/opcua.toml -d opc1 -p bad_point

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
> device (`cargo build --manifest-path impl/rust/Cargo.toml --release --features profibus`) and copy
> [config/profibus.toml](config/profibus.toml) into `/etc/tedge/plugins/ot/` —
> the connector speaks serial-over-TCP to the simulator directly
> (`port = "tcp://127.0.0.1:9200"`). The other four protocols work out of the
> box.

### Podman instead of Docker

[podman-compose.yaml](podman-compose.yaml) is a drop-in alternative for devices
that run podman. It is deliberately narrower than
[docker-compose.yaml](docker-compose.yaml): no `build:` sections, so it needs
no repository checkout and pulls only the images published by this repo, and no
host networking, so it works on **podman-compose 1.0.x**. Copy that one file
onto the device:

```sh
podman-compose -p tedge-dot-sims -f podman-compose.yaml pull
podman-compose -p tedge-dot-sims -f podman-compose.yaml up -d
podman-compose -p tedge-dot-sims -f podman-compose.yaml ps
podman-compose -p tedge-dot-sims -f podman-compose.yaml down
```

Pass `-p` explicitly: podman-compose 1.0.x ignores the compose-spec `name:`
key and would otherwise name the project after the enclosing directory. It runs
rootless, and `docker compose -f podman-compose.yaml` works too.

It covers Modbus, OPC-UA and PROFIBUS. The two CAN simulators are **not** in
it, because SocketCAN is a property of a network namespace rather than a port:
they need `--network host`, and podman-compose 1.0.2/1.0.3 emit both
`--network host` and `--net <project>_default --network-alias <svc>`, which
podman rejects (fixed in podman-compose 1.0.6). Start those two directly:

```sh
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0

sudo podman run -d --name canbus-sim  --network host --privileged \
  --restart unless-stopped ghcr.io/reubenmiller/tedge-dot/canbus-sim:latest
sudo podman run -d --name canopen-sim --network host --privileged \
  --restart unless-stopped ghcr.io/reubenmiller/tedge-dot/canopen-sim:latest
```

The simulator images are published for `linux/amd64` and `linux/arm64` only, so
a 32-bit armhf device can run the connector package but not these simulators.

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
sudo apt install ./tedge-dot-rs_*_linux_amd64.deb     # deb
# sudo dnf install ./tedge-dot-rs_*_linux_amd64.rpm   # rpm
# sudo apk add --allow-untrusted tedge-dot-rs_*.apk   # apk
```

Installing the package:

- drops one *empty* default config per protocol into `/etc/tedge/plugins/ot/`
  (`modbus.toml`, `opcua.toml`, `canbus.toml`, `canopen.toml`) — no devices
  are configured, so the service starts and idles;
- ships the demo configs from [config/](config/) (pre-wired to the simulators)
  in `/usr/share/tedge-dot/demo/`, plus the CAN database at
  `/usr/share/tedge-dot/demo/can/test.dbc`;
- ships a **point library** per simulator from [points.d/](points.d/) in
  `/usr/share/tedge-dot/points.d/<protocol>/demo-sim.toml` — the data point
  lists the demo configs reference, and the shortest path to reading a device
  of your own: give a `[[device]]` its address, add
  `points_from = ["demo-sim"]`, and skip the point definitions entirely;
- installs and starts **one** service: `tedge-dot.service`, which runs every
  configured connector inside a single `tedge-dot` process.

### 3. Enable the demo configs

Replace the empty defaults with the demo configs that point at the simulators:

```sh
sudo cp /usr/share/tedge-dot/demo/*.toml /etc/tedge/plugins/ot/
```

Nothing else to copy: the configs reference their point libraries by name, and
the connector looks those up under `/usr/share/tedge-dot/points.d/` where the
package already installed them.

### 4. Start the simulators

The compose file pulls prebuilt simulator images from GHCR (published by the
"Publish simulators" workflow), so no repository checkout is needed — the
package ships a copy:

```sh
docker compose -f /usr/share/tedge-dot/demo/docker-compose.yaml up -d
```

From a checkout of this repo, `just demo-sims-up` does the same but builds
the images from source (it passes `--build`), so local simulator changes are
picked up. Drop `--build` to use the prebuilt images:

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
`systemctl reload tedge-dot` (SIGHUP) applies edited, added and removed configs
without a restart: an unchanged connector keeps running untouched.
`systemctl stop tedge-dot` shuts every connector down cleanly (each publishes
its final health status before exiting).

The `vcan0` interface the CAN connectors need is created by the canbus/canopen
simulator containers themselves.

To run only some protocols, remove the configs you don't want from
`/etc/tedge/plugins/ot/` and reload the service.

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
