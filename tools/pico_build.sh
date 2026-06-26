#!/usr/bin/env bash
# Cross-compile a p2w program to a Raspberry Pi Pico 2 (RP2350, Cortex-M33) image.
#
# Proves the native backend targets the real board's CPU: it emits the same LLVM
# IR the host run-oracle uses, compiles it for `thumbv8m.main-none-eabi`, builds
# the p2w-rt runtime for the same target, and links them with the bare-metal glue
# (device/boot.c + device/rp2350.ld) into a complete Cortex-M33 ELF — then reports
# the flash footprint.
#
# This VERIFIES the toolchain path end to end (compile + link). It does NOT yet
# produce a bootable .uf2: running on hardware also needs the RP2350 bootrom
# IMAGE_DEF metadata block and clock/UART init, which require a board + picotool
# to validate (see PICO_BACKEND.md "Remaining hardware-gated work").
#
# Requires: clang + lld (LLVM) and the Rust target thumbv8m.main-none-eabi
# (`rustup target add thumbv8m.main-none-eabi`). Skips cleanly if absent.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1
OUT="$ROOT/target/pico"
mkdir -p "$OUT"
TARGET="thumbv8m.main-none-eabi"
CPU="-mcpu=cortex-m33 -mfloat-abi=soft"

if ! command -v clang >/dev/null 2>&1 || ! command -v ld.lld >/dev/null 2>&1; then
  echo "SKIP: needs clang + ld.lld (LLVM)."; exit 0; fi
if ! rustc --print target-list 2>/dev/null | grep -qx "$TARGET" \
   || ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "SKIP: Rust target $TARGET not installed (rustup target add $TARGET)."; exit 0; fi

# Resolve the (possibly custom) cargo target directory.
TGTDIR=$(cargo metadata --no-deps --format-version 1 --manifest-path runtime/Cargo.toml 2>/dev/null \
  | grep -o '"target_directory":"[^"]*"' | head -1 | sed 's/.*"target_directory":"//;s/"$//')
[ -n "$TGTDIR" ] || TGTDIR="$ROOT/target"

echo "building p2w-rt for $TARGET…"
cargo rustc --manifest-path runtime/Cargo.toml --release --target "$TARGET" \
  --crate-type staticlib -- -C panic=abort >/dev/null 2>"$OUT/rt.err" || {
  echo "FAIL: runtime build"; cat "$OUT/rt.err"; exit 1; }
RT="$TGTDIR/$TARGET/release/libp2w_rt.a"
[ -f "$RT" ] || { echo "FAIL: runtime staticlib not found at $RT"; exit 1; }

echo "compiling bare-metal glue (device/boot.c)…"
clang --target="$TARGET" $CPU -ffreestanding -Os -c device/boot.c -o "$OUT/boot.o" \
  2>"$OUT/boot.err" || { echo "FAIL: boot.c"; cat "$OUT/boot.err"; exit 1; }

build() { # name  source
  local name="$1" src="$2"
  printf '%b' "$src" | cargo run -q --example emit_ll > "$OUT/$name.ll" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: IR emit"; cat "$OUT/$name.err"; return 1; }
  clang --target="$TARGET" $CPU -Wno-override-module -O2 -c "$OUT/$name.ll" -o "$OUT/$name.o" \
    2>"$OUT/$name.err" || { echo "FAIL [$name]: cross-compile"; cat "$OUT/$name.err"; return 1; }
  ld.lld -T device/rp2350.ld --gc-sections "$OUT/$name.o" "$OUT/boot.o" "$RT" \
    -o "$OUT/$name.elf" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: link"; cat "$OUT/$name.err"; return 1; }
  # True flash image (code + initialised data), excluding the NOBITS arena.
  llvm-objcopy -O binary -j .vectors -j .ARM.exidx -j .text -j .data \
    "$OUT/$name.elf" "$OUT/$name.bin" 2>/dev/null
  local code; code=$(llvm-size "$OUT/$name.elf" 2>/dev/null | awk 'NR==2{print $1+$2}')
  local img; img=$(wc -c < "$OUT/$name.bin" 2>/dev/null)
  echo "OK [$name]: Cortex-M33 ELF — ${code} B code+data, ${img} B flash image"
}

fails=0
build typed_mul   'def sq(n: int) -> int:\n    return n * n\nprint(sq(7))\n'       || fails=$((fails+1))
build loop_sum    'def s(n: int) -> int:\n    t: int = 0\n    i: int = 0\n    while i < n:\n        t = t + i\n        i = i + 1\n    return t\nprint(s(10))\n' || fails=$((fails+1))
build comp_packed 'xs: list[int] = [1, 2, 3]\nys: list[int] = [x * x for x in xs]\nprint(ys)\n' || fails=$((fails+1))

echo "---"
if [ "$fails" -eq 0 ]; then
  echo "all pico cross-builds linked"
else
  echo "$fails build(s) FAILED"; exit 1
fi
