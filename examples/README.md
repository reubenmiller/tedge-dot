# Exploring Modbus and OPC-UA locally

Two ready-to-run simulators plus the connector's `read`/`write` CLI let you poke a
real Modbus TCP server and a real OPC-UA server on your machine — no cloud, no MQTT
broker, no physical hardware.

Build the connector once:

```sh
cargo build
```

## Modbus

The Modbus simulator (pymodbus) runs in Docker and exposes port 502 as host **5020**.

```sh
just sim-modbus     # docker compose up the simulator on 127.0.0.1:5020

# read typed values (uint16 / float32 / bool)
cargo run -- read  -c examples/modbus-local.toml -d plc1 -p temp_u16 -p level_f32 -p coil_rw

# write and read back
cargo run -- write -c examples/modbus-local.toml -d plc1 -p coil_rw  --value true
cargo run -- write -c examples/modbus-local.toml -d plc1 -p temp_u16 --value 1234
cargo run -- read  -c examples/modbus-local.toml -d plc1 -p temp_u16 --json

# a point that returns a Modbus exception -> bad quality, exit code 1
cargo run -- read  -c examples/modbus-local.toml -d plc1 -p bad_point

just sim-modbus-down
```

## OPC-UA

The OPC-UA simulator (python-asyncua) runs on the host and advertises
`opc.tcp://127.0.0.1:4840/`.

> asyncua needs **Python 3.10–3.13** (3.14 breaks its binary serialization, which
> shows up as a `BadTimeout` on connect). `just sim-opcua` picks a compatible
> interpreter automatically.

```sh
just sim-opcua      # foreground; Ctrl-C to stop. Run the CLI in another shell.

# read typed values (float64 / uint32 / int32 / bool)
cargo run -- read  -c examples/opcua-local.toml -d opc1 -p temperature -p count_u32 -p setpoint -p running --json

# write and read back
cargo run -- write -c examples/opcua-local.toml -d opc1 -p setpoint --value 42
cargo run -- write -c examples/opcua-local.toml -d opc1 -p running  --value true
cargo run -- read  -c examples/opcua-local.toml -d opc1 -p setpoint

# a node that returns a Bad status -> bad quality, exit code 1
cargo run -- read  -c examples/opcua-local.toml -d opc1 -p bad_point
```

The `Client is missing its application instance certificate` logs that async-opcua
emits for `security_policy = "None"` are silenced by default (no client cert is
needed for unencrypted, anonymous access). Set `RUST_LOG=info` to see the full
OPC-UA client logging.

## Full MQTT end-to-end stacks

To exercise the complete pipeline (connector + broker + simulator, publishing
samples over MQTT) use the Docker-based harnesses instead:

```sh
just test-e2e         # Modbus: bring stack up, run Robot suite, tear down
just test-e2e-opcua   # OPC-UA: same, against the asyncua simulator
just e2e-up           # Modbus stack up for manual inspection
just e2e-down
```
