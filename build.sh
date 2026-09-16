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
# The embedded Neovim lives beside the exe; refresh it when the bundle changed.
if [ -d runtime/nvim/nvim-win64 ] && ! cmp -s runtime/nvim/nvim-win64/bin/nvim.exe "$DEST/nvim/bin/nvim.exe" 2>/dev/null; then
  rm -rf "$DEST/nvim" && cp -r runtime/nvim/nvim-win64 "$DEST/nvim" && echo "-> $DEST/nvim (Neovim)"
fi
echo "-> $DEST/blackhole.exe"
