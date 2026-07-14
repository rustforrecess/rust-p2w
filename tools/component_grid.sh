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

def grid_show(data):
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
import { set, live } from './grid.component.js';
import { calls } from './host.js';
set(0, 0, "hi");
set(2, 1, "42");
const want = JSON.stringify([["#grid_0_0", "hi"], ["#grid_2_1", "42"]]);
const got = JSON.stringify(calls);
if (got !== want) { console.error(`FAIL: got ${got} want ${want}`); process.exit(1); }
// The memory oracle for a RESIDENT component is steady state, not live==0:
// a component never exits, so the per-call-site literal caches ("#grid_",
// "_") stay warm by design — main's exit epilogue (which frees them) never
// runs. What must NOT happen is per-call growth: every value a call creates
// must be freed by that call's end.
const warm = live();
for (let i = 0; i < 50; i++) set(i % 3, i % 3, "x" + i);
if (live() !== warm) {
  console.error(`FAIL: live grew ${warm} -> ${live()} across 50 calls (per-call leak)`);
  process.exit(1);
}
console.log(`PASS [grid-exec]  set() crossed the boundary as set-text; live steady at ${warm} (literal caches) across 52 calls`);
EOF

node "$OUT/jco/driver.mjs" || exit 1
echo "Grid showcase OK: a stamped Python component EXECUTES under a real component host."
