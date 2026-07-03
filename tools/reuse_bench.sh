#!/usr/bin/env bash
# Allocation benchmark for the native backend — the PERF net for the Perceus
# reuse tier (the correctness net is tools/native_run.sh's live==0 gate).
#
# For each case it compiles Python -> LLVM IR -> native exe (real clang + the
# p2w-rt runtime), runs it, and reports P2W_ALLOCS (total heap allocations) and
# P2W_LIVE (must be 0). "Reuse works" = the alloc counts on the WISHLIST cases
# drop while tools/native_run.sh stays green. Today they are the baseline to beat.
#
# Same host-portable trick as native_run.sh (i32 arena offsets, not pointers), so
# no board/QEMU. Requires clang + cargo + git-bash. Run from the crate root.
set -u
cd "$(dirname "$0")/.." || exit 1

if ! command -v clang >/dev/null 2>&1; then
  echo "SKIP: clang not found — the reuse bench needs the LLVM toolchain."
  exit 0
fi
export RUSTC_WRAPPER=''

OUT=target/reusebench
mkdir -p "$OUT"

echo "building runtime staticlib (release)…"
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib -- -C panic=abort >/dev/null 2>&1 || {
  echo "FAIL: runtime staticlib build"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/p2w_rt.lib
[ -f "$LIB" ] || { echo "FAIL: staticlib not found at $LIB"; exit 1; }

# Sink for output + an exit-time readout of the alloc/live counters.
cat > "$OUT/putc.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
extern int p2w_live(void);
extern int p2w_allocs(void);
extern int p2w_peak(void);
static void report(void) {
  fprintf(stderr, "P2W_ALLOCS=%d P2W_LIVE=%d P2W_PEAK=%d\n",
          p2w_allocs(), p2w_live(), p2w_peak());
}
void p2w_putc(unsigned char c) { (void)c; } /* bench: discard stdout */
__attribute__((constructor)) static void init(void) { atexit(report); }
EOF

# allocs = total births (drop-REUSE will shrink these); peak = high-water live
# objects (precise last-use DROPS shrink these); live must be 0.
printf '%-22s %8s %6s %6s\n' "case" "allocs" "peak" "live"
printf '%-22s %8s %6s %6s\n' "----" "------" "----" "----"

bench_case() {
  local name="$1" src="$2"
  printf '%b' "$src" | cargo run -q --example emit_ll > "$OUT/$name.ll" 2>"$OUT/$name.err" || {
    printf '%-22s %8s %6s\n' "$name" "EMIT-FAIL" "-"; return; }
  clang -Wno-override-module -c "$OUT/$name.ll" -o "$OUT/$name.o" 2>>"$OUT/$name.err" \
    && clang -Wno-override-module "$OUT/$name.o" "$OUT/putc.c" "$LIB" -o "$OUT/$name.exe" 2>>"$OUT/$name.err" || {
    printf '%-22s %8s %6s\n' "$name" "BUILD-FAIL" "-"; return; }
  local line; line=$(timeout 10 "$OUT/$name.exe" 2>&1 >/dev/null)
  local allocs live peak
  allocs=$(printf '%s' "$line" | sed -n 's/.*P2W_ALLOCS=\(-\?[0-9]*\).*/\1/p')
  live=$(printf '%s' "$line" | sed -n 's/.*P2W_LIVE=\(-\?[0-9]*\).*/\1/p')
  peak=$(printf '%s' "$line" | sed -n 's/.*P2W_PEAK=\(-\?[0-9]*\).*/\1/p')
  printf '%-22s %8s %6s %6s\n' "$name" "${allocs:-?}" "${peak:-?}" "${live:-?}"
}

# --- baselines: cases the reuse tier should improve ----------------------
# A unique in-place map already reuses (try_inplace_map) — expect FEW allocs.
bench_case fbip_unique 'data: list[int] = [1, 2, 3, 4]\ndata = [x * x for x in data]\nprint(data)\n'
# Aliased: must copy (someone else can observe the original) — more allocs is correct.
bench_case fbip_alias  'data: list[int] = [1, 2, 3, 4]\nalias = data\ndata = [x * x for x in data]\nprint(len(alias))\n'

# Chain maps — LANDED (drop-reuse step 3): each stage consumes its dying
# source's buffer, so the whole pipeline runs in ONE buffer (was 10 allocs /
# peak 3 under naive scope-end; now 3 allocs / peak 1).
bench_case wl_chain    'a: list[int] = [1, 2, 3, 4, 5]\nb = [x + 1 for x in a]\nc = [y * 2 for y in b]\nprint(c[0])\n'

# Reassignment churn — LANDED (assign-site literal reuse): each `xs = [...]`
# overwrites the dead old xs in place (was 6 allocs / peak 2; now 2 / 1).
bench_case wl_realloc  'xs = [0, 0, 0, 0]\nxs = [1, 1, 1, 1]\nxs = [2, 2, 2, 2]\nprint(xs[0])\n'

# Concat loop — LANDED (append drop-reuse, p2w_add_assign): a unique receiver
# grows in place within its block's spare capacity, realloc'ing with 2x slack
# when it runs out (was 17 allocs; now 10 — the remaining 8 are the
# per-iteration "x" LITERAL allocations; literal hoisting is the follow-on).
bench_case wl_concat   's = ""\nfor i in range(8):\n    s = s + "x"\nprint(len(s))\n'

echo
echo "Interpretation: live must be 0 everywhere. allocs drop when general"
echo "drop-REUSE lands (the wishlist counts are the target). peak drops with"
echo "precise last-use drops (landed: wl_chain went 4 -> 3 — each stage's input"
echo "buffer now dies before the next stage builds). Keep native_run.sh green."
