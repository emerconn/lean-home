#!/usr/bin/env python3
"""Regenerate crates/lean-home/src/manifests.json from the automations recording: HA's
`manifest/list` reply, cut to the integrations lean-home implements (`App::components` in
main.rs). Run after re-recording (docs/compatibility.md, "Upgrading").

usage: compat/manifests.py
"""

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
main_rs = (ROOT / "crates/lean-home/src/main.rs").read_text()
# Every string literal in `fn components`: the always-loaded list and the MQTT extras.
body = main_rs[main_rs.index("fn components"):main_rs.index("/// `get_config`")]
ours = re.findall(r'"([a-z_]+)"', body)

sent, reply = {}, None
for line in (ROOT / "compat/fixtures/automations/ws.jsonl").read_text().splitlines():
    frame = json.loads(line)
    for m in frame["msg"] if isinstance(frame["msg"], list) else [frame["msg"]]:
        if frame["dir"] == "send":
            sent[m.get("id")] = m
        elif sent.get(m.get("id"), {}).get("type") == "manifest/list" and m["type"] == "result":
            reply = m["result"]
manifests = [m for m in reply if m["domain"] in ours]
missing = set(ours) - {m["domain"] for m in manifests}
assert not missing, f"not in the recording's manifest/list: {missing}"
out = ROOT / "crates/lean-home/src/manifests.json"
out.write_text(json.dumps(manifests, ensure_ascii=False, indent=1) + "\n")
print(f"{out.relative_to(ROOT)}: {len(manifests)} manifests")
