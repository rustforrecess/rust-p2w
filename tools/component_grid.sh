#!/usr/bin/env bash
# The Grid showcase: the first STUDENT-SHAPED component through the 5e
# converter, executed in a real component host (jco/Node) with the capability
# provided host-side. Proves the whole contract: stamped Python -> clean gate
# -> WIT from annotations -> canonical shim -> component -> set(0, 0, "hi")
# reaches the HOST as set-text("#grid_0_0", "hi"), and live == 0 after.
set -u
cd "$(dirname "$0")/.." || exit 1

OUT=target/component/grid
mkdir -p "$OUT"

# The builtin Grid component's stamped code (userwidgets.rs, {id} = grid).
cat > "$OUT/grid.py" <<'EOF'
def grid_set(row: int, col: int, value: str):
    set_text("#grid_" + str(row) + "_" + str(col), value)

def grid_show(data: list[list[int]]):
    for r in range(len(data)):
        for c in range(len(data[r])):
            grid_set(r, c, str(data[r][c]))
EOF

bash tools/componentize.sh "$OUT/grid.py" grid set,show || exit 1
[ -f "$OUT/grid.component.wasm" ] || { echo "SKIP: no component built"; exit 0; }

command -v npx >/dev/null 2>&1 || { echo "SKIP exec: npx not found"; exit 0; }
echo; echo "=== component EXECUTION (jco host) ==="
( cd "$OUT" && npx --yes @bytecodealliance/jco transpile grid.component.wasm \
    -o jco --map 'acorn:component/host=./host.js' >jco.log 2>&1 ) || {
  echo "FAIL: jco transpile"; tail -5 "$OUT/jco.log"; exit 1; }

cat > "$OUT/jco/host.js" <<'EOF'
// Host side of acorn:component/host — record what the component asks for.
export const calls = [];
export const out = [];
export function p2wPutc(byte) { out.push(byte & 0xff); }
export function setText(selector, text) { calls.push([selector, text]); }
EOF

cat > "$OUT/jco/driver.mjs" <<'EOF'
import { set, show, live, dispose } from './grid.component.js';
import { calls } from './host.js';
set(0, 0, "hi");
set(2, 1, "42");
const want = JSON.stringify([["#grid_0_0", "hi"], ["#grid_2_1", "42"]]);
const got = JSON.stringify(calls);
if (got !== want) { console.error(`FAIL: got ${got} want ${want}`); process.exit(1); }

// LIST PARAMS: show(data: list[list[int]]) — a nested list crosses the
// boundary; the host passes [[...],[...]], the shim marshals it into a p2w
// list-of-lists, and show fills all nine cells via set-text.
calls.length = 0;
show([[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
const grid = {};
for (const [sel, text] of calls) grid[sel] = text;
if (grid["#grid_0_0"] !== "1" || grid["#grid_1_2"] !== "6" || grid["#grid_2_2"] !== "9"
    || calls.length !== 9) {
  console.error(`FAIL: show() nested-list marshalling wrong: ${JSON.stringify(calls)}`);
  process.exit(1);
}
console.log("  show([[…]]) filled 9 cells — nested list<list<s32>> crossed the boundary");

// Memory contract for a RESIDENT component, both halves:
// 1) STEADY STATE while running — the per-call-site literal caches stay
//    warm by design, but live must not grow per call.
const warm = live();
for (let i = 0; i < 50; i++) set(i % 3, i % 3, "x" + i);
if (live() !== warm) {
  console.error(`FAIL: live grew ${warm} -> ${live()} across 50 calls (per-call leak)`);
  process.exit(1);
}
// 2) live == 0 AT TEARDOWN — dispose() frees what main's exit epilogue
//    would have (a component never runs main), and resets the cache slots
//    so the component still works after; a second teardown returns to 0.
dispose();
if (live() !== 0) { console.error(`FAIL: live=${live()} after dispose()`); process.exit(1); }
set(1, 1, "post");
if (calls[calls.length - 1][0] !== "#grid_1_1") {
  console.error("FAIL: set() broken after dispose()"); process.exit(1);
}
dispose();
if (live() !== 0) { console.error(`FAIL: live=${live()} after second dispose()`); process.exit(1); }
console.log(`PASS [grid-exec]  set-text crossed the boundary; live steady at ${warm} across 52 calls; dispose() -> live=0, still usable, redispose -> 0`);
EOF

node "$OUT/jco/driver.mjs" || exit 1
echo "Grid showcase OK: a stamped Python component EXECUTES under a real component host."
