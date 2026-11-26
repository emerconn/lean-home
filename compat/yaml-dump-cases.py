#!/usr/bin/env python3
"""Dump values with HA's own YAML dumper (annotatedyaml.dumper.dump, as the config editors use to
write automations.yaml and scripts.yaml) and print one JSON line per case: {"value", "yaml"}.
Runs inside the HA image; crates/lean-home/src/yaml_dump.rs must produce the same text.

usage: docker run --rm -i --entrypoint python3 "$HA_IMAGE" - < compat/yaml-dump-cases.py \
         > compat/fixtures/yaml-dump/cases.jsonl
"""

import json

from annotatedyaml.dumper import dump

WORDS = [
    # YAML 1.1 words and numbers PyYAML's resolver reads as non-strings: must be quoted
    "on", "off", "On", "OFF", "yes", "no", "Y", "n", "true", "False", "null", "Null", "~", "",
    "1", "-1", "+1", "0x1f", "0o17", "017", "1_000", "1.5", "1e3", "1.0e+3", ".5", ".inf", "-.Inf",
    ".nan", "3:00", "190:20:30", "04:00:00", "2026-11-25", "2026-11-25 12:00:00", "2026-11-25T12:00:00Z",
    "=", "<<",
    # indicators and special characters
    "light.kitchen", "a: b", "a:b", "a #b", "a#b", "#x", "- x", "-x", "-", "? x", "?x", ":x", "x:",
    "{{ x }}", "[x]", "x]", "{x", "x,y", "&a", "*a", "!a", "|x", ">x", "'x", "\"x", "%x", "@x", "`x",
    "it's", "say \"hi\"", "a\\b", "x ", " x", " ", "a  b", "---", "--- x", "...", "a\tb", "\tx",
    "a\nb", "a\n", "\na", "a\n\nb", "a \nb", "a\n b", "line1\nline2\nline3\n",
    "Küche", "温度", "🙂 smile", "café", " nbsp", "a\u0085b", "a b", "\u0007bell", "﻿bom",
    # folding at 80 columns
    "word " * 20,
    "{{ states('sensor.demo_device_temperature') | float(0) < 30 and is_state('switch.x', 'on') }}",
    "a" * 100,
    "a" * 90 + " b",
    "x " * 50,
    "it's a long line with 'quotes' that has to be folded somewhere past the eightieth column ok",
    "two  spaces  everywhere  in  a  long  line  so  that  folding  has  to  pick  a  single  space  x",
    "long line ending with a space so it needs quotes and gets folded across lines when it is long ",
    "multi line\nwith a long second line that goes on and on past the eightieth column of the file ok",
]

cases = [{"k": w} for w in WORDS]
cases += [[w] for w in ["on", "a: b", "x", ""]]
cases += [
    {"n": None, "t": True, "f": False, "i": 30, "neg": -5, "big": 10**20, "fl": 1.5, "fl0": 1.0,
     "flsmall": 1e-7, "flbig": 1.5e20, "list": [], "map": {}, "nested": {"a": [1, None, {"b": None}]}},
    {"on": 1, "1": "one", "a b": 2, "a: b": 3, "": 4, "long key " * 12: 5},
    [{"id": "1700000000004", "alias": "Door closed", "triggers": [{"trigger": "state", "entity_id": ["a.b", "c.d"], "to": "off", "for": {"seconds": 5}}],
      "conditions": [], "actions": [{"action": "light.turn_on", "target": {"entity_id": "light.x"}, "data": {"brightness_pct": 30}}], "mode": "single"}],
    {"wave": {"alias": "Wave", "sequence": [{"delay": "00:00:01"}, {"choose": [{"conditions": [{"condition": "state", "entity_id": "a.b", "state": "on"}], "sequence": [{"action": "x.y"}]}], "default": []}]}},
    [[1, 2], [[3]], {"a": [[4]]}],
    [],
    {},
]

for value in cases:
    print(json.dumps({"value": value, "yaml": dump(value)}, ensure_ascii=False))
