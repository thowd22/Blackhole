#!/usr/bin/env bash
# Cross-compile from WSL and drop the exe on the Windows side.
set -e
cd "$(dirname "$0")"
cargo build --release
# Default: the per-user install location, so dev builds replace the installed app in place.
DEST=${1:-$(ls -d /mnt/c/Users/*/AppData/Local/Programs/Blackhole 2>/dev/null | head -1)}
DEST=${DEST:-/mnt/c/Users/thowd/blackhole}
mkdir -p "$DEST"
cp target/x86_64-pc-windows-gnu/release/blackhole.exe "$DEST/"
echo "-> $DEST/blackhole.exe"
