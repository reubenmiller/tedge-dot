# Linux runner for the j1939 Step-0 validation (for use from a macOS dev box).
# NOTE: `vcan` is a host-kernel module — run with --cap-add=NET_ADMIN, and the Docker
# VM's kernel must provide vcan. If modprobe fails, use a real Linux host / CI instead.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake ninja-build git iproute2 can-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /step0
COPY . /step0

RUN ./scripts/fetch-lib.sh && cmake -B build -S . -G Ninja && cmake --build build

# vcan setup needs a live kernel, so it runs at container start rather than build time.
CMD sh -c './scripts/setup-vcan.sh && (./build/harness & sleep 1 && ./scripts/send-vectors.sh && sleep 2)'
