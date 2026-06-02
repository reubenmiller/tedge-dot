# Build / package recipes for the Rust tedge-dot.

set dotenv-load := true

# Default cross-compilation target and matching package architecture.
TARGET := "aarch64-unknown-linux-musl"
PKG_ARCH := "arm64"
VERSION := `awk -F '"' '/^version/ {print $2; exit}' Cargo.toml`

# Run the Rust unit + integration tests
test *args="":
    cargo test --workspace {{args}}

# Lint
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Validate the thin-edge flows offline with `tedge flows test` (no broker/device/cloud).
test-flows:
    ./flows/test-flows.sh

# --- Local exploration -------------------------------------------------------
# Spin up a single simulator and poke it with the CLI (no MQTT broker / cloud).
# See examples/README.md for the full quickstart.

# Start the Modbus simulator in Docker on host port 5020 (pairs with examples/modbus-local.toml).
sim-modbus:
    docker compose -f e2e-modbus/docker-compose.yaml up -d --build simulator
    @echo "Modbus simulator listening on 127.0.0.1:5020"
    @echo "Try: cargo run -- read -c examples/modbus-local.toml -d plc1 -p temp_u16 -p level_f32 -p coil_rw"

# Stop the Modbus simulator container.
sim-modbus-down:
    docker compose -f e2e-modbus/docker-compose.yaml rm -sf simulator

# Start the OPC-UA simulator in Docker on host port 4840 (pairs with examples/opcua-local.toml).
sim-opcua:
    docker compose -f e2e-opcua/docker-compose.yaml up -d --build simulator
    @echo "OPC-UA simulator listening on opc.tcp://127.0.0.1:4840/"
    @echo "Try: cargo run -- read -c examples/opcua-local.toml -d opc1 -p temperature -p count_u32 --json"

# Stop the OPC-UA simulator container.
sim-opcua-down:
    docker compose -f e2e-opcua/docker-compose.yaml rm -sf simulator

# Run the MQTT end-to-end suite against a real Modbus simulator (Docker stack up/down).
test-e2e-modbus *args="":
    #!/usr/bin/env bash
    set -euxo pipefail
    docker compose -f e2e-modbus/docker-compose.yaml up -d --build
    [ -d e2e-modbus/.venv ] || python3 -m venv e2e-modbus/.venv
    ./e2e-modbus/.venv/bin/pip install -q -r e2e-modbus/requirements.txt
    rc=0
    ./e2e-modbus/.venv/bin/python -m robot --outputdir e2e-modbus/output {{args}} e2e-modbus/tests/modbus_e2e.robot || rc=$?
    docker compose -f e2e-modbus/docker-compose.yaml down -v
    exit $rc

# Bring the e2e stack up without running tests (for manual inspection).
e2e-modbus-up:
    docker compose -f e2e-modbus/docker-compose.yaml up -d --build

# Tear the e2e stack down.
e2e-modbus-down:
    docker compose -f e2e-modbus/docker-compose.yaml down -v

# Run the MQTT end-to-end suite against a real OPC-UA simulator (python-asyncua).
# Proves the connector contract is protocol-neutral: same envelopes, NodeId addressing.
test-e2e-opcua *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -f e2e-opcua/docker-compose.yaml up -d --build
    [ -d e2e-opcua/.venv ] || python3 -m venv e2e-opcua/.venv
    ./e2e-opcua/.venv/bin/pip install -q -r e2e-opcua/requirements.txt
    rc=0
    ./e2e-opcua/.venv/bin/python -m robot --outputdir e2e-opcua/output {{args}} e2e-opcua/tests/opcua_e2e.robot || rc=$?
    docker compose -f e2e-opcua/docker-compose.yaml down -v
    exit $rc


# Cross-compile + build all packages
build:
    goreleaser release --snapshot --clean

# Same as test-data but builds the deb fully inside Docker (no host toolchain needed)
test-data-docker pkg_arch=PKG_ARCH:
    @mkdir -p ../tests/data
    docker build -f Dockerfile.package --build-arg PKG_ARCH={{pkg_arch}} --target export --output ../tests/data .

# Configure and register the device to the cloud
bootstrap *args="":
    docker compose exec --env "DEVICE_ID=${DEVICE_ID:-}" --env "C8Y_BASEURL=${C8Y_BASEURL:-}" --env "C8Y_USER=${C8Y_USER:-}" --env "C8Y_PASSWORD=${C8Y_PASSWORD:-}" tedge bootstrap.sh {{args}}

# Start a shell
shell *args='bash':
    docker compose exec tedge {{args}}

# Full Cumulocity end-to-end for the Rust OT connector: build the connector .deb + tedge image,
# bring up the stack (tedge + simulator), bootstrap to Cumulocity, then run the Robot suite.
# Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / DEVICE_ID (and an active C8Y_TENANT) in the env.
E2E_C8Y_COMPOSE := "e2e-c8y/docker-compose.yaml"
test-e2e-c8y *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    # just build
    docker compose -f {{E2E_C8Y_COMPOSE}} up -d --build
    echo "Bootstrapping device ${DEVICE_ID} to Cumulocity"
    docker compose -f {{E2E_C8Y_COMPOSE}} exec -T \
        --env "DEVICE_ID=${DEVICE_ID}" --env "C8Y_BASEURL=${C8Y_BASEURL}" \
        --env "C8Y_USER=${C8Y_USER}" --env "C8Y_PASSWORD=${C8Y_PASSWORD}" \
        tedge bootstrap.sh
    [ -d e2e-c8y/.venv ] || python3 -m venv e2e-c8y/.venv
    ./e2e-c8y/.venv/bin/pip install -q -r e2e-c8y/requirements.txt
    rc=0
    ./e2e-c8y/.venv/bin/python -m robot \
        --outputdir e2e-c8y/output {{args}} \
        e2e-c8y/tests/modbus_c8y.robot || rc=$?
    docker compose -f {{E2E_C8Y_COMPOSE}} down -v
    exit $rc

# Bring the c8y e2e stack up (build + bootstrap) without running tests, for manual inspection.
e2e-c8y-up:
    #!/usr/bin/env bash
    set -euo pipefail
    just build
    docker compose -f {{E2E_C8Y_COMPOSE}} up -d --build
    docker compose -f {{E2E_C8Y_COMPOSE}} exec -T \
        --env "DEVICE_ID=${DEVICE_ID}" --env "C8Y_BASEURL=${C8Y_BASEURL}" \
        --env "C8Y_USER=${C8Y_USER}" --env "C8Y_PASSWORD=${C8Y_PASSWORD}" \
        tedge bootstrap.sh

# Tear down the c8y e2e stack.
e2e-c8y-down:
    docker compose -f {{E2E_C8Y_COMPOSE}} down -v

# Clean up
cleanup DEVICE_ID $CI="true":
    echo "Removing device and child devices (including certificates)"
    c8y devicemanagement certificates list -n --tenant "$(c8y currenttenant get --select name --output csv)" --filter "name eq ${DEVICE_ID}" --pageSize 2000 | c8y devicemanagement certificates delete --tenant "$(c8y currenttenant get --select name --output csv)"
    c8y inventory find -n --owner "device_${DEVICE_ID}" -p 100 | c8y inventory delete
    c8y users delete -n --id "device_${DEVICE_ID}$" --tenant "$(c8y currenttenant get --select name --output csv)" --silentStatusCodes 404 --silentExit
