#!/bin/sh
# Cross-compiles dist/Spotimy.exe on macOS.
# Needs: brew install mingw-w64 && rustup target add x86_64-pc-windows-gnu
set -eu
cd "$(dirname "$0")/.."
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
cargo build --release --target x86_64-pc-windows-gnu
mkdir -p dist
cp target/x86_64-pc-windows-gnu/release/spotimy.exe dist/Spotimy.exe
echo "Built dist/Spotimy.exe"
