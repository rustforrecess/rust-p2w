#!/bin/bash
# The Mojo-bridge differential (docs/MOJO_BRIDGE.md): every program in
# tests/mojo_bridge/ that `p2w check --profile mojo` calls READY must compile
# and run under the REAL Mojo compiler — prelude prepended, statements
# wrapped in def main() by tools/mojo/wrap.py — and print exactly what
# CPython prints. Cases named not_*.py assert the profile REFUSES them.
#
# This job is what turns the doc's "believed valid Mojo" into "verified".
set -u
cd "$(dirname "$0")/../.."

# --- toolchain: pixi, then Mojo from Modular's conda channel -----------------
export PATH="$HOME/.pixi/bin:$PATH"
if ! command -v pixi >/dev/null 2>&1; then
  curl -fsSL https://pixi.sh/install.sh | bash || { echo "FAIL: pixi install"; exit 1; }
  export PATH="$HOME/.pixi/bin:$PATH"
fi
ENV="$HOME/.p2w-mojo-env"
if [ ! -f "$ENV/pixi.toml" ]; then
  mkdir -p "$ENV"
  (cd "$ENV" && pixi init -c https://conda.modular.com/max -c conda-forge . \
    && pixi add mojo) || { echo "FAIL: mojo install"; exit 1; }
fi
run_mojo() { (cd "$ENV" && pixi run mojo "$@"); }
echo "== mojo version: $(run_mojo --version 2>&1 | head -1)"

# --- the p2w binary (target dir is shared and NOT ./target) ------------------
cargo build -q --bin p2w || exit 1
TD=$(cargo metadata --format-version 1 --no-deps \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
P2W="$TD/debug/p2w"

fails=0
for case in tests/mojo_bridge/*.py; do
  name=$(basename "$case" .py)
  ready=$("$P2W" check --profile mojo "$case" \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['mojo_profile']['ready'])")

  # not_*.py: the profile must REFUSE these (that's the whole test).
  if [[ "$name" == not_* ]]; then
    if [ "$ready" = "False" ]; then echo "PASS [$name] (profile refuses)"
    else echo "FAIL [$name]: profile should refuse this"; fails=$((fails+1)); fi
    continue
  fi

  if [ "$ready" != "True" ]; then
    echo "FAIL [$name]: profile says not ready:"
    "$P2W" check --profile mojo "$case"
    fails=$((fails+1)); continue
  fi

  want=$(python3 "$case") || { echo "FAIL [$name]: CPython run"; fails=$((fails+1)); continue; }
  python3 tools/mojo/wrap.py tools/mojo/p2w_prelude.mojo "$case" > "/tmp/$name.mojo"
  if ! got=$(run_mojo run "/tmp/$name.mojo" 2>"/tmp/$name.err"); then
    echo "FAIL [$name]: mojo rejected it —"
    sed -n '1,12p' "/tmp/$name.err"
    fails=$((fails+1)); continue
  fi
  if [ "$want" = "$got" ]; then echo "PASS [$name]"
  else
    echo "FAIL [$name]: output differs"
    echo "-- CPython:"; echo "$want"
    echo "-- Mojo:"; echo "$got"
    fails=$((fails+1))
  fi
done

echo "---"
if [ "$fails" -eq 0 ]; then echo "mojo bridge: all cases verified"
else echo "mojo bridge: $fails case(s) FAILED"; fi
exit "$fails"
