#!/usr/bin/env python3
"""Record real HA 2026.11.2 behavior for PR 2:
- triggers: numeric_state, time_pattern, homeassistant (start/shutdown), template
- condition: time

Usage (inside ghcr.io/home-assistant/home-assistant:2026.11.2):
  docker run --rm -i --entrypoint python3 ghcr.io/home-assistant/home-assistant:2026.11.2 - \
    < compat/record-pr2.py > compat/fixtures/step2/pr2-triggers-conditions.json
"""

import asyncio
from datetime import UTC, datetime, timedelta
import json
from typing import Any
from unittest.mock import patch

from homeassistant import bootstrap, core, runner
from homeassistant.const import EVENT_HOMEASSISTANT_STARTED
from homeassistant.helpers import condition, config_validation as cv, trigger as trigger_helper
from homeassistant.util import dt as dt_util


def serialize(val: Any) -> Any:
    if val is None or isinstance(val, (bool, int, float, str)):
        return val
    if isinstance(val, datetime):
        return val.isoformat()
    if isinstance(val, timedelta):
        return val.total_seconds()
    if isinstance(val, core.State):
        return {
            "entity_id": val.entity_id,
            "state": val.state,
            "attributes": serialize(dict(val.attributes)),
        }
    if isinstance(val, dict):
        return {str(k): serialize(v) for k, v in val.items()}
    if isinstance(val, (list, tuple, set)):
        return [serialize(v) for v in val]
    return repr(val)


async def record_triggers(hass: core.HomeAssistant) -> dict[str, Any]:
    events: list[dict[str, Any]] = []

    async def make_action(name: str):
        async def _action(run_variables: dict[str, Any], context=None):
            events.append({"name": name, "trigger": serialize(run_variables.get("trigger"))})

        return _action

    async def attach(name: str, cfg: dict[str, Any]):
        validated = await trigger_helper.async_validate_trigger_config(hass, [cfg])
        return await trigger_helper.async_initialize_triggers(
            hass,
            validated,
            await make_action(name),
            "automation",
            name,
            lambda *a, **kw: None,
        )

    # 1. numeric_state trigger
    # Set initial state below threshold (15), then cross above 20, stay above 20 (25 -> no fire),
    # drop to 18 (disarms), cross above 20 again (fires), set unavailable (disarms),
    # and also test attribute + value_template + above&below range.
    hass.states.async_set("sensor.temp", "15")
    hass.states.async_set("sensor.hum", "50", {"level": 10})
    unsub_ns1 = await attach(
        "ns_above_20",
        {"platform": "numeric_state", "entity_id": "sensor.temp", "above": 20},
    )
    unsub_ns2 = await attach(
        "ns_range_10_30_attr",
        {
            "platform": "numeric_state",
            "entity_id": "sensor.hum",
            "attribute": "level",
            "above": 15,
            "below": 25,
        },
    )
    unsub_ns3 = await attach(
        "ns_value_template",
        {
            "platform": "numeric_state",
            "entity_id": "sensor.temp",
            "value_template": "{{ float(state.state) * 2 }}",
            "above": 50,
        },
    )
    await hass.async_block_till_done()

    ns_steps = [
        ("sensor.temp", "19", {}),  # still <= 20: no fire
        ("sensor.temp", "20", {}),  # equal to above=20: no fire (strict >)
        ("sensor.temp", "21", {}),  # crosses > 20: fires ns_above_20
        ("sensor.temp", "26", {}),  # already above 20: no fire for ns_above_20; 26*2=52 > 50: fires ns_value_template
        ("sensor.temp", "unavailable", {}),  # disarms both
        ("sensor.temp", "22", {}),  # re-arms and fires ns_above_20 (44 <= 50 so no ns_value_template)
        ("sensor.hum", "50", {"level": 20}),  # 15 < 20 < 25: fires ns_range_10_30_attr
        ("sensor.hum", "50", {"level": 25}),  # equal to below=25: disarms, no fire
        ("sensor.hum", "50", {"level": 22}),  # re-enters (15, 25): fires ns_range_10_30_attr
    ]
    for eid, st, attrs in ns_steps:
        hass.states.async_set(eid, st, attrs)
        await hass.async_block_till_done()

    unsub_ns1()
    unsub_ns2()
    unsub_ns3()

    # 2. template trigger
    hass.states.async_set("input_boolean.a", "off")
    hass.states.async_set("input_boolean.b", "off")
    unsub_tmpl = await attach(
        "tmpl_both_on",
        {
            "platform": "template",
            "value_template": "{{ is_state('input_boolean.a', 'on') and is_state('input_boolean.b', 'on') }}",
        },
    )
    await hass.async_block_till_done()
    for eid, st in [
        ("input_boolean.a", "on"),  # false -> no fire
        ("input_boolean.b", "on"),  # false -> true: fires
        ("input_boolean.a", "on"),  # unchanged
        ("input_boolean.b", "off"), # true -> false: no fire
        ("input_boolean.b", "on"),  # false -> true: fires
    ]:
        hass.states.async_set(eid, st, force_update=True)
        await hass.async_block_till_done()
    unsub_tmpl()

    # 3. homeassistant start & shutdown triggers
    unsub_start = await attach("ha_start", {"platform": "homeassistant", "event": "start"})
    unsub_stop = await attach("ha_shutdown", {"platform": "homeassistant", "event": "shutdown"})
    hass.bus.async_fire(EVENT_HOMEASSISTANT_STARTED)
    await hass.async_block_till_done()
    unsub_start()
    # Keep unsub_stop attached so it fires on hass.async_stop()!

    # 4. time_pattern trigger: inspect exact matching logic via async_track_time_change
    from homeassistant.helpers.event import async_track_time_change

    tp_cases = [
        {"minutes": "/15"},           # seconds defaults to 0
        {"hours": "/2", "minutes": 30}, # seconds defaults to 0
        {"hours": "*", "minutes": "/30", "seconds": 0},
        {"seconds": "/20"},
    ]
    tp_matches: dict[str, list[str]] = {}
    sample_times = [
        datetime(2026, 11, 24, 0, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 0, 0, 20, tzinfo=UTC),
        datetime(2026, 11, 24, 0, 15, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 1, 30, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 2, 30, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 2, 30, 1, tzinfo=UTC),
    ]
    for idx, cfg in enumerate(tp_cases):
        key = json.dumps(cfg, sort_keys=True)
        matched = []
        fired = False

        def _cb(now, _k=key):
            nonlocal fired
            fired = True

        # Validate through time_pattern schema & attach
        from homeassistant.components.homeassistant.triggers import time_pattern as tp_mod
        v = tp_mod.TRIGGER_SCHEMA({"platform": "time_pattern", **cfg})
        h = v.get("hours")
        m = v.get("minutes")
        s = v.get("seconds")
        if m is None and h is not None:
            m = 0
        if s is None and m is not None:
            s = 0
        # Use HA's actual time_pattern matching via dt_util.find_next_time_expression_time
        from homeassistant.util.dt import parse_time_expression, find_next_time_expression_time
        hours_expr = parse_time_expression(h, 0, 23)
        mins_expr = parse_time_expression(m, 0, 59)
        secs_expr = parse_time_expression(s, 0, 59)
        for t in sample_times:
            nxt = find_next_time_expression_time(t, secs_expr, mins_expr, hours_expr)
            if nxt == t:
                matched.append(t.isoformat())
        tp_matches[key] = matched

    return {"events": events, "time_pattern_matches": tp_matches, "unsub_stop": unsub_stop}


async def record_time_conditions(hass: core.HomeAssistant) -> list[dict[str, Any]]:
    # 2026-11-24 is a Tuesday ("tue")
    hass.states.async_set("input_datetime.start", "08:00:00")
    hass.states.async_set("input_datetime.end", "17:00:00")
    await hass.async_block_till_done()

    cases = [
        {"condition": "time", "after": "08:00:00", "before": "17:00:00"},
        {"condition": "time", "after": "22:00:00", "before": "06:00:00"},  # overnight wrap
        {"condition": "time", "after": "08:00:00", "weekday": ["mon", "tue"]},
        {"condition": "time", "after": "08:00:00", "weekday": ["wed", "thu"]},
        {"condition": "time", "after": "input_datetime.start", "before": "input_datetime.end"},
    ]
    test_times = [
        datetime(2026, 11, 24, 5, 59, 59, tzinfo=UTC),
        datetime(2026, 11, 24, 6, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 8, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 17, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 22, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 23, 0, 0, tzinfo=UTC),
    ]

    results = []
    for cfg in cases:
        validated = cv.CONDITION_SCHEMA(cfg)
        cond_fn = await condition.async_from_config(hass, validated)
        evals = {}
        for t in test_times:
            with patch.object(dt_util, "now", return_value=t):
                evals[t.isoformat()] = bool(cond_fn(hass))
        results.append({"config": cfg, "results": evals})
    return results


async def main():
    import logging
    import os
    logging.disable(logging.WARNING)
    os.makedirs("/tmp/ha-pr2", exist_ok=True)
    with open("/tmp/ha-pr2/configuration.yaml", "w") as f:
        f.write("homeassistant:\n  time_zone: UTC\nautomation:\nscript:\n")
    hass = await bootstrap.async_setup_hass(runner.RuntimeConfig(config_dir="/tmp/ha-pr2"))
    await hass.async_start()
    trig_res = await record_triggers(hass)
    cond_res = await record_time_conditions(hass)
    await hass.async_stop()
    trig_res["unsub_stop"]()
    out = {
        "ha_version": "2026.11.2",
        "trigger_events": trig_res["events"],
        "time_pattern_matches": trig_res["time_pattern_matches"],
        "time_conditions": cond_res,
    }
    print(json.dumps(out, indent=2, ensure_ascii=False))


asyncio.run(main())
