#!/usr/bin/env bash
# `tedge-dot describe` parity check: the Rust and C binaries must render the
# same Cumulocity DTM property definitions for the same connector config.
#
#   impl/c/ci/describe-parity.sh [config.toml ...]
#
# With no arguments every connector config in the repo is checked (the demo
# configs, the e2e connector configs and the cloud harness config). Expects
# impl/c/build/tedge-dot and a Rust binary (impl/rust/target/{debug,release}/tedge-dot,
# or $RUST_BIN); the Rust one is built with cargo when neither exists.
#
# Key ORDER is allowed to differ — serde_json sorts object keys, cJSON keeps
# insertion order — so the documents are compared parsed, not as text. Numbers
# compare by value (0.0 == 0), which is what both JSON readers and the DTM
# service see.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../.." && pwd)

# Point libraries (contract §3.4) referenced by name resolve against the
# installed search path, which does not exist in a checkout — so point it at
# the ones in the repo. Every `points.d` here, since each stack and the demo
# carry their own.
library_path=""
for dir in "$repo"/demo/points.d "$repo"/connectors/*/points.d; do
    [ -d "$dir" ] || continue
    library_path="${library_path:+$library_path:}$dir"
done
export TEDGE_DOT_POINT_LIBRARY_PATH="$library_path"
c_bin="$repo/impl/c/build/tedge-dot"
rust_bin=${RUST_BIN:-}

if [ ! -x "$c_bin" ]; then
    echo "FAIL: $c_bin not built (cmake -B impl/c/build -S impl/c && cmake --build impl/c/build)" >&2
    exit 2
fi
if [ -z "$rust_bin" ]; then
    for candidate in "$repo/impl/rust/target/debug/tedge-dot" "$repo/impl/rust/target/release/tedge-dot"; do
        if [ -x "$candidate" ]; then
            rust_bin=$candidate
            break
        fi
    done
fi
if [ -z "$rust_bin" ]; then
    echo "== building the Rust binary (no impl/rust/target/{debug,release}/tedge-dot found)"
    (cd "$repo" && cargo build --quiet --manifest-path impl/rust/Cargo.toml)
    rust_bin="$repo/impl/rust/target/debug/tedge-dot"
fi

compare=$(mktemp)
trap 'rm -f "$compare"' EXIT
cat >"$compare" <<'PYEOF'
import json, os, sys

def docs(text):
    return [json.loads(line) for line in text.splitlines() if line.strip()]

name = os.environ["NAME"]
rust = docs(os.environ["RUST_OUT"])
c = docs(os.environ["C_OUT"])
if rust == c:
    print(f"OK   {name}: {len(rust)} definition(s) identical")
    sys.exit(0)
print(f"FAIL {name}: definitions differ", file=sys.stderr)
print("  rust:", json.dumps(rust, sort_keys=True, indent=2), file=sys.stderr)
print("  c:   ", json.dumps(c, sort_keys=True, indent=2), file=sys.stderr)
sys.exit(1)
PYEOF

configs=("$@")
if [ ${#configs[@]} -eq 0 ]; then
    configs=("$repo"/demo/config/*.toml "$repo"/connectors/*/connector.toml
             "$repo"/cloud/modbus/modbus.toml
             # Deliberately untyped, so the stderr comparison below actually compares a
             # warning instead of two empty files (§5.2).
             "$repo"/impl/c/ci/fixtures/untyped-modbus.toml
             # Degenerate meta.parameter shapes (lists, empty lists, junk entries).
             "$repo"/impl/c/ci/fixtures/parameter-shapes-modbus.toml
             # Long type/labels: the C build used to truncate the rendered title and
             # description where Rust does not (and overflowed a fixed title buffer).
             "$repo"/impl/c/ci/fixtures/long-strings-modbus.toml
             # Two device types folding to one qualifier: exercises the collision warning.
             "$repo"/impl/c/ci/fixtures/folded-types-modbus.toml)
fi

# stdout is the JSON and stderr carries diagnostics (a config with no device `type` is
# warned about, §5.2) — merging them would feed a warning line to the JSON parser below.
rust_errs=$(mktemp)
c_errs=$(mktemp)
trap 'rm -f "$compare" "$rust_errs" "$c_errs"' EXIT

fail=0

# Compare one `describe` invocation: exit status, stdout (parsed) and stderr must all match.
compare_run() {
    local name=$1 config=$2
    shift 2
    local rust_out c_out rust_rc=0 c_rc=0
    if [ -n "${STDIN_FROM:-}" ]; then
        # Each binary reads the file through its own pipe (`-c /dev/stdin`): a config that is
        # not a regular file must be accepted, or refused, by both.
        rust_out=$(cat "$STDIN_FROM" | "$rust_bin" describe -c "$config" --compact "$@" 2>"$rust_errs") || rust_rc=$?
        c_out=$(cat "$STDIN_FROM" | "$c_bin" describe -c "$config" --compact "$@" 2>"$c_errs") || c_rc=$?
    else
        rust_out=$("$rust_bin" describe -c "$config" --compact "$@" 2>"$rust_errs") || rust_rc=$?
        c_out=$("$c_bin" describe -c "$config" --compact "$@" 2>"$c_errs") || c_rc=$?
    fi
    # A config one binary rejects and the other accepts is the divergence that matters most:
    # the same file must be usable, or unusable, from either package.
    if [ "$rust_rc" != "$c_rc" ]; then
        echo "FAIL $name: rust exited $rust_rc, c exited $c_rc" >&2
        echo "  rust: $(cat "$rust_errs")" >&2
        echo "  c:    $(cat "$c_errs")" >&2
        fail=1
        return
    fi
    if [ "$rust_rc" != 0 ]; then
        # Both refused it, which is the part that has to match. The wording does not: the two
        # CLIs phrase and prefix their load errors differently, by design.
        echo "OK   $name: both rejected it"
        return
    fi
    # Diagnostics are part of the CLI contract too: the untyped-device warning (§5.2) must read
    # the same from either binary, or a user gets different advice depending on the package.
    if ! diff -u "$rust_errs" "$c_errs" >/dev/null; then
        echo "FAIL $name: the two binaries print different diagnostics:" >&2
        diff -u "$rust_errs" "$c_errs" >&2 || true
        fail=1
        # Deliberately no early return: a stdout divergence must still be reported, or a
        # differing diagnostic would mask the definitions differing too.
    fi
    if ! NAME="$name" RUST_OUT="$rust_out" C_OUT="$c_out" python3 "$compare"; then
        fail=1
    fi
}

for config in "${configs[@]}"; do
    compare_run "${config#"$repo"/}" "$config"
done

# `--set` forces one name for every point that does not give an absolute one, and a BLANK one
# means "not given" (an unset variable in a provisioning script). Both are easy to get subtly
# different between the two CLIs, and neither is exercised by the plain runs above.
if [ $# -eq 0 ]; then
    for forced in "plant_settings" "  plant_settings  " "" "  "; do
        compare_run "demo/config/modbus.toml --set '$forced'" \
            "$repo/demo/config/modbus.toml" --set "$forced"
    done
    # `-d` filters the devices, and the warnings are computed on what survives the filter —
    # the two CLIs implement that filter differently (retain vs swap-to-front).
    for glob in "d1" "d*" "nomatch"; do
        compare_run "folded-types-modbus.toml -d '$glob'" \
            "$repo/impl/c/ci/fixtures/folded-types-modbus.toml" -d "$glob"
    done
    # Several configs at once — a directory, or `-c` repeated — render one list of definitions
    # across all of them, with a set declared in several files merged into one, and the
    # warnings computed over every file. The demo directory mixes all five protocols; the
    # fixtures overlap on device names and types.
    compare_run "demo/config (directory)" "$repo/demo/config"
    compare_run "impl/c/ci/fixtures (directory)" "$repo/impl/c/ci/fixtures"
    for glob in "d1" "nomatch"; do
        compare_run "impl/c/ci/fixtures -d '$glob'" "$repo/impl/c/ci/fixtures" -d "$glob"
    done
    compare_run "untyped + folded-types (-c twice)" \
        "$repo/impl/c/ci/fixtures/untyped-modbus.toml" \
        -c "$repo/impl/c/ci/fixtures/folded-types-modbus.toml"
    # The configs a package installs define no devices: rendering them is empty, not an error,
    # while a `-d` pattern that was given — even `*` — must match a device.
    compare_run "packaging/config (no devices)" "$repo/packaging/config"
    compare_run "packaging/config -d '*' (no devices)" "$repo/packaging/config" -d '*'
    # A directory and one of its own files name that file twice; it is rendered once.
    compare_run "demo/config + demo/config/modbus.toml" "$repo/demo/config" \
        -c "$repo/demo/config/modbus.toml"

    # Which files a set of paths names has to agree as well, not only what the files say: a
    # config read from a pipe, a hidden file named just `.toml` (no extension, so not a config —
    # and not valid TOML here, so a build that loaded it would fail the run), one file under
    # several spellings, and more paths than a fixed-size table would hold. A file loaded twice
    # renders exactly like one loaded once, so the spellings case only pins that both builds
    # accept them; the Rust unit test `discover_configs_judges_files_not_spellings` pins the
    # de-duplication itself.
    STDIN_FROM="$repo/demo/config/modbus.toml" compare_run "a config from a pipe (-c /dev/stdin)" /dev/stdin
    paths_dir=$(mktemp -d)
    trap 'rm -rf "$compare" "$rust_errs" "$c_errs" "$paths_dir"' EXIT
    printf '[[[ not a config\n' > "$paths_dir/.toml"
    cp "$repo/impl/c/ci/fixtures/untyped-modbus.toml" "$paths_dir/untyped.toml"
    compare_run "a directory holding a bare .toml" "$paths_dir"
    compare_run "one file under three spellings" "$paths_dir" \
        -c "$paths_dir//untyped.toml" -c "$paths_dir/./untyped.toml"
    many=()
    for i in $(seq 1 70); do
        cp "$repo/impl/c/ci/fixtures/untyped-modbus.toml" "$paths_dir/copy-$i.toml"
        many+=(-c "$paths_dir/copy-$i.toml")
    done
    compare_run "70 config paths" "$paths_dir/copy-1.toml" "${many[@]:2}"
fi

if [ "$fail" != 0 ]; then
    echo "== describe parity FAILED" >&2
    exit 1
fi
echo "== describe parity passed"
