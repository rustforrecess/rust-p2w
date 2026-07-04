#!/usr/bin/env bash
# Differential fuzzer for the native backend: generated programs (SAFE by
# construction — tools/gen_program.py) are run under CPython AND compiled +
# run natively; outputs are diffed and the run must end leak-free (live==0).
# Turns the hand-written oracle into thousands of generated cases aimed at the
# drop-reuse machinery. Reproduce any failure with its printed seed:
#   python3 tools/gen_program.py SEED
#
# Usage: tools/fuzz_native.sh            (seeds 1..FUZZ_N, default 25)
#        FUZZ_N=200 FUZZ_START=1 tools/fuzz_native.sh
# Requires clang + cargo + python3; skips cleanly when a tool is missing.
set -u
cd "$(dirname "$0")/.." || exit 1
export RUSTC_WRAPPER=''

for t in clang python3 cargo; do
  command -v "$t" >/dev/null 2>&1 || { echo "SKIP: $t not found"; exit 0; }
done

N=${FUZZ_N:-25}
START=${FUZZ_START:-1}
OUT=target/fuzznative
mkdir -p "$OUT"

echo "building runtime staticlib…"
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib -- -C panic=abort >/dev/null 2>&1 || {
  echo "FAIL: runtime staticlib build"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/p2w_rt.lib
[ -f "$LIB" ] || { echo "FAIL: staticlib not found at $LIB"; exit 1; }

cat > "$OUT/putc.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
extern int p2w_live(void);
static void report(void) { fprintf(stderr, "P2W_LIVE=%d\n", p2w_live()); }
void p2w_putc(unsigned char c) { putchar(c); }
int p2w_getc(void) { return getchar(); }
__attribute__((constructor)) static void init(void) { atexit(report); }
EOF

fails=0
pass=0
END=$((START + N - 1))
for seed in $(seq "$START" "$END"); do
  P="$OUT/prog$seed.py"
  python3 tools/gen_program.py "$seed" > "$P" || { echo "FAIL [seed $seed]: generator"; fails=$((fails+1)); continue; }
  # Ground truth from CPython.
  want=$(timeout 10 python3 "$P" 2>"$OUT/py.err" | tr -d '\r')
  if [ $? -ne 0 ]; then echo "FAIL [seed $seed]: CPython rejected the program (generator bug)"; head -3 "$OUT/py.err"; fails=$((fails+1)); continue; fi
  # Native pipeline.
  cargo run -q --example emit_ll < "$P" > "$OUT/p.ll" 2>"$OUT/emit.err" || {
    echo "FAIL [seed $seed]: emit"; head -3 "$OUT/emit.err"; fails=$((fails+1)); continue; }
  clang -Wno-override-module "$OUT/p.ll" "$OUT/putc.c" "$LIB" -o "$OUT/p.exe" 2>"$OUT/cc.err" || {
    echo "FAIL [seed $seed]: clang"; head -3 "$OUT/cc.err"; fails=$((fails+1)); continue; }
  got=$(timeout 10 "$OUT/p.exe" 2>"$OUT/run.err" | tr -d '\r')
  if [ $? -eq 124 ]; then echo "FAIL [seed $seed]: native run timed out"; fails=$((fails+1)); continue; fi
  live=$(sed -n 's/.*P2W_LIVE=\(-\?[0-9]*\).*/\1/p' "$OUT/run.err"); : "${live:=?}"
  if [ "$got" != "$want" ]; then
    echo "DIFF [seed $seed]  (repro: python3 tools/gen_program.py $seed)"
    diff <(printf '%s\n' "$want") <(printf '%s\n' "$got") | head -8
    fails=$((fails+1)); continue
  fi
  if [ "$live" != "0" ]; then
    echo "LEAK [seed $seed]: output ok but live=$live  (repro: python3 tools/gen_program.py $seed)"
    fails=$((fails+1)); continue
  fi
  pass=$((pass+1))
done

echo
echo "fuzz: $pass/$N passed (seeds $START..$END)"
[ "$fails" = 0 ] || echo "REPRODUCE: python3 tools/gen_program.py <seed>  (program on stdout)"
exit "$fails"
