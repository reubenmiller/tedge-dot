# Connectors

Each OT protocol connector lives in its own subdirectory here. All connectors
share a common layout, and the `just` recipes pick up any protocol by name —
no justfile changes are ever needed to add a new one.

---

## Directory layout

```
connectors/
  _shared/                      # files shared across all connectors
    MqttClient.py               # Robot keyword library (paho-mqtt subscribe/assert)
    mosquitto.conf              # mosquitto config used by every e2e stack
    requirements.txt            # base Robot deps (robotframework, paho-mqtt)

  <proto>/                      # one directory per OT protocol
    sim/                        # simulator image (Dockerfile + server code)
      Dockerfile
      ...
    tests/
      <proto>_e2e.robot         # Robot Framework e2e suite
    packaging/
      <proto>.toml              # default installed connector config
    docker-compose.yaml         # 3-service stack: broker, simulator, connector
    Dockerfile.connector        # builds the Rust connector binary for e2e
    connector.toml              # connector config used inside the container
    entrypoint.sh               # waits for deps, then execs tedge-dot
    requirements.txt            # protocol-specific extra Python deps (optional)
```

### Broker port convention

Each protocol's e2e stack exposes the broker on a unique host port to avoid
clashing when multiple stacks run simultaneously:

| Protocol | Host broker port | Simulator port         |
|----------|-----------------|------------------------|
| modbus   | 11883           | 5020 (TCP)             |
| opcua    | 12883           | 4840 (TCP)             |
| canbus   | 13883           | vcan0 (kernel virtual) |
| *(next)* | 14883           | *(protocol)*           |

Pick the next unused port in `docker-compose.yaml` when adding a new connector.

---

## `just` recipes

All recipes take the protocol name as their first argument.

```sh
just sim modbus            # start only the simulator (for manual CLI exploration)
just sim-down modbus       # stop the simulator

just e2e-up modbus         # bring the full e2e stack up (broker + sim + connector)
just e2e-down modbus       # tear it down

just test-e2e modbus       # stack up → run robot suite → stack down
just test-e2e modbus --include smoke   # pass extra robot args

just cloud-up modbus       # bring up cloud (Cumulocity) stack and bootstrap
just cloud-down modbus     # tear it down
just test-cloud modbus     # full cloud e2e run (requires C8Y_* env vars)
```

The `test-e2e` recipe installs Python deps automatically:

1. `connectors/_shared/requirements.txt` (always — robotframework, paho-mqtt)
2. `connectors/<proto>/requirements.txt` (if present — protocol-specific extras)

---

## Adding a new protocol

> See [doc/connectors/_template-connector-spec.md](../doc/connectors/_template-connector-spec.md)
> for the full connector spec template and a detailed checklist.

1. **Rust crate** — create `crates/connector-<proto>/` and implement the
   [`Connector` trait](../doc/sdk/connector-sdk.md).  Add a cargo feature flag
   in `Cargo.toml`.

2. **Simulator** — create `connectors/<proto>/sim/` with a `Dockerfile` and
   whatever server code the protocol needs.

3. **Docker stack** — create `connectors/<proto>/docker-compose.yaml`,
   `Dockerfile.connector`, `connector.toml`, and `entrypoint.sh`.
   Pick the next free host port from the table above.

4. **Tests** — create `connectors/<proto>/tests/<proto>_e2e.robot`.
   Import the shared library:
   ```
   Library    ../../_shared/MqttClient.py
   ```

5. **Packaging config** — create `connectors/<proto>/packaging/<proto>.toml`
   with sane defaults (no devices, correct `protocol` and `service_name`).

That's it. `just sim <proto>` and `just test-e2e <proto>` work immediately.
