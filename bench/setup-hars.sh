#!/usr/bin/env bash
# Reproduce the yon/homeassistant-rs build used in docs/benchmarks.md (HA 2026.1.1, Python 3.13).
# Needs: git, cargo, uv. Output: $WORK/hars (repo + .venv), $WORK/hars-py, $WORK/hars-nopy.
set -euo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd); WORK=${WORK:-$BENCH/.work}; mkdir -p "$WORK"; cd "$WORK"
HARS_COMMIT=cf38e88940dc7097661bec4113311cf35874cb96
HA_CORE_COMMIT=b8f23bb388c5df4a98ddef143d28f25fb4981711  # HA 2026.1.1, the submodule pin

[ -d hars ] || git clone -q https://github.com/yon/homeassistant-rs.git hars
git -C hars checkout -q $HARS_COMMIT
if [ ! -f hars/vendor/ha-core/homeassistant/const.py ]; then
  mkdir -p hars/vendor/ha-core && cd hars/vendor/ha-core && git init -q
  git remote add origin https://github.com/home-assistant/core.git 2>/dev/null || true
  git fetch -q --depth 1 origin $HA_CORE_COMMIT && git checkout -q FETCH_HEAD && cd "$WORK"
fi

cd hars; C=vendor/ha-core/homeassistant
uv venv -q --python 3.13 .venv && touch .venv/.stamp
req() { python3 -c "import json;print(' '.join(json.load(open('$C/components/$1/manifest.json')).get('requirements',[])))"; }
# HA core + frontend + every requirement the demo/wemo test config needs (the official image ships these).
uv pip install -q --python .venv/bin/python -c $C/package_constraints.txt ./vendor/ha-core \
  "home-assistant-frontend==20260107.1" $(req wemo) $(req ffmpeg) $(req camera) $(req tts) numpy \
  maturin aiohttp pytest pytest-asyncio
# Libraries for the probe integrations and their dependency chains (probe.py, run-probe.sh).
uv pip install -q --python .venv/bin/python -c $C/package_constraints.txt $(python3 - $C/components mqtt shelly hue esphome met <<'EOF'
import json, os, sys
root, todo, seen, reqs = sys.argv[1], sys.argv[2:], set(), set()
while todo:
    d = todo.pop()
    if d in seen or not os.path.exists(f"{root}/{d}/manifest.json"): continue
    seen.add(d); m = json.load(open(f"{root}/{d}/manifest.json"))
    reqs.update(m.get("requirements", [])); todo += m.get("dependencies", []) + m.get("after_dependencies", [])
print(" ".join(sorted(reqs)))
EOF
)
export PYO3_PYTHON=$PWD/.venv/bin/python
cargo build -q --release --bin homeassistant --features python && cp target/release/homeassistant "$WORK/hars-py"
cargo build -q --release --bin homeassistant && cp target/release/homeassistant "$WORK/hars-nopy"
.venv/bin/python -c 'import sys;print(sys.base_prefix)' > "$WORK/pyhome"
.venv/bin/python -c 'import site;print(site.getsitepackages()[0])' > "$WORK/sp"
echo "built: $WORK/hars-py $WORK/hars-nopy"
