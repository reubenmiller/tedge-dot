# Build / package recipes for the Rust tedge-dot.

set dotenv-load := true

# Default cross-compilation target and matching package architecture.
TARGET := "aarch64-unknown-linux-musl"
PKG_ARCH := "arm64"
VERSION := `awk -F '"' '/^version = /{print $2; exit}' Cargo.toml`

# Run the Rust unit + integration tests
test *args="":
    cargo test --workspace {{args}}

# Lint
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the SDK property-based tests only (proptest; part of `just test` too).
test-properties:
    cargo test -p tedge-dot-sdk --test properties

# Compile-check the Linux-only code paths (SocketCAN connectors are cfg-gated and silently
# skipped by a macOS `cargo build`). profibus is excluded: its serial dependency has a native
# build script that needs Linux headers — it is covered by the Docker e2e build instead.
check-linux target=TARGET:
    cargo check -p connector-canbus -p connector-canopen --target {{target}}

# Fuzz one SDK target (decode_primitive, config_toml, transform, sample_envelope).
# Requires: rustup nightly + `cargo install cargo-fuzz`.
# Usage: just fuzz decode_primitive 60
fuzz target="decode_primitive" seconds="60":
    cd crates/sdk && cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Fuzz every SDK target briefly (CI smoke; ~2 min total).
fuzz-all seconds="30":
    cd crates/sdk && for t in decode_primitive config_toml transform sample_envelope; do \
        cargo +nightly fuzz run $t -- -max_total_time={{seconds}} || exit 1; done

# Validate the thin-edge flows offline with `tedge flows test` (no broker/device/cloud).
test-flows:
    ./flows/test-flows.sh

# --- All-in-one demo ---------------------------------------------------------
# Start every OT simulator (modbus, opcua, canbus, canopen, profibus) from one
# compose file. Pairs with the connectors installed from the tedge-dot package
# and run by the single tedge-dot.service. See demo/README.md.

# Bring up all simulators (build + start).
demo-sims-up:
    docker compose -f docker-compose.simulators.yaml up -d --build
    @echo "All OT simulators are up. Install the tedge-dot package and start tedge-dot.service to run the connectors."

# Tear down all simulators.
demo-sims-down:
    docker compose -f docker-compose.simulators.yaml down -v

# Show simulator status / logs.
demo-sims-status:
    docker compose -f docker-compose.simulators.yaml ps

demo-sims-logs *args="":
    docker compose -f docker-compose.simulators.yaml logs -f {{args}}

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
    # build and up are split: `up -d --build` can hang after "resolving provenance"
    # with a docker-container buildx builder (observed with compose 2.x + colima).
    docker compose -f connectors/{{proto}}/docker-compose.yaml build
    docker compose -f connectors/{{proto}}/docker-compose.yaml up -d
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
    docker compose -f connectors/{{proto}}/docker-compose.yaml build
    docker compose -f connectors/{{proto}}/docker-compose.yaml up -d

# Tear the e2e stack down.
e2e-down proto:
    docker compose -f connectors/{{proto}}/docker-compose.yaml down -v


# Cross-compile + build all packages
build:
    goreleaser release --snapshot --clean

# Build the deb fully inside Docker (no host toolchain needed); writes to ../tests/data
test-data-docker pkg_arch=PKG_ARCH:
    @mkdir -p ../tests/data
    docker build -f Dockerfile.package --build-arg PKG_ARCH={{pkg_arch}} --target export --output ../tests/data .

# Start a shell in the cloud e2e tedge container (after `just cloud-up <proto>`)
shell proto *args='bash':
    docker compose -f cloud/{{proto}}/docker-compose.yaml exec tedge {{args}}

# Full Cumulocity end-to-end for a protocol: build the connector .deb + tedge image,
# bring up the stack, bootstrap to Cumulocity, then run the Robot suite.
# Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / DEVICE_ID in the env.
# Usage: just test-cloud modbus
test-cloud proto *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    # build and up are split: `up -d --build` can hang after "resolving provenance"
    # with a docker-container buildx builder (observed with compose 2.x + colima).
    docker compose -f cloud/{{proto}}/docker-compose.yaml build
    docker compose -f cloud/{{proto}}/docker-compose.yaml up -d
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
