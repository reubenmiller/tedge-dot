#!/bin/sh
# Clone Open-SAE-J1939 (MIT) into vendor/ for the spike. Gitignored.
# Pin REF to a tag/commit before promoting this into the real connector.
set -eu

REF="${1:-main}"
DEST="$(dirname "$0")/../vendor/open-sae-j1939"

if [ -d "$DEST" ]; then
    echo "vendor/open-sae-j1939 already present — skipping clone"
    exit 0
fi

mkdir -p "$(dirname "$DEST")"
git clone --depth 1 --branch "$REF" \
    https://github.com/DanielMartensson/Open-SAE-J1939.git "$DEST"
echo "cloned Open-SAE-J1939@$REF into $DEST"
