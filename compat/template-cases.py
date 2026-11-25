#!/usr/bin/env python3
"""Template cases from HA's own tests, rendered by HA's own engine.

usage (inside the HA image, see compat/README.md):
  template-cases.py <ha-core-checkout>/tests/helpers/template > compat/fixtures/templates/cases.jsonl

Reads HA's template test files with `ast` (never runs them). For each test function, in source
order, it tracks the literal `hass.states.async_set(...)` calls and collects every literal
template passed to `render`, `render_to_info`, `extract_entities` or `template.Template`,
expanding `@pytest.mark.parametrize` tables when the template comes from one. Each case is
then rendered here, by HA, with exactly those states and a frozen clock, and written with what
HA produced: the text, the parsed result, the error, the listeners, and the names it uses.
"""

import ast
import asyncio
import json
import sys
from datetime import UTC, datetime
from pathlib import Path

import jinja2
from jinja2 import nodes

FROZEN = datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC)
RENDER_FUNCS = {"render", "render_to_info", "extract_entities"}


def literal(node):
    try:
        return True, ast.literal_eval(node)
    except Exception:
        return False, None


def param_tables(func):
    """`@pytest.mark.parametrize("a,b", [(...), ...])` as a list of {name: value} dicts."""
    rows = [{}]
    for dec in func.decorator_list:
        if not (isinstance(dec, ast.Call) and getattr(dec.func, "attr", "") == "parametrize"):
            continue
        ok_n, names = literal(dec.args[0]) if dec.args else (False, None)
        ok_v, values = literal(dec.args[1]) if len(dec.args) > 1 else (False, None)
        if not (ok_n and ok_v):
            continue
        names = [n.strip() for n in names.split(",")] if isinstance(names, str) else list(names)
        table = []
        for v in values:
            v = v if len(names) > 1 else (v,)
            if isinstance(v, (tuple, list)) and len(v) == len(names):
                table.append(dict(zip(names, v)))
        rows = [{**r, **t} for r in rows for t in table] or rows
    return rows


def calls_in_order(func):
    calls = [n for n in ast.walk(func) if isinstance(n, ast.Call)]
    return sorted(calls, key=lambda c: (c.lineno, c.col_offset))


def extract(path):
    tree = ast.parse(path.read_text())
    for func in ast.walk(tree):
        if not isinstance(func, (ast.FunctionDef, ast.AsyncFunctionDef)) or not func.name.startswith("test_"):
            continue
        for params in param_tables(func):
            states = {}

            def value(node):
                if isinstance(node, ast.Name) and node.id in params:
                    return True, params[node.id]
                return literal(node)

            for call in calls_in_order(func):
                f = call.func
                name = f.attr if isinstance(f, ast.Attribute) else getattr(f, "id", "")
                owner = ast.unparse(f.value) if isinstance(f, ast.Attribute) else ""
                if name == "async_set" and owner.endswith("states") and len(call.args) >= 2:
                    ok_e, eid = value(call.args[0])
                    ok_s, st = value(call.args[1])
                    ok_a, attrs = value(call.args[2]) if len(call.args) > 2 else (True, {})
                    if ok_e and ok_s and ok_a and isinstance(eid, str):
                        states[eid] = (str(st), attrs or {})
                    continue
                if name == "async_remove" and owner.endswith("states") and call.args:
                    ok, eid = value(call.args[0])
                    if ok:
                        states.pop(eid, None)
                    continue
                if name in RENDER_FUNCS and len(call.args) >= 2:
                    tpl_node, var_node = call.args[1], call.args[2] if len(call.args) > 2 else None
                elif name == "Template" and call.args:
                    tpl_node, var_node = call.args[0], None
                else:
                    continue
                ok_t, tpl = value(tpl_node)
                if not ok_t or not isinstance(tpl, str):
                    continue
                variables = None
                kw = {k.arg: k.value for k in call.keywords}
                if var_node is None and "variables" in kw:
                    var_node = kw["variables"]
                if var_node is not None:
                    ok_v, variables = value(var_node)
                    if not ok_v or not isinstance(variables, (dict, type(None))):
                        continue
                yield {
                    "test": f"{path.name}::{func.name}",
                    "template": tpl,
                    "variables": variables,
                    "states": [{"entity_id": e, "state": s, "attributes": a} for e, (s, a) in sorted(states.items())],
                }


def names_used(src):
    """Globals, filters, tests and method calls a template uses (Jinja's AST)."""
    env = jinja2.Environment(extensions=["jinja2.ext.loopcontrols", "jinja2.ext.do"])
    try:
        ast_ = env.parse(src)
    except jinja2.TemplateSyntaxError:
        return []
    out = set()
    for n in ast_.find_all((nodes.Name, nodes.Filter, nodes.Test, nodes.Call)):
        if isinstance(n, nodes.Name) and n.ctx == "load":
            out.add(n.name)
        elif isinstance(n, nodes.Filter):
            out.add("filter:" + n.name)
            # map('apply', ...), select('is_state', ...): a filter or test named by a string
            if n.name in ("map", "select", "reject", "selectattr", "rejectattr") and n.args:
                arg = n.args[-1] if n.name.endswith("attr") and len(n.args) > 1 else n.args[0]
                if isinstance(arg, nodes.Const) and isinstance(arg.value, str):
                    out.add(("filter:" if n.name == "map" else "test:") + arg.value)
        elif isinstance(n, nodes.Test):
            out.add("test:" + n.name)
        elif isinstance(n, nodes.Call) and isinstance(n.node, nodes.Getattr):
            out.add("method:" + n.node.attr)
    return sorted(out)


def jsonable(v):
    """The parsed result as JSON; other Python types as {"py": repr}."""
    if v is None or isinstance(v, (bool, int, float, str)):
        if isinstance(v, float) and (v != v or v in (float("inf"), float("-inf"))):
            return {"py": repr(v)}
        return v
    if isinstance(v, (list, tuple)):
        return [jsonable(x) for x in v]
    if isinstance(v, dict) and all(isinstance(k, str) for k in v):
        return {k: jsonable(x) for k, x in v.items()}
    return {"py": repr(v)}


async def main(test_dir):
    from homeassistant.core import HomeAssistant
    from homeassistant.helpers import template
    from homeassistant.util import dt as dt_util

    # A frozen clock, as HA's tests use (freezer); the template extension reads dt_util.
    dt_util.utcnow = lambda: FROZEN
    dt_util.now = lambda time_zone=None: FROZEN.astimezone(time_zone or dt_util.DEFAULT_TIME_ZONE)

    hass = HomeAssistant("/tmp/lh-template-cases")
    # Registries and helper modules, empty, the way bootstrap.py sets them up
    # (`_async_load_registries`); state_with_unit and others read them.
    from homeassistant.helpers import (
        area_registry, category_registry, device_registry, entity, entity_registry,
        floor_registry, frame, issue_registry, label_registry, translation,
    )
    for mod in (entity, frame, template, translation):
        mod.async_setup(hass)
    from homeassistant import config_entries
    hass.config_entries = config_entries.ConfigEntries(hass, {})
    device_registry.async_setup(hass)
    await asyncio.gather(hass.config_entries.async_initialize(), *(
        reg.async_load(hass, load_empty=True)
        for reg in (area_registry, category_registry, device_registry, entity_registry,
                    floor_registry, issue_registry, label_registry)
    ))
    seen = set()
    cases = []
    for path in sorted(Path(test_dir).rglob("test_*.py")):
        for case in extract(path):
            key = json.dumps([case["template"], case["variables"], case["states"]], sort_keys=True, default=repr)
            if key in seen:
                continue
            seen.add(key)
            cases.append(case)

    for case in cases:
        for s in list(hass.states.async_all()):
            hass.states.async_remove(s.entity_id)
        try:
            for s in case["states"]:
                hass.states.async_set(s["entity_id"], s["state"], s["attributes"])
        except Exception as e:  # noqa: BLE001
            case["skip"] = f"states: {e}"
            continue
        variables = case["variables"]
        try:
            text = template.Template(case["template"], hass).async_render(variables, parse_result=False)
            case["text"] = text
        except Exception as e:  # noqa: BLE001
            case["text_error"] = str(e)
        info = template.Template(case["template"], hass).async_render_to_info(variables)
        if info.exception is not None:
            case["error"] = str(info.exception)
        else:
            case["result"] = jsonable(info.result())
        case["listeners"] = {
            "all": info.all_states,
            "domains": sorted(info.domains),
            "entities": sorted(info.entities),
            "time": info.has_time,
            "all_lifecycle": info.all_states_lifecycle,
            "domains_lifecycle": sorted(info.domains_lifecycle),
        }
        case["uses"] = names_used(case["template"])
        # Variables and attributes that aren't JSON (datetimes, sets...) can't be replayed.
        case["variables"] = jsonable(variables)
        for s in case["states"]:
            s["attributes"] = jsonable(s["attributes"])

    # HA-specific names: in HA's environment but not in plain Jinja's.
    plain = jinja2.Environment(extensions=["jinja2.ext.loopcontrols", "jinja2.ext.do"])
    ha_env = template.Template("{{ 1 }}", hass)._env
    ha_names = (
        {n for n in ha_env.globals if n not in plain.globals}
        | {"filter:" + n for n in ha_env.filters if n not in plain.filters}
        | {"test:" + n for n in ha_env.tests if n not in plain.tests}
    )
    for case in cases:
        if "uses" in case:
            case["ha_names"] = [n for n in case["uses"] if n in ha_names]
        print(json.dumps(case, ensure_ascii=False, sort_keys=True))
    await hass.async_stop(force=True)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1]))
