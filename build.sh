#!/usr/bin/env bash
# Cross-compile from WSL and drop the exe on the Windows side.
set -e
cd "$(dirname "$0")"
cargo build --release
DEST=${1:-/mnt/c/Users/thowd/blackhole}
mkdir -p "$DEST"
cp target/x86_64-pc-windows-gnu/release/blackhole.exe "$DEST/"
echo "-> $DEST/blackhole.exe"
