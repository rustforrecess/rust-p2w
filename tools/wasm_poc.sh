#!/usr/bin/env bash
# PROOF OF CONCEPT: compiled Python -> linear-memory wasm32 (NO WASM-GC), run in a
# real wasm VM. This is the "non-JS guest / PXC component" claim made real: the
# LLVM/p2w-rt backend (i32 arena offsets, RC — the Pico value model) retargeted to
# wasm32 instead of the host, then run via Node's WebAssembly API (same engine path
# as the browser) with a host-provided `p2w_putc` import — exactly like the native
# oracle's putc.c. Output is diffed against CPython.
#
# Requires: clang+wasm-ld (LLVM), the wasm32-unknown-unknown Rust target, node.
set -u
cd "$(dirname "$0")/.." || exit 1
export RUSTC_WRAPPER=''
OUT=target/wasmpoc
mkdir -p "$OUT"

for t in clang wasm-ld node; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: $t not found"; exit 0; }; done

echo "building p2w-rt as a wasm32 staticlib (no_std, panic=abort)…"
cargo rustc --manifest-path runtime/Cargo.toml --target wasm32-unknown-unknown \
  --release --crate-type staticlib -- -C panic=abort >"$OUT/rtbuild.log" 2>&1 || {
  echo "FAIL: runtime wasm32 staticlib build"; tail -20 "$OUT/rtbuild.log"; exit 1; }
LIB=$(cargo metadata --manifest-path runtime/Cargo.toml --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/wasm32-unknown-unknown/release/libp2w_rt.a
[ -f "$LIB" ] || LIB=$(dirname "$LIB")/p2w_rt.a
[ -f "$LIB" ] || { echo "FAIL: wasm staticlib not found"; ls "$(dirname "$LIB")" 2>/dev/null; exit 1; }
echo "staticlib: $LIB"

# Node host: instantiate the .wasm, provide p2w_putc, call _start, print output.
cat > "$OUT/run.cjs" <<'EOF'
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const out = [];
const env = { p2w_putc: (c) => out.push(c & 0xff) };
WebAssembly.instantiate(bytes, { env }).then(({ instance }) => {
  const entry = instance.exports.main || instance.exports._start;
  if (typeof entry !== 'function') throw new Error('no main/_start export');
  entry();
  process.stdout.write(Buffer.from(out).toString('utf8'));
  // Memory correctness: our RC runtime should have freed everything (no GC).
  const live = instance.exports.p2w_live ? instance.exports.p2w_live() : 'n/a';
  process.stderr.write('P2W_LIVE=' + live + '\n');
}).catch((e) => { console.error('WASM-RUN-ERROR:', e.message); process.exit(2); });
EOF

run_case() {
  local name="$1" src="$2" want="$3"
  printf '%b' "$src" | cargo run -q --example emit_ll > "$OUT/$name.ll" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: emit_ll"; cat "$OUT/$name.err"; return 1; }
  # LLVM IR -> wasm32, linked against the runtime; p2w_putc stays an import (env).
  clang --target=wasm32 -Wno-override-module -nostdlib -O2 \
    -Wl,--no-entry -Wl,--export=main -Wl,--export=p2w_live -Wl,--export=p2w_allocs -Wl,--allow-undefined \
    "$OUT/$name.ll" "$LIB" -o "$OUT/$name.wasm" 2>"$OUT/$name.err" || {
    echo "FAIL [$name]: clang/wasm-ld"; cat "$OUT/$name.err"; return 1; }
  local got; got=$(node "$OUT/run.cjs" "$OUT/$name.wasm" 2>"$OUT/$name.runerr" | tr -d '\r')
  if grep -q WASM-RUN-ERROR "$OUT/$name.runerr"; then
    echo "FAIL [$name]: $(cat "$OUT/$name.runerr")"; return 1; fi
  local live; live=$(sed -n 's/.*P2W_LIVE=\(-\?[0-9]*\).*/\1/p' "$OUT/$name.runerr"); : "${live:=?}"
  local size; size=$(wc -c < "$OUT/$name.wasm")
  if [ "$got" != "$(printf '%b' "$want")" ]; then
    echo "FAIL [$name]: got [$got] want [$(printf '%b' "$want")]  live=$live"; return 1; fi
  if [ "$live" != "0" ]; then
    echo "LEAK [$name]: output ok but live=$live (RC should free everything)"; return 1; fi
  echo "PASS [$name]  live=0  (${size} byte .wasm)"; return 0
}

fails=0
run_case ints  'print(6 * 7)\nprint(10 - 3)\n'                              '42\n7'  || fails=$((fails+1))
run_case loop  'total = 0\nfor i in range(5):\n    total = total + i\nprint(total)\n' '10' || fails=$((fails+1))
run_case heap  'xs = [1, 2, 3]\nxs.append(4)\nt = 0\nfor x in xs:\n    t = t + x\nprint(t)\nprint("done")\n' '10\ndone' || fails=$((fails+1))
run_case strs  'print("py" + "thon")\n'                                     'python' || fails=$((fails+1))

echo
[ "$fails" = 0 ] && echo "PoC OK: compiled Python ran as linear-memory wasm32 (no WASM-GC) in a real VM." \
                 || echo "PoC: $fails case(s) failed."
exit "$fails"
