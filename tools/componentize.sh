#!/usr/bin/env bash
# The component converter tool (LESSON_PLAYER.md step 5e): stamped-component
# Python -> a real Component-Model component. Generalizes tools/wasm_poc.sh
# (the proof this chain works) into a per-component build:
#
#   componentize.sh <program.py> <instance> <api,csv>
#
# 1. rust-p2w's converter front half (to_component, via examples/emit_component)
#    gates on the component-clean lint and generates component.py (the verbatim
#    def group), component.wit (exports from annotations, imports = used caps),
#    and shim.c (the canonical-ABI marshalling).
# 2. The LLVM/p2w-rt backend compiles component.py to linear-memory wasm32.
# 3. wasm-tools embed/new wraps it as a component; validate + print its WIT.
#
# Output: target/component/<instance>/<instance>.component.wasm
# Requires: clang+wasm-ld, wasm-tools, the wasm32-unknown-unknown Rust target.
set -u
cd "$(dirname "$0")/.." || exit 1
export RUSTC_WRAPPER=''

PY=${1:?usage: componentize.sh <program.py> <instance> <api,csv>}
INSTANCE=${2:?instance id (e.g. grid)}
API=${3:?api csv (e.g. set,show)}
OUT=target/component/$INSTANCE
mkdir -p "$OUT"

for t in clang wasm-ld wasm-tools; do
  command -v "$t" >/dev/null 2>&1 || { echo "SKIP: $t not found"; exit 0; }
done

echo "building p2w-rt as a wasm32 staticlib…"
cargo rustc --manifest-path runtime/Cargo.toml --target wasm32-unknown-unknown \
  --release --crate-type staticlib -- -C panic=abort >"$OUT/rtbuild.log" 2>&1 || {
  echo "FAIL: runtime wasm32 staticlib build"; tail -20 "$OUT/rtbuild.log"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/wasm32-unknown-unknown/release/libp2w_rt.a
[ -f "$LIB" ] || LIB=$(dirname "$LIB")/p2w_rt.a
[ -f "$LIB" ] || { echo "FAIL: wasm staticlib not found"; exit 1; }

echo "converting (clean gate + WIT + shim)…"
cargo run -q --example emit_component -- "$INSTANCE" "$API" "$OUT" < "$PY" \
  | tee "$OUT/convert.txt" || { echo "FAIL: conversion refused"; exit 1; }
grep -q '^exports:' "$OUT/convert.txt" || { echo "FAIL: conversion refused"; exit 1; }

echo "compiling the def group (LLVM/p2w-rt, linear memory)…"
cargo run -q --example emit_ll < "$OUT/component.py" > "$OUT/component.ll" 2>"$OUT/emit.err" || {
  echo "FAIL: emit_ll"; cat "$OUT/emit.err"; exit 1; }

# No --allow-undefined: every symbol must resolve (runtime, shim, or a
# canonical import) — a stray undefined would become an env.* import that
# `component new` rejects anyway; surface it at link time instead.
clang --target=wasm32 -Wno-override-module -nostdlib -O2 -Wl,--no-entry \
  "$OUT/component.ll" "$OUT/shim.c" "$LIB" -o "$OUT/core.wasm" 2>"$OUT/link.err" || {
  echo "FAIL: clang/wasm-ld"; cat "$OUT/link.err"; exit 1; }

# The WIT world name is the kebab of the instance id (underscores are not
# valid WIT identifiers): open_response -> world open-response.
WORLD=${INSTANCE//_/-}
wasm-tools component embed "$OUT/component.wit" --world "$WORLD" "$OUT/core.wasm" \
  -o "$OUT/embed.wasm" 2>"$OUT/wt.err" \
  && wasm-tools component new "$OUT/embed.wasm" -o "$OUT/$INSTANCE.component.wasm" 2>>"$OUT/wt.err" \
  && wasm-tools validate --features component-model "$OUT/$INSTANCE.component.wasm" 2>>"$OUT/wt.err" || {
  echo "FAIL: wasm-tools"; cat "$OUT/wt.err"; exit 1; }

echo "OK: $OUT/$INSTANCE.component.wasm ($(wc -c < "$OUT/$INSTANCE.component.wasm") bytes)"
echo "--- its WIT ---"
wasm-tools component wit "$OUT/$INSTANCE.component.wasm"
