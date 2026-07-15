#!/usr/bin/env bash
# The str-return showcase: a component that COMPUTES and returns a string to
# the host (the compute-and-report class — e.g. a scorer or formatter). This
# exercises the canonical return-area + cabi_post cleanup path: the export
# returns a pointer to [ptr, len] over the p2w string's bytes, and the host
# calls cabi_post_<name> after copying the string out, where the guest frees
# the value. If cabi_post is wrong, the string leaks and dispose won't hit 0.
set -u
cd "$(dirname "$0")/.." || exit 1

command -v npx >/dev/null 2>&1 || { echo "SKIP: npx not found"; exit 0; }

OUT=target/component/fmt
mkdir -p "$OUT"
cat > "$OUT/fmt.py" <<'EOF'
def fmt_label(n: int) -> str:
    return "n=" + str(n)

def fmt_grade(score: int) -> str:
    if score >= 90:
        return "A"
    if score >= 80:
        return "B"
    return "C"
EOF

bash tools/componentize.sh "$OUT/fmt.py" fmt label,grade || exit 1
[ -f "$OUT/fmt.component.wasm" ] || { echo "SKIP: no component built"; exit 0; }

( cd "$OUT" && npx --yes @bytecodealliance/jco transpile fmt.component.wasm \
    -o jco --map 'acorn:component/host=./host.js' >jco.log 2>&1 ) || {
  echo "FAIL: jco transpile"; tail -5 "$OUT/jco.log"; exit 1; }

cat > "$OUT/jco/host.js" <<'EOF'
export function p2wPutc(b) {}
EOF

cat > "$OUT/jco/driver.mjs" <<'EOF'
import { label, grade, live, dispose } from './fmt.component.js';

// Strings COMPUTED in the component and returned to the host.
const a = label(5), b = label(42);
if (a !== "n=5" || b !== "n=42") {
  console.error(`FAIL: label returns wrong: ${JSON.stringify([a, b])}`); process.exit(1);
}
if (grade(95) !== "A" || grade(85) !== "B" || grade(70) !== "C") {
  console.error(`FAIL: grade returns wrong`); process.exit(1);
}

// The cleanup contract: cabi_post frees each returned string, so many calls
// don't grow live, and dispose lands at 0.
for (let i = 0; i < 50; i++) label(i);
const warm = live();
for (let i = 0; i < 50; i++) grade(i);
if (live() !== warm) {
  console.error(`FAIL: live grew ${warm} -> ${live()} — cabi_post isn't freeing returned strings`);
  process.exit(1);
}
dispose();
if (live() !== 0) { console.error(`FAIL: live=${live()} after dispose`); process.exit(1); }
console.log(`PASS [fmt-return]  computed strings crossed guest->host; cabi_post freed them (live steady ${warm}); dispose -> live=0`);
EOF

node "$OUT/jco/driver.mjs" || exit 1
echo "str-return OK: a component that computes and returns strings runs under a real host, leak-free."
