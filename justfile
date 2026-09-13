# Build / package recipes for tedge-dot.
#
# Both implementations live under impl/: the Rust workspace in impl/rust/ and the C one in
# impl/c/. Recipes always run from the repository root (where the shared connectors/,
# cloud/, flows/, demo/ and packaging/ trees live), so cargo is pointed at the workspace
# with --manifest-path rather than by changing directory.

set dotenv-load := true

# Default cross-compilation target and matching package architecture.
TARGET := "aarch64-unknown-linux-musl"
PKG_ARCH := "arm64"
VERSION := `awk -F '"' '/^version = /{print $2; exit}' impl/rust/Cargo.toml`

# Points cargo at the Rust workspace without leaving the repository root.
MANIFEST := "--manifest-path impl/rust/Cargo.toml"

# --- implementation capabilities ------------------------------------------------------------
#
# The Rust and C implementations run the SAME system tests, which is how parity is proven.
# Where one of them genuinely cannot support a feature, the test that covers it is tagged
# `requires:<capability>` and is SKIPPED for that implementation -- reported as a skip, with a
# reason, rather than quietly dropped or (worse) passing for the wrong reason.
#
# Every capability name a `requires:<capability>` tag may use. Declaring the vocabulary in one
# place is what turns a mistyped tag into an error instead of a test that quietly runs against
# a build that cannot pass it (see `just check-capability-tags`).
KNOWN_CAPABILITIES := "subscribe opcua-security canbus-fd profibus-serial"

# This list is the single source of truth for what the C build still lacks. Keep it in sync
# with the parity table in impl/c/README.md. Adding a capability here is a deliberate act:
# prefer implementing the feature.
#
# NOTE: a capability listed here only becomes ENFORCED once a test is tagged with it;
# `just check-capability-tags` reports the ones that are still inert.
C_MISSING_CAPABILITIES := "opcua-security canbus-fd profibus-serial"

# Create/refresh the single Python virtualenv used by every system test (and by the editor,
# see .vscode/settings.json).
venv:
    #!/usr/bin/env bash
    set -euo pipefail
    # A .venv copied from another checkout keeps that checkout's path in its scripts, so its
    # `pip` would install into the OTHER project. Recreate unless this venv was made here.
    if [ -d .venv ] && ! grep -q "venv $PWD/.venv\$" .venv/pyvenv.cfg 2>/dev/null; then
        echo "recreating ./.venv (it was not created for this checkout)" >&2
        rm -rf .venv
    fi
    [ -d .venv ] || python3 -m venv .venv
    # Always go through `python -m pip`: immune to a stale shebang in .venv/bin/pip.
    ./.venv/bin/python -m pip install -q --upgrade pip
    ./.venv/bin/python -m pip install -q -r requirements-test.txt
    # Fail loudly if anything landed outside this venv.
    ./.venv/bin/python - <<'EOF'
    import pathlib, sys, DeviceLibrary, robot
    here = pathlib.Path(".venv").resolve()
    for mod in (DeviceLibrary, robot):
        path = pathlib.Path(mod.__file__).resolve()
        if here not in path.parents:
            sys.exit(f"{mod.__name__} resolved outside ./.venv: {path}")
    print("venv ready: ./.venv (robot, DeviceLibrary, robotframework-c8y)")
    EOF

# Run the Rust unit + integration tests
test *args="":
    cargo test {{MANIFEST}} --workspace {{args}}

# Lint
lint:
    cargo clippy {{MANIFEST}} --workspace --all-targets -- -D warnings

# Run the SDK property-based tests only (proptest; part of `just test` too).
test-properties:
    cargo test {{MANIFEST}} -p tedge-dot-sdk --test properties

# Run the OT connector conformance suite (layers 1-3; built-in broker + simulator, no
# hardware). Usage: just conformance modbus [extra ot-conformance flags]
conformance protocol="modbus" *args="":
    cargo run {{MANIFEST}} -p ot-conformance -- check --spec connectors/{{protocol}}/conformance.toml {{args}}

# The same conformance suite against the C build (impl/c/), launched as an external connector
# through the `[harness] command` of connectors/<proto>/conformance-c.toml. Build the C binary
# first: cmake -S impl/c -B impl/c/build && cmake --build impl/c/build
# Usage: just conformance-c modbus
conformance-c protocol="modbus" *args="":
    cargo run {{MANIFEST}} -p ot-conformance -- check --spec connectors/{{protocol}}/conformance-c.toml {{args}}

# Compile-check the Linux-only code paths (SocketCAN connectors are cfg-gated and silently
# skipped by a macOS `cargo build`). profibus is excluded: its serial dependency has a native
# build script that needs Linux headers — it is covered by the Docker e2e build instead.
check-linux target=TARGET:
    cargo check {{MANIFEST}} -p connector-canbus -p connector-canopen --target {{target}}

# Fuzz one SDK target (decode_primitive, config_toml, transform, sample_envelope).
# Requires: rustup nightly + `cargo install cargo-fuzz`.
# Usage: just fuzz decode_primitive 60
fuzz target="decode_primitive" seconds="60":
    cd impl/rust/crates/sdk && cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Fuzz every SDK target briefly (CI smoke; ~2 min total).
fuzz-all seconds="30":
    cd impl/rust/crates/sdk && for t in decode_primitive config_toml transform sample_envelope; do \
        cargo +nightly fuzz run $t -- -max_total_time={{seconds}} || exit 1; done

# Cross-check the `requires:<capability>` test tags against the declared capability lists.
check-capability-tags:
    ./connectors/_shared/check-capability-tags.sh

# Cross-check that both packages install the same files: .goreleaser.yaml (tedge-dot-rs) and
# impl/c/packaging/nfpm.yaml (tedge-dot-c) are meant to be interchangeable, and nothing else
# enforces it.
check-manifest-parity:
    ./packaging/check-manifest-parity.sh

# Validate the thin-edge flows offline with `tedge flows test` (no broker/device/cloud).
test-flows:
    ./flows/test-flows.sh

# --- All-in-one demo ---------------------------------------------------------
# Start every OT simulator (modbus, opcua, canbus, canopen, profibus) from one
# compose file. Pairs with the connectors installed from the tedge-dot package
# and run by the single tedge-dot.service. See demo/README.md.

# Bring up all simulators (build + start).
demo-sims-up:
    docker compose -f demo/docker-compose.yaml up -d --build
    @echo "All OT simulators are up. Install the tedge-dot package and start tedge-dot.service to run the connectors."

# Tear down all simulators.
demo-sims-down:
    docker compose -f demo/docker-compose.yaml down -v

# Show simulator status / logs.
demo-sims-status:
    docker compose -f demo/docker-compose.yaml ps

demo-sims-logs *args="":
    docker compose -f demo/docker-compose.yaml logs -f {{args}}

# --- Local exploration -------------------------------------------------------
# Spin up a single simulator and poke it with the CLI (no MQTT broker / cloud).
# See demo/README.md and connectors/README.md for the full quickstart.

# Start the protocol simulator in Docker (pairs with demo/config/<proto>.toml), pinning the
# host port the demo configs expect — the e2e stacks leave it ephemeral so they stay
# parallel-safe. Usage: just sim modbus   just sim opcua
sim proto:
    #!/usr/bin/env bash
    set -euo pipefail
    export $(just _sim-port {{proto}})
    # Same retried pre-pull the e2e suites use: a bare `compose up` here fails the whole CI job
    # on `toomanyrequests: Rate exceeded`, which the shared runner egress makes routine.
    just _pull-stack-images connectors/{{proto}}/docker-compose.yaml
    docker compose -p tedge-dot-sim-{{proto}} -f connectors/{{proto}}/docker-compose.yaml up -d --build --wait simulator
    echo "{{proto}} simulator ready — see demo/config/{{proto}}.toml for usage"

# Stop the protocol simulator container.
sim-down proto:
    #!/usr/bin/env bash
    set -euo pipefail
    export $(just _sim-port {{proto}})
    docker compose -p tedge-dot-sim-{{proto}} -f connectors/{{proto}}/docker-compose.yaml rm -sf simulator

# The fixed simulator host port a protocol's demo config expects (empty = protocol has none).
_sim-port proto:
    #!/usr/bin/env bash
    case "{{proto}}" in
        modbus)   echo "MODBUS_SIM_PORT=5020" ;;
        opcua)    echo "OPCUA_SIM_PORT=4840" ;;
        profibus) echo "PROFIBUS_SIM_PORT=9200" ;;
        *)        echo "UNUSED_SIM_PORT=" ;;
    esac

# Run the MQTT end-to-end suite for a protocol. The Robot suite starts and stops the stack
# itself (DeviceLibrary, see connectors/_shared/stack.resource) — no `docker compose up` first,
# and every run gets its own randomly named compose project.
# Usage: just test-e2e modbus   just test-e2e opcua
test-e2e proto *args="":
    just _e2e {{proto}} rust "{{args}}"

# Same suite, same stack, but the connector is the C implementation (impl/c/): the Rust and
# C connectors are maintained to the same contract, so they get the same e2e coverage.
# Usage: just test-e2e-c modbus
test-e2e-c proto *args="":
    just _e2e {{proto}} c "{{args}}"

# Capabilities the named implementation does NOT provide, one per line, so the suite runners
# can turn them into `robot --skip requires:<capability>` arguments. An unknown implementation
# is an error rather than an empty list: a typo must not silently run every test.
_missing-capabilities impl:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{impl}}" in
        rust) : ;;
        c)    for cap in {{C_MISSING_CAPABILITIES}}; do echo "$cap"; done ;;
        *)    echo "unknown implementation '{{impl}}' (expected rust or c)" >&2; exit 1 ;;
    esac

# Shared body of test-e2e / test-e2e-c. `impl` is "rust" (the stack's own Dockerfile.connector)
# or "c" (connectors/_shared/Dockerfile.connector-c, selected via CONNECTOR_DOCKERFILE).
_e2e proto impl args:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{impl}}" in
        rust) outdir=connectors/{{proto}}/output ;;
        c)    outdir=connectors/{{proto}}/output-c
              export CONNECTOR_DOCKERFILE=connectors/_shared/Dockerfile.connector-c ;;
        # Without this the stack would fall through to the Rust Dockerfile and report a
        # fully green run as coverage of the OTHER implementation.
        *)    echo "unknown implementation '{{impl}}' (expected rust or c)" >&2; exit 1 ;;
    esac
    export IMPL={{impl}}
    # Tests covering a capability this implementation lacks are skipped, not dropped: they
    # show up in the report as skips so the parity gap stays visible.
    #
    # Assign first, THEN loop: a `while read < <(cmd)` swallows cmd's exit status entirely --
    # neither `set -e` nor `pipefail` sees it -- so a failing lookup would silently produce an
    # empty skip list instead of stopping the run.
    caps=$(just _missing-capabilities {{impl}})
    skips=()
    while read -r cap; do
        [ -n "$cap" ] && skips+=(--skip "requires:$cap")
    done <<< "$caps"
    just venv
    just _pull-stack-images connectors/{{proto}}/docker-compose.yaml
    ./.venv/bin/python -m robot \
        --outputdir "$outdir" --variable IMPL:{{impl}} "${skips[@]+"${skips[@]}"}" {{args}} \
        connectors/{{proto}}/tests/

# Pull the stack's registry images once, with backoff, before any suite starts.
#
# The stacks already pull from public.ecr.aws rather than docker.io for its more generous
# anonymous limits, but a full CI matrix still trips its per-IP throttle: the broker pull returns
# `toomanyrequests: Rate exceeded`. Nothing then caches the image, so every suite in the job
# repeats the same failing pull and the whole run fails in setup, before a single test executes.
#
# One retried pull up front fixes that: compose's default pull policy is "missing", so once the
# image is in the local store no suite touches the registry again. Only registry references are
# pulled -- the connector's `image:` is a locally built tag with no host, and is skipped.
#
# Best effort by design: if the image still cannot be pulled, fall through and let compose fail
# with the real error in context rather than masking it here.
_pull-stack-images compose_file:
    #!/usr/bin/env bash
    set -uo pipefail
    images=$(grep -oE 'image: *[a-z0-9.-]+\.[a-z]+/[^ ]+' {{compose_file}} | sed -E 's/image: *//' | sort -u)
    for image in $images; do
        docker image inspect "$image" >/dev/null 2>&1 && continue
        pulled=""
        for attempt in 1 2 3 4 5; do
            docker pull -q "$image" >/dev/null 2>&1 && { pulled=yes; break; }
            [ "$attempt" = 5 ] && break
            echo "pull of $image failed (attempt $attempt/5); retrying in $((attempt * 5))s" >&2
            sleep $((attempt * 5))
        done
        if [ -n "$pulled" ]; then
            echo "pulled $image"
        else
            echo "warning: could not pull $image; letting compose try" >&2
        fi
    done

# Bring a stack up manually for inspection, with the host ports pinned (the test stacks use
# ephemeral ones). Tear it down with `just e2e-down <proto> [impl]`.
# Usage: just e2e-up modbus [c]
e2e-up proto impl="rust":
    #!/usr/bin/env bash
    set -euo pipefail
    # Same guard as the suites: an unrecognised impl would otherwise export IMPL=<typo>,
    # leave CONNECTOR_DOCKERFILE unset, and quietly bring up the RUST connector under a
    # tedge-dot-<typo>-<proto>-connector image tag.
    just _missing-capabilities {{impl}} >/dev/null
    export IMPL={{impl}} BROKER_PORT=1884
    export $(just _sim-port {{proto}})
    [ "{{impl}}" = "c" ] && export CONNECTOR_DOCKERFILE=connectors/_shared/Dockerfile.connector-c || true
    docker compose -p tedge-dot-{{proto}}-manual -f connectors/{{proto}}/docker-compose.yaml up -d --build --wait
    echo "stack up: broker on localhost:1884 (canbus/canopen: see the compose file)"

# Tear down a manually started stack.
e2e-down proto impl="rust":
    #!/usr/bin/env bash
    set -euo pipefail
    export IMPL={{impl}}
    docker compose -p tedge-dot-{{proto}}-manual -f connectors/{{proto}}/docker-compose.yaml down -v

# The C implementation is packaged separately, by `just c-cross-all` + `just c-package`
# (a different toolchain entirely); release.yaml runs both and publishes one release.
# Cross-compile + package the RUST implementation (the `tedge-dot-rs` package).
build:
    goreleaser release --snapshot --clean

# Build the deb fully inside Docker (no host toolchain needed); writes to ../tests/data
test-data-docker pkg_arch=PKG_ARCH:
    @mkdir -p ../tests/data
    docker build -f Dockerfile.package --build-arg PKG_ARCH={{pkg_arch}} --target export --output ../tests/data .

# Start a shell in the tedge container of a manually started cloud stack
# (after `just cloud-up <proto> [impl]`).
shell proto *args='bash':
    docker compose -p tedge-dot-cloud-{{proto}} -f cloud/{{proto}}/docker-compose.yaml exec tedge {{args}}

# Full Cumulocity end-to-end for a protocol. The Robot suite starts the stack, bootstraps a
# freshly named device and deletes it from the tenant again (DeviceLibrary, see
# cloud/_shared/device.resource) — no compose up and no manual cleanup.
# Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / C8Y_TENANT in the env or .env.
# Run `just build` first so dist/ holds the packages the image installs.
# Usage: just test-cloud modbus
test-cloud proto *args="":
    just _cloud {{proto}} rust "{{args}}"

# The same cloud suites against the C implementation (impl/c/), which the image compiles itself —
# no `just build` needed, since dist/ is not read on this path.
# Usage: just test-cloud-c modbus
test-cloud-c proto *args="":
    just _cloud {{proto}} c "{{args}}"

# Shared body of test-cloud / test-cloud-c. `impl` picks the connector the image installs
# (IMPL, see cloud/<proto>/Dockerfile.tedge); the output directory keeps the runs apart.
_cloud proto impl args:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{impl}}" in
        rust) outdir=cloud/{{proto}}/output ;;
        c)    outdir=cloud/{{proto}}/output-c ;;
        *)    echo "unknown implementation '{{impl}}' (expected rust or c)" >&2; exit 1 ;;
    esac
    export IMPL={{impl}}
    # Same capability skipping as the connector suites, and the same reason for assigning
    # before looping (see _e2e).
    caps=$(just _missing-capabilities {{impl}})
    skips=()
    while read -r cap; do
        [ -n "$cap" ] && skips+=(--skip "requires:$cap")
    done <<< "$caps"
    just venv
    ./.venv/bin/python -m robot \
        --outputdir "$outdir" --variable IMPL:{{impl}} "${skips[@]+"${skips[@]}"}" {{args}} \
        cloud/{{proto}}/tests/

# Bring a cloud stack up manually for inspection (fixed project name, DEVICE_ID from the env),
# e.g. to poke at the mapper or run bootstrap.sh by hand. The test suites do NOT need this.
# One project per protocol, so switching `impl` replaces the running container.
# Usage: just cloud-up modbus [c]
cloud-up proto impl="rust":
    #!/usr/bin/env bash
    set -euo pipefail
    export IMPL={{impl}}
    # Only the Rust image installs the packages from dist/; the C image compiles the binary.
    if [ "{{impl}}" = "rust" ]; then just build; fi
    docker compose -p tedge-dot-cloud-{{proto}} -f cloud/{{proto}}/docker-compose.yaml up -d --build --wait
    docker compose -p tedge-dot-cloud-{{proto}} -f cloud/{{proto}}/docker-compose.yaml exec -T tedge bootstrap.sh

# Tear down a manually started cloud stack.
cloud-down proto impl="rust":
    IMPL={{impl}} docker compose -p tedge-dot-cloud-{{proto}} -f cloud/{{proto}}/docker-compose.yaml down -v

# Delete leftover test devices from the tenant. The suites clean up after themselves; this is
# the fallback for runs that crashed before their teardown. Device ids generated by
# DeviceLibrary all start with "TST_".
cleanup PATTERN="TST_*" $CI="true":
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Removing devices matching '{{PATTERN}}' (and their certificates + users)"
    tenant="$(c8y currenttenant get --select name --output csv)"
    c8y devicemanagement certificates list -n --tenant "$tenant" --filter "name like {{PATTERN}}" --pageSize 2000 \
        | c8y devicemanagement certificates delete --tenant "$tenant" --silentStatusCodes 404 || true
    c8y inventory find -n --query "name eq '{{PATTERN}}'" -p 100 | c8y inventory delete --silentStatusCodes 404 || true
    c8y users list -n --tenant "$tenant" --filter "userName like device_{{PATTERN}}" --pageSize 2000 \
        | c8y users delete --tenant "$tenant" --silentStatusCodes 404 || true

# --- C implementation (impl/c/) -----------------------------------------------
#
# The C build is cross-compiled with zig inside a Debian multiarch container
# (impl/c/cross/), so one host builds every architecture and the binaries carry
# a glibc floor we choose (2.17 by default) rather than the build host's.

# Build the C implementation natively and run its unit tests: the golden decode vectors shared with
# the Rust SDK, plus the device-parameter/`describe` checks.
# Usage: just c-test [extra ctest flags]
c-test *args="":
    cmake -B impl/c/build -S impl/c
    cmake --build impl/c/build
    ctest --test-dir impl/c/build --output-on-failure {{args}}

# Check that `tedge-dot manifest` agrees between the Rust and C builds — the manifests
# themselves, and the Cumulocity DTM definitions rendered from them. With no argument every
# connector config in the repo is compared.
#
# The script keeps its old name: it is under impl/c/, frozen at contract 0.1 with the rest of
# the C tree (RFC 0006 §9), and the one-pass C port renames it along with everything else.
# Usage: just c-manifest-parity [config.toml ...]
c-manifest-parity *configs="":
    ./impl/c/ci/describe-parity.sh {{configs}}

# Debian architectures the C implementation is built and packaged for.
C_ARCHS := "amd64 arm64 armhf"
C_GLIBC_MIN := "2.17"

# Build the zig cross-compilation image.
c-cross-image:
    docker build -t tedge-dot-cross impl/c/cross

# Cross-build the C implementation for one architecture into impl/c/dist/<arch>/.
# Usage: just c-cross arm64 [extra cmake args]
c-cross arch="arm64" *args="": c-cross-image
    mkdir -p "impl/c/dist/{{arch}}"
    docker run --rm \
        -v "$PWD:/src:ro" -v "$PWD/impl/c/dist/{{arch}}:/out" \
        -e ARCH={{arch}} -e GLIBC_MIN={{C_GLIBC_MIN}} \
        tedge-dot-cross {{args}}

# Cross-build every architecture in C_ARCHS.
c-cross-all: c-cross-image
    #!/usr/bin/env bash
    set -euo pipefail
    for arch in {{C_ARCHS}}; do just c-cross "$arch"; done

# Run the golden decode vectors for a cross-built architecture on an old distro
# (Debian bullseye, glibc 2.31) — checks both the cross build and the glibc floor.
# Non-native architectures need binfmt/qemu:
#   docker run --privileged --rm tonistiigi/binfmt --install all
c-verify arch="arm64":
    docker run --rm --platform linux/{{ if arch == "armhf" { "arm/v7" } else { arch } }} \
        -v "$PWD/impl/c/dist/{{arch}}:/out" \
        -v "$PWD:/src:ro" \
        -v "$PWD/impl/c/cross/verify.sh:/verify.sh:ro" \
        debian:bullseye-slim /verify.sh

# Package one cross-built architecture as deb/rpm/apk into impl/c/dist/packages/.
# Usage: just c-package arm64 0.1.0
c-package arch="arm64" version="0.0.0-dev":
    #!/usr/bin/env bash
    set -euo pipefail
    # nfpm expands env vars in scalar fields but not in contents[].src, so stage
    # the architecture's binary at the fixed path nfpm.yaml points to.
    mkdir -p impl/c/dist/staged impl/c/dist/packages
    cp "impl/c/dist/{{arch}}/tedge-dot" impl/c/dist/staged/tedge-dot
    case "{{arch}}" in
        armhf) nfpm_arch=arm7 ;;
        *)     nfpm_arch="{{arch}}" ;;
    esac
    for format in deb rpm apk; do
        docker run --rm -v "$PWD:/work" -w /work \
            -e ARCH="$nfpm_arch" -e VERSION="{{version}}" \
            ghcr.io/goreleaser/nfpm:latest \
            pkg -f impl/c/packaging/nfpm.yaml -p "$format" -t impl/c/dist/packages/
    done
    ls -l impl/c/dist/packages
