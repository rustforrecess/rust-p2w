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
[ "$fails" = 0 ] && echo "Core PoC OK: compiled Python ran as linear-memory wasm32 (no WASM-GC) in a real VM." \
                 || echo "Core PoC: $fails case(s) failed."

# --- Component step: wrap the SAME compiled Python as a real Component-Model
# component (the PXC-guest shape). wasm-tools needs canonical-ABI import/export
# names, so a tiny shim renames the host import to a WIT interface and exports a
# no-arg `run`. Proves: compiled Python -> a valid component implementing a WIT
# (imports a capability, exports an entry) — and then EXECUTES it in a real
# component host (jco/Node, the Bytecode Alliance's JS host), the capability
# provided host-side, asserting output + live==0 end to end.
if command -v wasm-tools >/dev/null 2>&1 && [ -f "$OUT/heap.ll" ]; then
  echo; echo "=== component step (heap case) ==="
  cat > "$OUT/poc.wit" <<'EOF'
package poc:guest;
interface env {
  p2w-putc: func(byte: s32);
}
world guest {
  import env;
  export run: func() -> s32;
  export live: func() -> s32;
}
EOF
  cat > "$OUT/shim.c" <<'EOF'
__attribute__((import_module("poc:guest/env"), import_name("p2w-putc")))
extern void canon_putc(int c);
void p2w_putc(int c) { canon_putc(c); }
int p2w_getc(void) { return -1; } /* component PoC: no input stream yet */
extern int main(void);
extern int p2w_live(void);
__attribute__((export_name("run"))) int run(void) { return main(); }
__attribute__((export_name("live"))) int live(void) { return p2w_live(); }
EOF
  if clang --target=wasm32 -Wno-override-module -nostdlib -O1 -Wl,--no-entry \
        -Wl,--export=run -Wl,--export=live \
        "$OUT/heap.ll" "$OUT/shim.c" "$LIB" -o "$OUT/heapc.core.wasm" 2>"$OUT/comp.err" \
     && wasm-tools component embed "$OUT/poc.wit" --world guest "$OUT/heapc.core.wasm" \
        -o "$OUT/heapc.embed.wasm" 2>>"$OUT/comp.err" \
     && wasm-tools component new "$OUT/heapc.embed.wasm" -o "$OUT/heap.component.wasm" 2>>"$OUT/comp.err" \
     && wasm-tools validate --features component-model "$OUT/heap.component.wasm" 2>>"$OUT/comp.err"; then
    echo "PASS [component]  valid Component-Model component ($(wc -c < "$OUT/heap.component.wasm") bytes)"
    echo "--- its WIT ---"; wasm-tools component wit "$OUT/heap.component.wasm" 2>/dev/null
  else
    echo "FAIL [component]:"; tail -8 "$OUT/comp.err"; fails=$((fails+1))
  fi

  # Execute the component. First run downloads jco via npx (cached after);
  # skip gracefully when npx is unavailable.
  if command -v npx >/dev/null 2>&1 && [ -f "$OUT/heap.component.wasm" ]; then
    echo; echo "=== component EXECUTION (jco host) ==="
    ( cd "$OUT" && npx --yes @bytecodealliance/jco transpile heap.component.wasm \
        -o jco --map 'poc:guest/env=./env.js' >jco.log 2>&1 ) || {
      echo "FAIL [component-exec]: jco transpile"; tail -5 "$OUT/jco.log"; fails=$((fails+1)); exit "$fails"; }
    cat > "$OUT/jco/env.js" <<'EOF'
// Host side of the poc:guest/env capability: buffer the activity's output bytes.
export const out = [];
export function p2wPutc(byte) { out.push(byte & 0xff); }
EOF
    cat > "$OUT/jco/driver.mjs" <<'EOF'
import { run, live } from './heap.component.js';
import { out } from './env.js';
const code = run();
process.stdout.write(Buffer.from(out).toString('utf8'));
console.error(`exit=${code} P2W_LIVE=${live()}`);
EOF
    got=$(node "$OUT/jco/driver.mjs" 2>"$OUT/jco/run.err" | tr -d '\r')
    live=$(sed -n 's/.*P2W_LIVE=\(-\?[0-9]*\).*/\1/p' "$OUT/jco/run.err"); : "${live:=?}"
    want=$(printf '10\ndone')
    if [ "$got" = "$want" ] && [ "$live" = "0" ]; then
      echo "PASS [component-exec]  output matches CPython, live=0 — the component RUNS"
    else
      echo "FAIL [component-exec]: got [$got] live=$live"; fails=$((fails+1))
    fi
  fi
fi

exit "$fails"
