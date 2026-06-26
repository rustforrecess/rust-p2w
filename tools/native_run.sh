#!/usr/bin/env bash
# Host run-oracle for the native (Pico) backend.
#
# Compiles each Python case to LLVM IR, runs it through the REAL LLVM toolchain
# (clang), links it against the actual p2w-rt runtime, executes it on this host,
# and diffs the output against the expected string. This works because our value
# model is i32 *offsets* into a static arena (not machine pointers), so the
# emitted IR + runtime are host-portable — no board or QEMU needed.
#
# Requires: clang (LLVM) + cargo + git-bash. Skips cleanly if clang is absent.
# Usage: tools/native_run.sh   (run from the rust-p2w crate root)
set -u
cd "$(dirname "$0")/.." || exit 1

if ! command -v clang >/dev/null 2>&1; then
  echo "SKIP: clang not found — the native run-oracle needs the LLVM toolchain."
  exit 0
fi
export RUSTC_WRAPPER=''

OUT=target/nativerun
mkdir -p "$OUT"

echo "building runtime staticlib (panic=abort)…"
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib -- -C panic=abort >/dev/null 2>&1 || {
  echo "FAIL: runtime staticlib build"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/p2w_rt.lib
[ -f "$LIB" ] || { echo "FAIL: staticlib not found at $LIB"; exit 1; }

# Byte sink for p2w_print + a leak readout: at process exit, print the live
# heap-object count to stderr so the harness can gate on RC correctness.
cat > "$OUT/putc.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
extern int p2w_live(void);
extern int p2w_allocs(void);
static void report(void) {
  fprintf(stderr, "P2W_LIVE=%d\n", p2w_live());
  fprintf(stderr, "P2W_ALLOCS=%d\n", p2w_allocs());
}
void p2w_putc(unsigned char c) { putchar(c); }
__attribute__((constructor)) static void init(void) { atexit(report); }
EOF

# A case: name | source | expected-stdout.  Use \n in source for newlines.
run_case() {
  local name="$1" src="$2" want="$3"
  printf '%b' "$src" | cargo run -q --example emit_ll > "$OUT/$name.ll" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: emit"; cat "$OUT/$name.err"; return 1; }
  clang -Wno-override-module -c "$OUT/$name.ll" -o "$OUT/$name.o" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: clang compile"; cat "$OUT/$name.err"; return 1; }
  clang -Wno-override-module "$OUT/$name.o" "$OUT/putc.c" "$LIB" -o "$OUT/$name.exe" \
    2>"$OUT/$name.err" || { echo "FAIL [$name]: link"; cat "$OUT/$name.err"; return 1; }
  # strip CR: the Windows CRT writes \n as \r\n in text mode. A runtime trap
  # spin-loops (device behavior), so cap each run — a hang is a bug, not a pass.
  local got; got=$(timeout 10 "$OUT/$name.exe" 2>"$OUT/$name.live" | tr -d '\r')
  if [ $? -eq 124 ]; then
    echo "FAIL [$name]: timed out (likely a runtime trap / use-after-free)"
    return 1
  fi
  local live; live=$(sed -n 's/.*P2W_LIVE=\(-\?[0-9]*\).*/\1/p' "$OUT/$name.live")
  : "${live:=?}"
  if [ "$got" != "$(printf '%b' "$want")" ]; then
    echo "FAIL [$name]: got [$got] want [$(printf '%b' "$want")]  live=$live"
    return 1
  fi
  if [ "$GATE_LEAKS" = "1" ] && [ "$live" != "0" ]; then
    echo "LEAK [$name]: output ok but live=$live (expected 0)"
    return 1
  fi
  echo "PASS [$name]  live=$live"
  return 0
}

# The emitter RC pass is in, so leaks are regressions: gate on live==0 by
# default. Set GATE_LEAKS=0 to only check output (e.g. when diagnosing).
GATE_LEAKS=${GATE_LEAKS:-1}
fails=0
run_case ints      'print(6 * 7)\nprint(10 - 3)\n'                              '42\n7'        || fails=$((fails+1))
run_case floats    'print(7 / 2)\nprint(2 ** 10)\nprint(1.5 + 2)\n'            '3.5\n1024\n3.5' || fails=$((fails+1))
run_case nativearith 'print(2 + 3 * 4)\nprint((10 - 3) * 2)\nprint(1 - 5)\n'    '14\n14\n-4'    || fails=$((fails+1))
run_case floatmath 'print(1.5 * 2.0)\nprint(7 / 2)\nprint(1 + 2.5)\nprint(10.0 - 3)\n' '3.0\n3.5\n3.5\n7.0' || fails=$((fails+1))
run_case floatcmp  'print(1.5 < 2.0)\nprint(2.5 > 3.0)\nprint(3.0 == 3.0)\n'     'True\nFalse\nTrue' || fails=$((fails+1))
run_case floatparam 'def dbl(x: float) -> float:\n    return x * 2.0\nprint(dbl(2.5))\n' '5.0'   || fails=$((fails+1))
run_case floatlocal 'total: float = 0.0\ntotal = total + 1.5\ntotal = total + 2.5\nprint(total)\n' '4.0' || fails=$((fails+1))
run_case floatintarg 'def half(x: float) -> float:\n    return x / 2.0\nprint(half(7))\n' '3.5'   || fails=$((fails+1))
run_case truediv   'print(4 / 2)\nprint(2 ** -1)\n'                            '2.0\n0.5'     || fails=$((fails+1))
run_case loop      'total = 0\nfor i in range(5):\n    total = total + i\nprint(total)\n' '10' || fails=$((fails+1))
run_case func      'def f(n):\n    return n * n\nprint(f(9))\n'                 '81'          || fails=$((fails+1))
# --- typed int params: raw slots, native body arithmetic, coercions ---
run_case typedsq   'def sq(n: int) -> int:\n    return n * n\nprint(sq(7))\n'   '49'          || fails=$((fails+1))
run_case typedfact 'def fact(n: int) -> int:\n    if n < 2:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n' '120' || fails=$((fails+1))
run_case typedboxarg 'def sq(n: int) -> int:\n    return n * n\nx = 6\nprint(sq(x))\n' '36'      || fails=$((fails+1))
run_case typedreassign 'def bump(n: int) -> int:\n    n = n + 1\n    return n\nprint(bump(41))\n' '42' || fails=$((fails+1))
run_case typedcmp  'def clamp(n: int) -> int:\n    if n < 0:\n        return 0\n    return n\nprint(clamp(-3))\nprint(clamp(5))\n' '0\n5' || fails=$((fails+1))
run_case annlocal  'x: int = 3 * 4\nprint(x + 1)\n'                              '13'          || fails=$((fails+1))
run_case whileloop 'def sum_to(n: int) -> int:\n    total: int = 0\n    i: int = 0\n    while i < n:\n        total = total + i\n        i = i + 1\n    return total\nprint(sum_to(5))\n' '10' || fails=$((fails+1))
run_case forsum    'def fsum(n: int) -> int:\n    total: int = 0\n    for i in range(n):\n        total = total + i\n    return total\nprint(fsum(5))\n' '10' || fails=$((fails+1))
run_case fordown   'for i in range(3, 0, -1):\n    print(i)\n'                   '3\n2\n1'      || fails=$((fails+1))
run_case lists     'xs = [1, 2, 3]\nxs.append(4)\nprint(xs)\nprint(len(xs))\n' '[1, 2, 3, 4]\n4' || fails=$((fails+1))
run_case strcat    'print("py" + "thon")\n'                                    'python'      || fails=$((fails+1))
run_case str_builtin 'print(str(42))\nprint(str([1, 2, 3]))\nprint(str(3.5))\n' '42\n[1, 2, 3]\n3.5' || fails=$((fails+1))
run_case fstring   'name = "world"\nn = 3\nprint(f"hi {name}, n={n}")\n'        'hi world, n=3' || fails=$((fails+1))
run_case fstring_expr 'x = 5\nprint(f"x*2 = {x * 2}")\n'                        'x*2 = 10'    || fails=$((fails+1))
run_case fstring_typed 'def sq(n: int) -> int:\n    return n * n\nprint(f"sq(4)={sq(4)}")\n' 'sq(4)=16' || fails=$((fails+1))
run_case slice_list 'xs = [1, 2, 3, 4, 5]\nprint(xs[1:4])\nprint(xs[:2])\nprint(xs[3:])\n' '[2, 3, 4]\n[1, 2]\n[4, 5]' || fails=$((fails+1))
run_case slice_str 's = "hello"\nprint(s[1:4])\nprint(s[::-1])\n'               'ell\nolleh'    || fails=$((fails+1))
run_case slice_step 'xs = [0, 1, 2, 3, 4, 5]\nprint(xs[::2])\nprint(xs[::-1])\n' '[0, 2, 4]\n[5, 4, 3, 2, 1, 0]' || fails=$((fails+1))
run_case slice_neg 'xs = [1, 2, 3, 4]\nprint(xs[-2:])\nprint(xs[:-1])\n'         '[3, 4]\n[1, 2, 3]' || fails=$((fails+1))
# --- RC stress: reassignment, nesting, dicts, early return, short-circuit, etc.
run_case reassign  'xs = [1, 2]\nxs = [3, 4, 5]\nprint(len(xs))\n'              '3'           || fails=$((fails+1))
run_case strreassign 's = "ab"\ns = s + "c"\nprint(s)\n'                        'abc'         || fails=$((fails+1))
run_case foreachstr 'for w in ["a", "b", "c"]:\n    print(w)\n'                 'a\nb\nc'      || fails=$((fails+1))
run_case nested    'xs = [[1], [2, 3]]\nprint(len(xs))\n'                       '2'           || fails=$((fails+1))
run_case dict      'd = {"a": 1, "b": 2}\nprint(d["a"])\nprint(len(d))\n'       '1\n2'        || fails=$((fails+1))
run_case dictupd   'd = {"a": 1}\nd["a"] = 2\nd["b"] = 3\nprint(d["a"])\nprint(d["b"])\n' '2\n3' || fails=$((fails+1))
run_case retlist   'def mk():\n    return [1, 2, 3]\nys = mk()\nprint(ys)\n'    '[1, 2, 3]'   || fails=$((fails+1))
run_case passheap  'def first(xs):\n    return xs[0]\nprint(first([9, 8, 7]))\n' '9'          || fails=$((fails+1))
run_case earlyret  'def find():\n    for i in range(10):\n        if i == 3:\n            return i\n    return -1\nprint(find())\n' '3' || fails=$((fails+1))
run_case earlyretheap 'def f(xs):\n    for x in xs:\n        return x\n    return "?"\nprint(f(["z", "y"]))\n' 'z' || fails=$((fails+1))
run_case shortcirc 'print("" or "x")\nprint("a" and "b")\n'                     'x\nb'        || fails=$((fails+1))
run_case concatloop 's = ""\nfor i in range(3):\n    s = s + "x"\nprint(s)\n'    'xxx'         || fails=$((fails+1))
run_case poptest   'xs = [1, 2, 3]\nv = xs.pop()\nprint(v)\nprint(len(xs))\n'    '3\n2'        || fails=$((fails+1))
run_case strlist   'names = ["amy", "bob"]\nnames.append("cy")\nfor n in names:\n    print(n)\n' 'amy\nbob\ncy' || fails=$((fails+1))
# --- borrowed params: a named heap value read by a helper must survive the call
run_case borrowarg 'def total(xs):\n    s = 0\n    for x in xs:\n        s = s + x\n    return s\nys = [1, 2, 3, 4]\nprint(total(ys))\nprint(len(ys))\n' '10\n4' || fails=$((fails+1))
run_case borrowtwice 'def total(xs):\n    s = 0\n    for x in xs:\n        s = s + x\n    return s\nys = [2, 3]\nprint(total(ys))\nprint(total(ys))\n' '5\n5' || fails=$((fails+1))
run_case escarg    'def echo(xs):\n    return xs\nzs = echo([5, 6])\nprint(zs)\n' '[5, 6]' || fails=$((fails+1))
run_case borrowstr 'def shout(s):\n    print(s)\nname = "hi"\nshout(name)\nprint(name)\n' 'hi\nhi' || fails=$((fails+1))

# --- FBIP drop-reuse: in-place map over a unique array; copy when aliased ---
run_case fbip_unique 'data: list[int] = [1, 2, 3]\ndata = [x * x for x in data]\nprint(data)\n' '[1, 4, 9]' || fails=$((fails+1))
run_case fbip_alias 'data: list[int] = [1, 2, 3]\nalias = data\ndata = [x * x for x in data]\nprint(data)\nprint(alias)\n' '[1, 4, 9]\n[1, 2, 3]' || fails=$((fails+1))
run_case fbip_float 'd: list[float] = [1.0, 2.0, 3.0]\nd = [v * 2.0 for v in d]\nprint(d)\n' '[2.0, 4.0, 6.0]' || fails=$((fails+1))
# --- list comprehensions ---
run_case comp_dyn  'xs: list[int] = [1, 2, 3]\nys = [x * x for x in xs]\nprint(ys)\n' '[1, 4, 9]' || fails=$((fails+1))
run_case comp_packed 'xs: list[int] = [1, 2, 3, 4]\nsq: list[int] = [x * x for x in xs]\nprint(sq)\nprint(len(sq))\n' '[1, 4, 9, 16]\n4' || fails=$((fails+1))
run_case comp_filter 'nums: list[int] = [1, 2, 3, 4, 5, 6]\nevens: list[int] = [n for n in nums if n % 2 == 0]\nprint(evens)\n' '[2, 4, 6]' || fails=$((fails+1))
run_case comp_range 'squares: list[int] = [i * i for i in range(5)]\nprint(squares)\n' '[0, 1, 4, 9, 16]' || fails=$((fails+1))
run_case comp_float 'data: list[float] = [x / 2 for x in range(4)]\nprint(data)\n' '[0.0, 0.5, 1.0, 1.5]' || fails=$((fails+1))
run_case dictcomp  'd = {x: x * x for x in range(4)}\nprint(d[2])\nprint(len(d))\n' '4\n4' || fails=$((fails+1))
run_case dictcomp_filter 'd = {n: n + 1 for n in range(6) if n % 2 == 0}\nprint(len(d))\nprint(d[4])\n' '3\n5' || fails=$((fails+1))
run_case dictcomp_str 'names = ["amy", "bo"]\nd = {n: len(n) for n in names}\nprint(d["amy"])\nprint(d["bo"])\n' '3\n2' || fails=$((fails+1))
run_case comp_typed_return 'def squares(n: int) -> list[int]:\n    return [i * i for i in range(n)]\nys: list[int] = squares(4)\nprint(ys)\nprint(len(ys))\n' '[0, 1, 4, 9]\n4' || fails=$((fails+1))
run_case comp_nested 'pairs: list[int] = [x * 10 + y for x in range(2) for y in range(3)]\nprint(pairs)\n' '[0, 1, 2, 10, 11, 12]' || fails=$((fails+1))
run_case comp_nested_filter 'd: list[int] = [x + y for x in range(3) for y in range(3) if x < y]\nprint(d)\n' '[1, 2, 3]' || fails=$((fails+1))
# --- packed int arrays (list[int]) ---
run_case iarray    'xs: list[int] = [10, 20, 30]\nprint(xs)\nprint(xs[1])\nprint(len(xs))\n' '[10, 20, 30]\n20\n3' || fails=$((fails+1))
run_case iarraysum 'def total(xs: list[int]) -> int:\n    s: int = 0\n    for x in xs:\n        s = s + x\n    return s\nys: list[int] = [1, 2, 3, 4]\nprint(total(ys))\nprint(len(ys))\n' '10\n4' || fails=$((fails+1))
run_case iarrayappend 'xs: list[int] = [1]\nxs.append(2)\nxs.append(3)\nprint(xs)\nprint(len(xs))\n' '[1, 2, 3]\n3' || fails=$((fails+1))
run_case iarrayset 'xs: list[int] = [5, 6, 7]\nxs[1] = 99\nprint(xs)\nprint(xs[-1])\n' '[5, 99, 7]\n7' || fails=$((fails+1))
run_case iarrayliteralarg 'def first(xs: list[int]) -> int:\n    return xs[0]\nprint(first([42, 7]))\n' '42' || fails=$((fails+1))
run_case farray    'xs: list[float] = [1.5, 2.5, 3.0]\nprint(xs)\nprint(xs[1])\nprint(len(xs))\n' '[1.5, 2.5, 3.0]\n2.5\n3' || fails=$((fails+1))
run_case farraysum 'def total(xs: list[float]) -> float:\n    s: float = 0.0\n    for x in xs:\n        s = s + x\n    return s\nys: list[float] = [1.5, 2.5]\nprint(total(ys))\n' '4.0' || fails=$((fails+1))
run_case farraymix 'xs: list[float] = [1.0, 2.0]\nxs.append(3)\nxs[0] = 9.5\nprint(xs)\n' '[9.5, 2.0, 3.0]' || fails=$((fails+1))

# --- sets + set theory (A = B & C) ---
run_case set_intersect 'B = {1, 2, 3, 4}\nC = {3, 4, 5, 6}\nA = B & C\nprint(A)\n' '{3, 4}' || fails=$((fails+1))
run_case set_union 'print({1, 2} | {2, 3})\n'                                   '{1, 2, 3}'   || fails=$((fails+1))
run_case set_diff  'print({1, 2, 3} - {2})\n'                                   '{1, 3}'      || fails=$((fails+1))
run_case set_symdiff 'print({1, 2, 3} ^ {2, 3, 4})\n'                           '{1, 4}'      || fails=$((fails+1))
run_case set_member 's = {1, 2, 3}\nprint(2 in s)\nprint(9 not in s)\n'         'True\nTrue'  || fails=$((fails+1))
run_case set_dedup 'print(len({1, 1, 2, 3, 3, 3}))\n'                           '3'           || fails=$((fails+1))
run_case set_iter 'total = 0\nfor x in {10, 20, 30}:\n    total = total + x\nprint(total)\n' '60' || fails=$((fails+1))
run_case substr_in 'print("ll" in "hello")\nprint("z" in "hello")\n'           'True\nFalse' || fails=$((fails+1))
run_case int_bitwise 'print(6 & 3)\nprint(5 | 2)\n'                             '2\n7'        || fails=$((fails+1))
run_case set_add   's = {1, 2}\ns.add(3)\ns.add(2)\nprint(len(s))\nprint(3 in s)\n' '3\nTrue' || fails=$((fails+1))
run_case set_remove 's = {1, 2, 3}\ns.remove(2)\ns.discard(9)\nprint(len(s))\nprint(2 in s)\n' '2\nFalse' || fails=$((fails+1))
run_case set_methods 'a = {1, 2, 3}\nb = {2, 3, 4}\nprint(len(a.union(b)))\nprint(len(a.intersection(b)))\nprint(a.issubset({1, 2, 3, 4}))\n' '4\n2\nTrue' || fails=$((fails+1))
run_case set_copy_clear 's = {1, 2, 3}\nt = s.copy()\ns.clear()\nprint(len(s))\nprint(len(t))\n' '0\n3' || fails=$((fails+1))
run_case set_pop   's = {5}\nx = s.pop()\nprint(x)\nprint(len(s))\n'             '5\n0'        || fails=$((fails+1))
# --- tuples (a distinct, immutable type) ---
run_case tuple_print 't = (1, 2, 3)\nprint(t)\n' '(1, 2, 3)' || fails=$((fails+1))
run_case tuple_single 't = (5,)\nprint(t)\n' '(5,)' || fails=$((fails+1))
run_case tuple_membership 't = (1, 2, 3)\nprint(2 in t)\nprint(9 in t)\n' 'True\nFalse' || fails=$((fails+1))
run_case tuple_iter 't = (1, 2, 3)\ns = 0\nfor x in t:\n    s = s + x\nprint(s)\n' '6' || fails=$((fails+1))
run_case tuple_in_list 'xs = [(1, 2), (3, 4)]\nprint(xs)\n' '[(1, 2), (3, 4)]' || fails=$((fails+1))
run_case tuple_unpack 't = (1, 2, 3)\na, b, c = t\nprint(a)\nprint(c)\n' '1\n3' || fails=$((fails+1))
run_case tuple_swap 'a = 1\nb = 2\na, b = b, a\nprint(a)\nprint(b)\n' '2\n1' || fails=$((fails+1))
run_case tuple_return 'def minmax(x: int, y: int):\n    if x < y:\n        return x, y\n    return y, x\nlo, hi = minmax(5, 3)\nprint(lo)\nprint(hi)\n' '3\n5' || fails=$((fails+1))
run_case tuple_index 'pt = (10, 20, 30)\nprint(pt[1])\nprint(len(pt))\n' '20\n3' || fails=$((fails+1))
run_case comp_tuple_target 'pairs = [(1, 10), (2, 20), (3, 30)]\nsums: list[int] = [a + b for a, b in pairs]\nprint(sums)\n' '[11, 22, 33]' || fails=$((fails+1))

echo "---"
if [ "$fails" -eq 0 ]; then
  echo "all native-run cases passed"
else
  echo "$fails case(s) FAILED"
fi
exit "$fails"
