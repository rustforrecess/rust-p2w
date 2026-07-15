#!/usr/bin/env bash
# The 5e-b showcase: EVERY builtin component executes under a real component
# host, not just Grid. Poll exercises the canonical string-RETURN machinery
# (get-field flows host -> guest through the retptr + cabi_realloc), Draw
# exercises add-element + the pointer readers, Open response exercises
# get-value + evidence. Each ends with dispose() -> live == 0.
# (Grid, incl. the dispose contract in detail, lives in component_grid.sh.)
set -u
cd "$(dirname "$0")/.." || exit 1

command -v npx >/dev/null 2>&1 || { echo "SKIP: npx not found"; exit 0; }
fails=0

# ---- stamped sources ({id} = the fixed instance ids) -----------------------
mkdir -p target/component

cat > target/component/poll.py <<'EOF'
def poll_bump(option: str):
    count = get_field("poll_" + option)
    if count == "":
        count = "0"
    n = int(count) + 1
    set_field("poll_" + option, str(n))
    set_text("#poll_count_" + option, str(n))

def poll_vote_a():
    poll_bump("a")

def poll_vote_b():
    poll_bump("b")

on("#poll_a", "click", poll_vote_a)
on("#poll_b", "click", poll_vote_b)
EOF

cat > target/component/draw.py <<'EOF'
def draw_dot():
    n = get_field("draw_n")
    if n == "":
        n = "0"
    dot = "draw_d" + n
    add_element("#draw", "circle", dot)
    set_attr("#" + dot, "cx", str(pointer_x()))
    set_attr("#" + dot, "cy", str(pointer_y()))
    set_attr("#" + dot, "r", "2")
    set_field("draw_n", str(int(n) + 1))

on("#draw", "mousedown", draw_dot)
EOF

cat > target/component/open_response.py <<'EOF'
def open_response_submit():
    answer = get_value("#open_response_text")
    set_field("open_response_answer", answer)
    evidence("open_response", answer)
    set_text("#open_response_status", "Saved!")

on("#open_response_send", "click", open_response_submit)
EOF

# ---- build all three -------------------------------------------------------
bash tools/componentize.sh target/component/poll.py poll bump || fails=$((fails+1))
bash tools/componentize.sh target/component/draw.py draw dot || fails=$((fails+1))
bash tools/componentize.sh target/component/open_response.py open_response submit || fails=$((fails+1))
[ "$fails" = 0 ] || { echo "FAIL: build(s) failed"; exit "$fails"; }

# ---- execute each under jco -------------------------------------------------
run_component() {
  local name="$1" hostjs="$2" driver="$3"
  local out="target/component/$name"
  ( cd "$out" && npx --yes @bytecodealliance/jco transpile "$name.component.wasm" \
      -o jco --map "acorn:component/host=./host.js" >jco.log 2>&1 ) || {
    echo "FAIL [$name]: jco transpile"; tail -5 "$out/jco.log"; return 1; }
  printf '%s' "$hostjs" > "$out/jco/host.js"
  printf '%s' "$driver" > "$out/jco/driver.mjs"
  node "$out/jco/driver.mjs" || return 1
}

run_component poll "$(cat <<'EOF'
export const fields = {};
export const texts = [];
export function p2wPutc(b) {}
export function getField(key) { return fields[key] ?? ""; }
export function setField(key, value) { fields[key] = value; }
export function setText(sel, text) { texts.push([sel, text]); }
EOF
)" "$(cat <<'EOF'
import { bump, live, dispose } from './poll.component.js';
import { fields, texts } from './host.js';
bump("a"); bump("a"); bump("b");
if (fields.poll_a !== "2" || fields.poll_b !== "1") {
  console.error(`FAIL [poll]: fields ${JSON.stringify(fields)}`); process.exit(1);
}
const last = texts[texts.length - 2] ?? [];
if (JSON.stringify(texts).indexOf('["#poll_count_a","2"]') < 0) {
  console.error(`FAIL [poll]: texts ${JSON.stringify(texts)}`); process.exit(1);
}
dispose();
if (live() !== 0) { console.error(`FAIL [poll]: live=${live()} after dispose`); process.exit(1); }
console.log("PASS [poll-exec]  get-field string returns crossed host->guest; counts correct; dispose -> live=0");
EOF
)" || fails=$((fails+1))

run_component draw "$(cat <<'EOF'
export const fields = {};
export const els = [];
export const attrs = [];
export function p2wPutc(b) {}
export function getField(key) { return fields[key] ?? ""; }
export function setField(key, value) { fields[key] = value; }
export function addElement(parent, tag, id) { els.push([parent, tag, id]); }
export function setAttr(sel, name, value) { attrs.push([sel, name, value]); }
export function pointerX() { return 42; }
export function pointerY() { return 17; }
EOF
)" "$(cat <<'EOF'
import { dot, live, dispose } from './draw.component.js';
import { fields, els, attrs } from './host.js';
dot(); dot();
const wantEls = JSON.stringify([["#draw", "circle", "draw_d0"], ["#draw", "circle", "draw_d1"]]);
if (JSON.stringify(els) !== wantEls) {
  console.error(`FAIL [draw]: els ${JSON.stringify(els)}`); process.exit(1);
}
if (JSON.stringify(attrs).indexOf('["#draw_d0","cx","42"]') < 0
 || JSON.stringify(attrs).indexOf('["#draw_d0","cy","17"]') < 0) {
  console.error(`FAIL [draw]: attrs ${JSON.stringify(attrs)}`); process.exit(1);
}
if (fields.draw_n !== "2") { console.error(`FAIL [draw]: n=${fields.draw_n}`); process.exit(1); }
dispose();
if (live() !== 0) { console.error(`FAIL [draw]: live=${live()} after dispose`); process.exit(1); }
console.log("PASS [draw-exec]  add-element + pointer readers crossed the boundary; dispose -> live=0");
EOF
)" || fails=$((fails+1))

run_component open_response "$(cat <<'EOF'
export const fields = {};
export const texts = [];
export const evidenceLog = [];
export function p2wPutc(b) {}
export function getValue(sel) { return sel === "#open_response_text" ? "my answer" : ""; }
export function setField(key, value) { fields[key] = value; }
export function setText(sel, text) { texts.push([sel, text]); }
export function evidence(key, value) { evidenceLog.push([key, value]); }
EOF
)" "$(cat <<'EOF'
import { submit, live, dispose } from './open_response.component.js';
import { fields, texts, evidenceLog } from './host.js';
submit();
if (fields.open_response_answer !== "my answer") {
  console.error(`FAIL [open]: fields ${JSON.stringify(fields)}`); process.exit(1);
}
if (JSON.stringify(evidenceLog) !== JSON.stringify([["open_response", "my answer"]])) {
  console.error(`FAIL [open]: evidence ${JSON.stringify(evidenceLog)}`); process.exit(1);
}
if (JSON.stringify(texts).indexOf('["#open_response_status","Saved!"]') < 0) {
  console.error(`FAIL [open]: texts ${JSON.stringify(texts)}`); process.exit(1);
}
dispose();
if (live() !== 0) { console.error(`FAIL [open]: live=${live()} after dispose`); process.exit(1); }
console.log("PASS [open-exec]  get-value + evidence crossed the boundary; dispose -> live=0");
EOF
)" || fails=$((fails+1))

echo
[ "$fails" = 0 ] && echo "ALL BUILTIN COMPONENTS EXECUTE: Poll, Draw, Open response (+ Grid via component_grid.sh)." \
                 || echo "component_all: $fails failure(s)."
exit "$fails"
