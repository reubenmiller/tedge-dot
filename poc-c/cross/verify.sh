#!/bin/sh
# Smoke-check a cross-built binary on a deliberately OLD distro, to prove both
# that the cross build works and that the glibc floor zig pinned is real.
#
# Runs inside a debian:bullseye-slim container (glibc 2.31, pulled from the
# ECR Public mirror rather than Docker Hub) for the target
# architecture; a native build on ubuntu-24.04 (glibc 2.39) could not run here
# at all. Packages come from archive.debian.org: bullseye is out of security
# support, so deb.debian.org no longer carries it and its Release files have
# expired. This container only installs three libraries.
#
#   BIN  directory holding tedge-dot and tedge-dot-golden (default /out)
#   SRC  repository root, read-only (default /src)
set -eu

BIN="${BIN:-/out}"
SRC="${SRC:-/src}"

cat > /etc/apt/sources.list <<'EOF'
deb http://archive.debian.org/debian bullseye main
EOF
rm -f /etc/apt/sources.list.d/*.list 2>/dev/null || true

apt-get -o Acquire::Check-Valid-Until=false update >/dev/null
apt-get install -y --no-install-recommends \
  libmodbus5 libmosquitto1 libcjson1 >/dev/null

echo "==> host glibc: $(ldd --version | head -1)"
echo "==> CLI starts:"
"$BIN/tedge-dot" 2>&1 | head -1
echo "==> golden decode vectors:"
"$BIN/tedge-dot-golden" "$SRC/crates/sdk/conformance/vectors.json"
