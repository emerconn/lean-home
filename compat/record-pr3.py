#!/usr/bin/env python3
"""Record real HA 2026.11.2 behavior for PR 3:
- actions: wait_for_trigger, wait_template, repeat (count, for_each, while, until), sequence, parallel
- trigger & action: event

Usage (inside ghcr.io/home-assistant/home-assistant:2026.11.2):
  docker run --rm -i --entrypoint python3 ghcr.io/home-assistant/home-assistant:2026.11.2 - \
    < compat/record-pr3.py > compat/fixtures/step2/pr3-actions-events.json
"""

import asyncio
from datetime import datetime, timedelta
import json
import logging
import os
from typing import Any

from homeassistant import bootstrap, core, runner
from homeassistant.components.trace import DATA_TRACE
from homeassistant.components.script.trace import trace_script
from homeassistant.helpers import config_validation as cv, script, trigger as trigger_helper


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
    if isinstance(val, core.Event):
        return {
            "event_type": val.event_type,
            "data": serialize(dict(val.data)),
            "origin": str(val.origin),
        }
    if isinstance(val, dict):
        return {str(k): serialize(v) for k, v in val.items()}
    if isinstance(val, (list, tuple, set)):
        return [serialize(v) for v in val]
    return repr(val)


async def run_script_and_trace(
    hass: core.HomeAssistant, name: str, raw_seq: list[dict[str, Any]], variables: dict[str, Any] | None = None
) -> dict[str, Any]:
    validated = cv.SCRIPT_SCHEMA(raw_seq)
    s = script.Script(hass, validated, name, "script", running_description="script sequence")
    ctx = core.Context()
    with trace_script(hass, f"script.{name}", {"sequence": raw_seq}, None, ctx, {"stored_traces": 5}) as script_trace:
        run_res = await s.async_run(run_variables=variables or {}, context=ctx)
        last_trace = serialize(script_trace.as_dict())
    return {
        "name": name,
        "result": serialize(run_res),
        "trace": last_trace,
    }


async def main():
    logging.disable(logging.WARNING)
    os.makedirs("/tmp/ha-pr3", exist_ok=True)
    with open("/tmp/ha-pr3/configuration.yaml", "w") as f:
        f.write("homeassistant:\n  time_zone: UTC\nautomation:\nscript:\n")
    hass = await bootstrap.async_setup_hass(runner.RuntimeConfig(config_dir="/tmp/ha-pr3"))
    await hass.async_start()

    recorded_events: list[dict[str, Any]] = []
    event_triggers: list[dict[str, Any]] = []

    # 1. Event trigger + event action
    async def on_event_trigger(run_variables: dict[str, Any], context=None):
        event_triggers.append(serialize(run_variables.get("trigger")))

    ev_cfg = await trigger_helper.async_validate_trigger_config(
        hass,
        [{"platform": "event", "event_type": "custom_test_event", "event_data": {"kind": "ping"}}],
    )
    unsub_ev = await trigger_helper.async_initialize_triggers(
        hass, ev_cfg, on_event_trigger, "automation", "ev_test", lambda *a, **kw: None
    )

    hass.bus.async_listen("custom_test_event", lambda ev: recorded_events.append(serialize(ev)))
    hass.bus.async_listen("log_step", lambda ev: recorded_events.append(serialize(ev)))

    # 2. Run scripts covering: event action, repeat, sequence, parallel, wait_template, wait_for_trigger
    script_results = []

    # Script A: event action (matching and non-matching event_data)
    script_results.append(
        await run_script_and_trace(
            hass,
            "event_action",
            [
                {"event": "custom_test_event", "event_data": {"kind": "other", "n": 1}},
                {"event": "custom_test_event", "event_data": {"kind": "ping", "n": 2}},
            ],
        )
    )
    await hass.async_block_till_done()

    # Script B: repeat (count, for_each, while, until)
    hass.states.async_set("sensor.counter", "0")
    script_results.append(
        await run_script_and_trace(
            hass,
            "repeat_variants",
            [
                {
                    "repeat": {
                        "count": 3,
                        "sequence": [
                            {
                                "event": "log_step",
                                "event_data": {
                                    "kind": "count",
                                    "index": "{{ repeat.index }}",
                                    "first": "{{ repeat.first }}",
                                    "last": "{{ repeat.last }}",
                                },
                            }
                        ],
                    }
                },
                {
                    "repeat": {
                        "for_each": ["alpha", "beta"],
                        "sequence": [
                            {
                                "event": "log_step",
                                "event_data": {
                                    "kind": "for_each",
                                    "item": "{{ repeat.item }}",
                                    "index": "{{ repeat.index }}",
                                    "first": "{{ repeat.first }}",
                                    "last": "{{ repeat.last }}",
                                },
                            }
                        ],
                    }
                },
                {
                    "repeat": {
                        "while": [{"condition": "template", "value_template": "{{ repeat.index <= 2 }}"}],
                        "sequence": [
                            {
                                "event": "log_step",
                                "event_data": {"kind": "while", "index": "{{ repeat.index }}"},
                            }
                        ],
                    }
                },
                {
                    "repeat": {
                        "until": [{"condition": "template", "value_template": "{{ repeat.index >= 2 }}"}],
                        "sequence": [
                            {
                                "event": "log_step",
                                "event_data": {"kind": "until", "index": "{{ repeat.index }}"},
                            }
                        ],
                    }
                },
            ],
        )
    )

    # Script C: sequence and parallel
    script_results.append(
        await run_script_and_trace(
            hass,
            "sequence_and_parallel",
            [
                {
                    "alias": "Explicit sequence",
                    "sequence": [
                        {"event": "log_step", "event_data": {"step": "seq_1"}},
                        {"event": "log_step", "event_data": {"step": "seq_2"}},
                    ],
                },
                {
                    "alias": "Parallel branches",
                    "parallel": [
                        {"event": "log_step", "event_data": {"step": "par_1"}},
                        {
                            "sequence": [
                                {"event": "log_step", "event_data": {"step": "par_2a"}},
                                {"event": "log_step", "event_data": {"step": "par_2b"}},
                            ]
                        },
                    ],
                },
            ],
        )
    )

    # Script D: wait_template (immediate true, then state-change true, then timeout continue vs abort)
    hass.states.async_set("binary_sensor.door", "off")
    script_results.append(
        await run_script_and_trace(
            hass,
            "wait_template_immediate",
            [
                {"wait_template": "{{ is_state('binary_sensor.door', 'off') }}"},
                {
                    "event": "log_step",
                    "event_data": {
                        "completed": "{{ wait.completed }}",
                        "remaining": "{{ wait.remaining }}",
                    },
                },
            ],
        )
    )

    script_results.append(
        await run_script_and_trace(
            hass,
            "wait_template_timeout_continue",
            [
                {
                    "wait_template": "{{ is_state('binary_sensor.door', 'on') }}",
                    "timeout": {"milliseconds": 50},
                    "continue_on_timeout": True,
                },
                {
                    "event": "log_step",
                    "event_data": {
                        "after_timeout": True,
                        "completed": "{{ wait.completed }}",
                        "remaining": "{{ wait.remaining }}",
                    },
                },
            ],
        )
    )

    script_results.append(
        await run_script_and_trace(
            hass,
            "wait_template_timeout_abort",
            [
                {
                    "wait_template": "{{ is_state('binary_sensor.door', 'on') }}",
                    "timeout": {"milliseconds": 50},
                    "continue_on_timeout": False,
                },
                {"event": "log_step", "event_data": {"should_not_run": True}},
            ],
        )
    )

    # Script E: wait_for_trigger (matched via async state change, and timeout)
    async def trigger_door_soon():
        await asyncio.sleep(0.05)
        hass.states.async_set("binary_sensor.door", "on")

    task = asyncio.create_task(trigger_door_soon())
    script_results.append(
        await run_script_and_trace(
            hass,
            "wait_for_trigger_matched",
            [
                {
                    "wait_for_trigger": [
                        {"trigger": "state", "entity_id": "binary_sensor.door", "to": "on"}
                    ],
                    "timeout": {"seconds": 2},
                },
                {
                    "event": "log_step",
                    "event_data": {
                        "completed": "{{ wait.completed }}",
                        "trigger_platform": "{{ wait.trigger.platform }}",
                        "trigger_to": "{{ wait.trigger.to_state.state }}",
                    },
                },
            ],
        )
    )
    await task

    script_results.append(
        await run_script_and_trace(
            hass,
            "wait_for_trigger_timeout",
            [
                {
                    "wait_for_trigger": [
                        {"trigger": "state", "entity_id": "binary_sensor.door", "to": "off"}
                    ],
                    "timeout": {"milliseconds": 50},
                    "continue_on_timeout": True,
                },
                {
                    "event": "log_step",
                    "event_data": {
                        "completed": "{{ wait.completed }}",
                        "trigger_is_none": "{{ wait.trigger is none }}",
                    },
                },
            ],
        )
    )

    await hass.async_block_till_done()
    unsub_ev()
    await hass.async_stop()

    out = {
        "ha_version": "2026.11.2",
        "event_triggers": event_triggers,
        "recorded_events": recorded_events,
        "scripts": script_results,
    }
    print(json.dumps(out, indent=2, ensure_ascii=False))


asyncio.run(main())
