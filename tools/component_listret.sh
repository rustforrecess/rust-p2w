#!/usr/bin/env bash
# The list-return showcase: a component that COMPUTES and returns lists to the
# host (the compute-and-report class extended to sequences). Exercises the
# canonical list-RESULT lowering: the guest reads its p2w list into a canonical
# (ptr, len) buffer, hands it back via the return area, and cabi_post frees the
# list after the host has copied it out (string elements point into it).
set -u
cd "$(dirname "$0")/.." || exit 1

command -v npx >/dev/null 2>&1 || { echo "SKIP: npx not found"; exit 0; }

OUT=target/component/seq
mkdir -p "$OUT"
cat > "$OUT/seq.py" <<'EOF'
def seq_nums(a: int, b: int, c: int) -> list[int]:
    return [a, b, c]

def seq_words() -> list[str]:
    return ["hi", "yo", "sup"]
EOF

bash tools/componentize.sh "$OUT/seq.py" seq nums,words || exit 1
[ -f "$OUT/seq.component.wasm" ] || { echo "SKIP: no component built"; exit 0; }

( cd "$OUT" && npx --yes @bytecodealliance/jco transpile seq.component.wasm \
    -o jco --map 'acorn:component/host=./host.js' >jco.log 2>&1 ) || {
  echo "FAIL: jco transpile"; tail -5 "$OUT/jco.log"; exit 1; }

cat > "$OUT/jco/host.js" <<'EOF'
export function p2wPutc(b) {}
EOF

cat > "$OUT/jco/driver.mjs" <<'EOF'
import { nums, words, live, dispose } from './seq.component.js';

const a = nums(4, 5, 6);
if (JSON.stringify([...a]) !== JSON.stringify([4, 5, 6])) {
  console.error(`FAIL: nums returned ${JSON.stringify([...a])}`); process.exit(1);
}
const w = words();
if (JSON.stringify([...w]) !== JSON.stringify(["hi", "yo", "sup"])) {
  console.error(`FAIL: words returned ${JSON.stringify([...w])}`); process.exit(1);
}

// The cleanup contract: cabi_post frees each returned list, so repeated calls
// don't grow live, and dispose lands at 0.
for (let i = 0; i < 50; i++) { nums(i, i, i); words(); }
const warm = live();
for (let i = 0; i < 50; i++) { nums(i, i, i); words(); }
if (live() !== warm) {
  console.error(`FAIL: live grew ${warm} -> ${live()} — cabi_post isn't freeing returned lists`);
  process.exit(1);
}
dispose();
if (live() !== 0) { console.error(`FAIL: live=${live()} after dispose`); process.exit(1); }
console.log(`PASS [seq-return]  computed list<s32> + list<string> crossed guest->host; cabi_post freed them (live steady ${warm}); dispose -> live=0`);
EOF

node "$OUT/jco/driver.mjs" || exit 1
echo "list-return OK: a component that computes and returns lists runs under a real host, leak-free."
