#!/usr/bin/env bash
# What does memory safety cost? — p2w native vs equivalent hand-written C.
#
# Fil-C publishes ~4x overhead for memory-safe C against unsafe C (down from
# 10x via InvisiCaps). That is the bar for "safety by runtime mechanism", and
# rust-p2w's claim — Rust-class memory management with no GC — has had no
# number attached to it at all. tools/reuse_bench.sh counts ALLOCATIONS, which
# is the reuse tier's benefit, not its cost.
#
# ⚠ WHAT THIS MEASURES, AND WHAT IT DOES NOT
#
# Fil-C's 4x isolates safety: same C source, safe compiler vs unsafe compiler.
# We cannot isolate it that way — there is no "p2w without refcounting" to
# compare against, and building one would be unsound. So this compares p2w's
# native output against hand-written C doing the same algorithm, which folds
# together THREE things:
#
#   1. the refcount + reuse machinery (the safety cost we want),
#   2. codegen quality (our LLVM emitter vs clang -O2 on idiomatic C),
#   3. the value representation (tagged i32 arena offsets vs native scalars).
#
# So a number here is an UPPER BOUND on the safety cost, not the safety cost.
# Read it as "what a student's program costs against C", which is the honest
# claim, and note the scalar case separates (2)+(3) from (1): it touches no
# heap, so any gap there is codegen and representation alone.
set -u
cd "$(dirname "$0")/.." || exit 1

for t in clang python; do
  command -v "$t" >/dev/null 2>&1 || { echo "SKIP: $t not found"; exit 0; }
done
export RUSTC_WRAPPER=''
OUT=target/safetycost
mkdir -p "$OUT"

echo "building runtime staticlib…"
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib -- -C panic=abort >/dev/null 2>&1 || {
  echo "FAIL: staticlib build"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/p2w_rt.lib
[ -f "$LIB" ] || { echo "FAIL: staticlib not found"; exit 1; }

cat > "$OUT/putc.c" <<'CEOF'
#include <stdio.h>
#include <stdlib.h>
extern int p2w_allocs(void);
static void report(void) { fprintf(stderr, "ALLOCS=%d\n", p2w_allocs()); }
void p2w_putc(unsigned char c) { putchar(c); }
int p2w_getc(void) { return getchar(); }
__attribute__((constructor)) static void init(void) { atexit(report); }
CEOF

# Median of N runs in milliseconds — one outlier on a laptop is otherwise the
# whole measurement.
time_exe() {
  local exe="$1" n=5 t times=()
  for _ in $(seq $n); do
    local s e
    s=$(python -c 'import time;print(int(time.perf_counter()*1000))')
    "$exe" >/dev/null 2>&1
    e=$(python -c 'import time;print(int(time.perf_counter()*1000))')
    times+=($((e - s)))
  done
  printf '%s\n' "${times[@]}" | sort -n | sed -n "$(((n + 1) / 2))p"
}

run_case() {
  local name="$1" py="$2" c="$3"
  printf '%b' "$py" > "$OUT/$name.py"
  printf '%b' "$c"  > "$OUT/$name.c"

  cargo run -q --example emit_ll < "$OUT/$name.py" > "$OUT/$name.ll" 2>"$OUT/$name.err" || {
    echo "  $name: FAIL emit"; return; }
  clang -O2 -Wno-override-module "$OUT/$name.ll" "$OUT/putc.c" "$LIB" -o "$OUT/$name.p2w.exe" \
    2>>"$OUT/$name.err" || { echo "  $name: FAIL link p2w"; return; }
  clang -O2 "$OUT/$name.c" -o "$OUT/$name.c.exe" 2>>"$OUT/$name.err" || {
    echo "  $name: FAIL link c"; return; }

  # Same answer, or the comparison is meaningless.
  local a b
  a=$("$OUT/$name.p2w.exe" 2>/dev/null | tr -d '\r')
  b=$("$OUT/$name.c.exe" 2>/dev/null | tr -d '\r')
  if [ "$a" != "$b" ]; then
    echo "  $name: MISMATCH p2w=[$a] c=[$b] — not comparable"; return
  fi

  local tp tc allocs ratio
  tp=$(time_exe "$OUT/$name.p2w.exe")
  tc=$(time_exe "$OUT/$name.c.exe")
  allocs=$("$OUT/$name.p2w.exe" 2>&1 >/dev/null | sed -n 's/.*ALLOCS=\([0-9]*\).*/\1/p')
  [ "$tc" -eq 0 ] && tc=1
  ratio=$(python -c "print(f'{$tp/$tc:.2f}')")
  printf '  %-14s p2w %5s ms   C %5s ms   %5sx   allocs=%s\n' \
    "$name" "$tp" "$tc" "$ratio" "${allocs:-?}"
}

echo
echo "case            p2w         C          ratio   heap"
echo "-----------------------------------------------------------"

# No heap at all: isolates codegen + value representation from refcounting.
run_case scalar \
'total = 0\ni = 0\nwhile i < 20000000:\n    total = total + i\n    i = i + 1\nprint(total)\n' \
'#include <stdio.h>\nint main(void){long long t=0;for(long long i=0;i<20000000;i++)t+=i;printf("%lld\\n",t%2147483648LL);return 0;}\n'

# Heap-resident but not churning: allocation once, reads many.
run_case listsum \
'xs: list[int] = [1, 2, 3, 4, 5, 6, 7, 8]\ntotal = 0\nr = 0\nwhile r < 2000000:\n    for x in xs:\n        total = total + x\n    r = r + 1\nprint(total)\n' \
'#include <stdio.h>\nint main(void){int xs[8]={1,2,3,4,5,6,7,8};long long t=0;for(int r=0;r<2000000;r++)for(int i=0;i<8;i++)t+=xs[i];printf("%lld\\n",t%2147483648LL);return 0;}\n'

# Heap CHURN — allocate and die every iteration. Where refcounting is paid, and
# where the reuse tier is supposed to earn its keep.
run_case churn \
'r = 0\nlast = 0\nwhile r < 300000:\n    a: list[int] = [1, 2, 3, 4]\n    b = [x + 1 for x in a]\n    last = b[3]\n    r = r + 1\nprint(last)\n' \
'#include <stdio.h>\n#include <stdlib.h>\nint main(void){int last=0;for(int r=0;r<300000;r++){int*a=malloc(4*sizeof(int));for(int i=0;i<4;i++)a[i]=i+1;int*b=malloc(4*sizeof(int));for(int i=0;i<4;i++)b[i]=a[i]+1;last=b[3];free(a);free(b);}printf("%d\\n",last);return 0;}\n'

echo
echo "Ratios are an UPPER BOUND on safety cost — they include codegen quality"
echo "and value representation. The scalar row touches no heap, so its gap is"
echo "those two alone; subtract it in spirit when reading the heap rows."
echo "Fil-C reports ~4x for memory-safe C vs unsafe C."
