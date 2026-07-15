#!/usr/bin/env bash
# The 5e-c showcase: an INTERACTIVE component's event wiring survives
# conversion and the exported handlers are HOST-CALLABLE. A WIT component has
# no DOM, so top-level `on("#poll_a", "click", poll_vote_a)` becomes a wiring
# manifest (wiring.json) + a no-arg export the host calls when the event
# fires. Here the driver plays "host": it reads the manifest, then calls the
# named export as if a click arrived, and checks the component's state moved.
set -u
cd "$(dirname "$0")/.." || exit 1

command -v npx >/dev/null 2>&1 || { echo "SKIP: npx not found"; exit 0; }

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

bash tools/componentize.sh target/component/poll.py poll bump || exit 1
OUT=target/component/poll
[ -f "$OUT/wiring.json" ] || { echo "FAIL: no wiring.json emitted"; exit 1; }
echo "--- wiring.json ---"; cat "$OUT/wiring.json"; echo

( cd "$OUT" && npx --yes @bytecodealliance/jco transpile poll.component.wasm \
    -o jco --map 'acorn:component/host=./host.js' >jco.log 2>&1 ) || {
  echo "FAIL: jco transpile"; tail -5 "$OUT/jco.log"; exit 1; }

cat > "$OUT/jco/host.js" <<'EOF'
export const fields = {};
export const texts = [];
export function p2wPutc(b) {}
export function getField(k) { return fields[k] ?? ""; }
export function setField(k, v) { fields[k] = v; }
export function setText(s, t) { texts.push([s, t]); }
EOF

cat > "$OUT/jco/driver.mjs" <<'EOF'
import { readFileSync } from 'node:fs';
import * as poll from './poll.component.js';
import { fields } from './host.js';

// The host reads the wiring manifest and builds its listener table. jco
// camelCases kebab exports (poll-vote-a -> pollVoteA), so map through that.
const camel = s => s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
const wiring = JSON.parse(readFileSync(new URL('../wiring.json', import.meta.url)));
const table = wiring.map(w => ({ ...w, fn: poll[camel(w.handler)] }));

// Manifest sanity: #poll_a click is wired to a real, callable export.
const a = table.find(w => w.selector === '#poll_a' && w.event === 'click');
if (!a || typeof a.fn !== 'function') {
  console.error(`FAIL: #poll_a click not wired to a callable export: ${JSON.stringify(wiring)}`);
  process.exit(1);
}

// "The host received a click on #poll_a" — dispatch through the table twice,
// then a #poll_b click once. Component state must move accordingly.
const dispatch = (sel, ev) => {
  const w = table.find(w => w.selector === sel && w.event === ev);
  if (!w) { console.error(`no wiring for ${sel} ${ev}`); process.exit(1); }
  w.fn();
};
dispatch('#poll_a', 'click');
dispatch('#poll_a', 'click');
dispatch('#poll_b', 'click');

if (fields.poll_a !== '2' || fields.poll_b !== '1') {
  console.error(`FAIL: counts wrong after wired clicks: ${JSON.stringify(fields)}`);
  process.exit(1);
}
poll.dispose();
if (poll.live() !== 0) { console.error(`FAIL: live=${poll.live()} after dispose`); process.exit(1); }
console.log("PASS [poll-events]  on() wiring survived conversion; host drove exports via the manifest; dispose -> live=0");
EOF

node "$OUT/jco/driver.mjs" || exit 1
echo "5e-c OK: an interactive component's events cross the boundary as a wiring manifest + host-callable exports."
