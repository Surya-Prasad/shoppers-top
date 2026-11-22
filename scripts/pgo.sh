#!/usr/bin/env bash
# v3h: regenerate pgo/merged.profdata. Usage: scripts/pgo.sh DATA_DIR...
# Builds an instrumented binary with the Docker image's toolchain (rust 1.95, so the profile
# format matches its LLVM), runs it on each generated dataset, and merges a sparse profile.
set -euo pipefail
[ $# -ge 1 ] || { echo "usage: $0 DATA_DIR..." >&2; exit 2; }
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
rustup toolchain install 1.95 --profile minimal -c llvm-tools >/dev/null
PD=$(find "$(rustc +1.95 --print sysroot)" -name llvm-profdata | head -1)
RUSTFLAGS="-C target-cpu=x86-64-v3 -C profile-generate=$T/raw" cargo +1.95 build --release --locked -q --target-dir "$T/target"
for d in "$@"; do DATA_DIR="$d" OUTPUT_DIR="$T/out" "$T/target/release/shopping-sessions" >/dev/null 2>&1; rm -rf "$T/out"; done
mkdir -p pgo && "$PD" merge --sparse -o pgo/merged.profdata "$T"/raw/*.profraw
ls -la pgo/merged.profdata
