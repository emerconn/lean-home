#!/usr/bin/env python3
"""Compare a lean-home replay (compat/replay.sh) with the HA recording it replays.

usage: compare.py <scenario> <recorded-dir> <replay-dir>
Exits 1 with a readable list of differences. What HA has and lean-home deliberately doesn't (other
integrations' entities, unimplemented components) is ignored; every other tolerance is commented.
"""

import json
import re
import sys
from pathlib import Path

# Commands lean-home doesn't implement yet; the frontend copes with these errors. Anything else
# lean-home answers with an error is a failure.
ALLOWED_ERRORS = {
    ("brands/access_token", "unknown_command"),
    ("http/config", "unknown_command"),
    ("recorder/info", "unknown_command"),
    ("lovelace/config", "config_not_found"),  # HA answers the same: no dashboard saved yet
}


def expected_entities(scenario, recv):
    """The entities lean-home must provide, taken from the recording, so a missing one fails."""
    if scenario == "frontend-load":
        # The entity the recording toggles, which --demo provides. The demo's other entities aren't
        # HA's: switch.fan isn't in the recording, and HA's template sensor.temperature has no
        # state_class while the demo's has.
        return {"light.kitchen"}
    # Every MQTT entity HA created from the recorded discovery messages.
    return {e["ei"] for e in (first_result(recv, "config/entity_registry/list_for_display") or {}).get("entities", []) if e["pl"] == "mqtt"}


problems = []
compared = {}  # what was checked, for the summary line


def check(ok, what, recorded=None, replayed=None):
    if not ok:
        msg = what
        if recorded is not None or replayed is not None:
            msg += f"\n    HA:        {json.dumps(recorded, ensure_ascii=False)}\n    lean-home: {json.dumps(replayed, ensure_ascii=False)}"
        problems.append(msg)


def ws_messages(path):
    """(command type, message) for every received message, per message rather than per frame
    (HA coalesces messages into arrays depending on timing), plus the sent commands."""
    sent, recv = {}, []
    for line in path.read_text().splitlines():
        frame = json.loads(line)
        for m in frame["msg"] if isinstance(frame["msg"], list) else [frame["msg"]]:
            if frame["dir"] == "send":
                sent[m.get("id")] = m
            else:
                recv.append((sent.get(m.get("id"), {}).get("type"), m))
    return sent, recv


def first_result(recv, command):
    return next((m.get("result") for t, m in recv if t == command and m["type"] == "result"), None)


def entity_changes(recv, entities):
    """subscribe_entities `c` events for `entities`, one (entity_id, diff) per change."""
    out = []
    for t, m in recv:
        if t == "subscribe_entities" and m["type"] == "event":
            for eid, diff in m["event"].get("c", {}).items():
                if eid in entities:
                    if "-" in diff:
                        # HA builds the removed-attribute list from a set: its order is arbitrary.
                        diff = {**diff, "-": {k: sorted(v) for k, v in diff["-"].items()}}
                    out.append((eid, diff))
    # HA writes a batch of states caused by one event (e.g. every entity of a device going
    # unavailable) in subscription-callback order, which is arbitrary: compare consecutive changes
    # to the same new state as a set.
    groups, key = [], object()
    for eid, diff in out:
        k = diff.get("+", {}).get("s")
        if k is None or k != key:
            groups.append([])
        groups[-1].append((eid, diff))
        key = k
    return [sorted(g, key=lambda x: x[0]) for g in groups]


def compare_ws(scenario, rec_dir, rep_dir):
    rec_sent, rec = ws_messages(rec_dir / "ws.jsonl")
    rep_sent, rep = ws_messages(rep_dir / "ws.jsonl")
    snapshot = lambda recv: next(m["event"]["a"] for t, m in recv if t == "subscribe_entities" and m["type"] == "event" and "a" in m["event"])
    ha_snap, lh_snap = snapshot(rec), snapshot(rep)
    entities = expected_entities(scenario, rec)
    check(entities, "the recording has no entities to compare")
    missing = sorted(entities - set(lh_snap))
    check(not missing, f"the recording has entities lean-home didn't create: {', '.join(missing)}")
    for eid in sorted(entities & set(lh_snap)):
        check(ha_snap.get(eid) == lh_snap[eid], f"initial state of {eid} differs", ha_snap.get(eid), lh_snap[eid])

    ha_changes, lh_changes = entity_changes(rec, entities), entity_changes(rep, entities)
    compared["entities"] = len(entities)
    compared["state changes"] = sum(map(len, ha_changes))
    for i in range(max(len(ha_changes), len(lh_changes))):
        a = ha_changes[i] if i < len(ha_changes) else None
        b = lh_changes[i] if i < len(lh_changes) else None
        check(a == b, f"state change #{i + 1} differs", a, b)
        if a != b:
            break  # later changes would all be reported shifted

    # Registry data the frontend shows, for the integration lean-home implements.
    display = lambda recv: [e for e in (first_result(recv, "config/entity_registry/list_for_display") or {}).get("entities", []) if e["pl"] == "mqtt"]
    check(display(rec) == display(rep), "entity_registry/list_for_display (mqtt) differs", display(rec), display(rep))
    devices = lambda recv: [d for d in first_result(recv, "config/device_registry/list") or [] if any(i[0] == "mqtt" for i in d["identifiers"])]
    # json.dumps keeps key order, which HA's frontend doesn't need but we match anyway.
    check(json.dumps(devices(rec)) == json.dumps(devices(rep)), "device_registry/list (mqtt) differs", devices(rec), devices(rep))

    # Service calls the scenario made: each must succeed where HA's did.
    calls = lambda sent, recv: [
        (sent[m["id"]]["domain"], sent[m["id"]]["service"], m.get("success"))
        for t, m in recv if t == "call_service" and m["type"] == "result" and sent[m["id"]]["domain"] != "system_log"
    ]
    check(calls(rec_sent, rec) == calls(rep_sent, rep), "call_service results differ", calls(rec_sent, rec), calls(rep_sent, rep))
    # The frontend reports its own errors through system_log.write.
    logged = [m["service_data"].get("message", "")[:300] for m in rep_sent.values() if m.get("type") == "call_service" and m.get("domain") == "system_log"]
    check(not logged, "the frontend reported errors (system_log.write)", None, logged)

    errors = {(t, m["error"]["code"]) for t, m in rep if m["type"] == "result" and m.get("success") is False}
    check(errors <= ALLOWED_ERRORS, "lean-home returned unexpected errors", None, sorted(errors - ALLOWED_ERRORS))


def compare_http(rec_dir, rep_dir):
    # capture.mjs logs responses as they complete; requests in flight together (the page and the
    # frontend's /.well-known fetch) finish in either order, so compare in path order.
    load = lambda d: sorted((json.loads(l) for l in (d / "http.jsonl").read_text().splitlines()), key=lambda r: (r["path"], r["method"]))
    rec, rep = load(rec_dir), load(rep_dir)
    compared["HTTP requests"] = len(rec)
    for a, b in zip(rec, rep):
        if a["res"] is None or b["res"] is None:
            # The page can navigate away before the recorder reads a body (res: null); it happens
            # for /.well-known and for the login step, whose success redirects at once. Method,
            # path, status and request are still compared, and what the body decides is checked
            # downstream: the code_challenge sent to /auth/login_flow (S256 from /.well-known) and
            # the code exchanged at /auth/token (the login result) are compared exactly.
            a, b = {**a, "res": None}, {**b, "res": None}
        elif a["path"] == "/.well-known/oauth-authorization-server":
            # lean-home serves relative URLs without `issuer` (HA's own fallback when it can't
            # tell its URL) and says it doesn't fetch client_id metadata documents (it doesn't).
            a = json.loads(re.sub(r"https?://[^/\"]+(/auth/)", r"\1", json.dumps(a)))
            a["res"].pop("issuer", None)
            a["res"]["client_id_metadata_document_supported"] = False
        check(a == b, f"HTTP {a['method']} {a['path']} differs", a, b)
    check(len(rec) == len(rep), "different number of HTTP requests", [r["path"] for r in rec], [r["path"] for r in rep])


def compare_mqtt(rec_dir, rep_dir):
    # What the hub published: commands (`…/set`) and its birth/will on homeassistant/status. The
    # test devices' own messages (discovery, states) are the same input on both sides.
    hub = lambda d: [
        m for m in map(json.loads, (d / "mqtt.jsonl").read_text().splitlines())
        if m["topic"].endswith("/set") or m["topic"] == "homeassistant/status"
    ]
    compared["MQTT messages"] = len(hub(rec_dir))
    check(hub(rec_dir) == hub(rep_dir), "MQTT messages published by the hub differ", hub(rec_dir), hub(rep_dir))


def main():
    scenario, rec_dir, rep_dir = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    compare_http(rec_dir, rep_dir)
    compare_ws(scenario, rec_dir, rep_dir)
    if scenario == "frontend-load":
        check((rec_dir / "index.html").read_text() == (rep_dir / "index.html").read_text(), "index.html differs")
    if scenario == "mqtt-discovery":
        compare_mqtt(rec_dir, rep_dir)
    if problems:
        print(f"{scenario}: {len(problems)} difference(s) from the HA recording:")
        for p in problems:
            print(f"  - {p}")
        sys.exit(1)
    print(f"{scenario}: matches the HA recording ({', '.join(f'{v} {k}' for k, v in compared.items())})")


if __name__ == "__main__":
    main()
