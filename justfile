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
# See examples/README.md and connectors/README.md for the full quickstart.

# Start the protocol simulator in Docker (pairs with examples/<proto>-local.toml).
# Usage: just sim modbus   just sim opcua
sim proto:
    docker compose -f connectors/{{proto}}/docker-compose.yaml up -d --build simulator
    @echo "{{proto}} simulator ready — see examples/{{proto}}-local.toml for usage"

# Stop the protocol simulator container.
sim-down proto:
    docker compose -f connectors/{{proto}}/docker-compose.yaml rm -sf simulator

# Run the MQTT end-to-end suite for a protocol (Docker stack up → robot → down).
# Usage: just test-e2e modbus   just test-e2e opcua
test-e2e proto *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -f connectors/{{proto}}/docker-compose.yaml up -d --build
    [ -d connectors/{{proto}}/.venv ] || python3 -m venv connectors/{{proto}}/.venv
    connectors/{{proto}}/.venv/bin/pip install -q -r connectors/_shared/requirements.txt
    [ -f connectors/{{proto}}/requirements.txt ] && \
        connectors/{{proto}}/.venv/bin/pip install -q -r connectors/{{proto}}/requirements.txt || true
    rc=0
    connectors/{{proto}}/.venv/bin/python -m robot \
        --outputdir connectors/{{proto}}/output {{args}} \
        connectors/{{proto}}/tests/ || rc=$?
    docker compose -f connectors/{{proto}}/docker-compose.yaml down -v
    exit $rc

# Bring the e2e stack up without running tests (for manual inspection).
e2e-up proto:
    docker compose -f connectors/{{proto}}/docker-compose.yaml up -d --build

# Tear the e2e stack down.
e2e-down proto:
    docker compose -f connectors/{{proto}}/docker-compose.yaml down -v


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
# Full Cumulocity end-to-end for a protocol: build the connector .deb + tedge image,
# bring up the stack, bootstrap to Cumulocity, then run the Robot suite.
# Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / DEVICE_ID in the env.
# Usage: just test-cloud modbus
test-cloud proto *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -f cloud/{{proto}}/docker-compose.yaml up -d --build
    echo "Bootstrapping device ${DEVICE_ID} to Cumulocity"
    docker compose -f cloud/{{proto}}/docker-compose.yaml exec -T \
        --env "DEVICE_ID=${DEVICE_ID}" --env "C8Y_BASEURL=${C8Y_BASEURL}" \
        --env "C8Y_USER=${C8Y_USER}" --env "C8Y_PASSWORD=${C8Y_PASSWORD}" \
        tedge bootstrap.sh
    [ -d cloud/{{proto}}/.venv ] || python3 -m venv cloud/{{proto}}/.venv
    ./cloud/{{proto}}/.venv/bin/pip install -q -r cloud/{{proto}}/requirements.txt
    rc=0
    ./cloud/{{proto}}/.venv/bin/python -m robot \
        --outputdir cloud/{{proto}}/output {{args}} \
        cloud/{{proto}}/tests/ || rc=$?
    docker compose -f cloud/{{proto}}/docker-compose.yaml down -v
    exit $rc

# Bring the cloud e2e stack up (build + bootstrap) without running tests, for manual inspection.
cloud-up proto:
    #!/usr/bin/env bash
    set -euo pipefail
    just build
    docker compose -f cloud/{{proto}}/docker-compose.yaml up -d --build
    docker compose -f cloud/{{proto}}/docker-compose.yaml exec -T \
        --env "DEVICE_ID=${DEVICE_ID}" --env "C8Y_BASEURL=${C8Y_BASEURL}" \
        --env "C8Y_USER=${C8Y_USER}" --env "C8Y_PASSWORD=${C8Y_PASSWORD}" \
        tedge bootstrap.sh

# Tear down the cloud e2e stack.
cloud-down proto:
    docker compose -f cloud/{{proto}}/docker-compose.yaml down -v

# Clean up
cleanup DEVICE_ID $CI="true":
    echo "Removing device and child devices (including certificates)"
    c8y devicemanagement certificates list -n --tenant "$(c8y currenttenant get --select name --output csv)" --filter "name eq ${DEVICE_ID}" --pageSize 2000 | c8y devicemanagement certificates delete --tenant "$(c8y currenttenant get --select name --output csv)"
    c8y inventory find -n --owner "device_${DEVICE_ID}" -p 100 | c8y inventory delete
    c8y users delete -n --id "device_${DEVICE_ID}$" --tenant "$(c8y currenttenant get --select name --output csv)" --silentStatusCodes 404 --silentExit
