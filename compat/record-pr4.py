#!/usr/bin/env python3
"""Record real HA 2026.11.2 behavior for PR 4:
- use_blueprint automations: homeassistant/motion_light.yaml and homeassistant/notify_leaving_zone.yaml

Usage (inside ghcr.io/home-assistant/home-assistant:2026.11.2):
  docker run --rm -i --entrypoint python3 ghcr.io/home-assistant/home-assistant:2026.11.2 - \
    < compat/record-pr4.py > compat/fixtures/step2/pr4-blueprints.json
"""

import asyncio
from datetime import datetime, timedelta
import json
import logging
import os
from pathlib import Path
import shutil
from typing import Any

from homeassistant import bootstrap, core, runner
from homeassistant.components.automation import DOMAIN as AUTO_DOMAIN
from homeassistant.components.automation.config import async_validate_config_item
from homeassistant.components.trace import DATA_TRACE
from homeassistant.helpers import device_registry as dr


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


async def main():
    logging.disable(logging.WARNING)
    cfg_dir = "/tmp/ha-pr4"
    os.makedirs(cfg_dir, exist_ok=True)
    bp_dst = Path(cfg_dir, "blueprints", "automation", "homeassistant")
    bp_dst.parent.mkdir(parents=True, exist_ok=True)
    if not bp_dst.exists():
        shutil.copytree("/usr/src/homeassistant/homeassistant/components/automation/blueprints", bp_dst)

    with open(f"{cfg_dir}/configuration.yaml", "w") as f:
        f.write("homeassistant:\n  time_zone: UTC\nautomation: !include automations.yaml\n")
    with open(f"{cfg_dir}/automations.yaml", "w") as f:
        f.write(
            """- id: "bp_motion_light_default"
  alias: "Motion Light (default wait)"
  use_blueprint:
    path: homeassistant/motion_light.yaml
    input:
      motion_entity: binary_sensor.hall_motion
      light_target:
        entity_id: light.hall

- id: "bp_motion_light_zero"
  alias: "Motion Light (zero wait)"
  use_blueprint:
    path: homeassistant/motion_light.yaml
    input:
      motion_entity: binary_sensor.kitchen_motion
      light_target:
        entity_id: light.kitchen
      no_motion_wait: 0
"""
        )

    hass = await bootstrap.async_setup_hass(runner.RuntimeConfig(config_dir=cfg_dir))

    service_calls: list[dict[str, Any]] = []

    async def record_service(call: core.ServiceCall):
        service_calls.append(
            {
                "domain": call.domain,
                "service": call.service,
                "data": serialize(dict(call.data)),
            }
        )

    hass.services.async_register("light", "turn_on", record_service)
    hass.services.async_register("light", "turn_off", record_service)

    hass.states.async_set("binary_sensor.hall_motion", "off", {"device_class": "motion"})
    hass.states.async_set("binary_sensor.kitchen_motion", "off", {"device_class": "motion"})
    hass.states.async_set("light.hall", "off")
    hass.states.async_set("light.kitchen", "off")

    await hass.async_start()
    await asyncio.sleep(0.2)

    # Trigger bp_motion_light_zero: off -> on -> off
    hass.states.async_set("binary_sensor.kitchen_motion", "on", {"device_class": "motion"})
    await asyncio.sleep(0.2)
    hass.states.async_set("binary_sensor.kitchen_motion", "off", {"device_class": "motion"})
    await asyncio.sleep(0.3)

    auto_states = {s.entity_id: serialize(s) for s in hass.states.async_all(AUTO_DOMAIN)}
    traces_data = {}
    for key, traces in hass.data.get(DATA_TRACE, {}).items():
        if key.startswith("automation."):
            traces_data[key] = [serialize(t.as_dict()) for t in traces.all_traces()]

    # Validate motion_light with default input (no_motion_wait omitted) via async_validate_config_item
    raw_motion_default = {
        "id": "bp_motion_light_default",
        "alias": "Motion Light (default wait)",
        "use_blueprint": {
            "path": "homeassistant/motion_light.yaml",
            "input": {
                "motion_entity": "binary_sensor.hall_motion",
                "light_target": {"entity_id": "light.hall"},
            },
        },
    }
    validated_motion = await async_validate_config_item(hass, "bp_motion_light_default", raw_motion_default)

    # Also test BlueprintInputs substitution directly for notify_leaving_zone.yaml
    from homeassistant.components.automation.config import async_get_blueprints
    blueprints = async_get_blueprints(hass)
    raw_zone_notify = {
        "id": "bp_zone_notify",
        "alias": "Notify Leaving Home",
        "use_blueprint": {
            "path": "homeassistant/notify_leaving_zone.yaml",
            "input": {
                "person_entity": "person.alice",
                "zone_entity": "zone.home",
                "notify_device": "fake_device_id_123",
            },
        },
    }
    bp_inputs_zone = await blueprints.async_inputs_from_config(raw_zone_notify)
    substituted_zone = bp_inputs_zone.async_substitute()

    try:
        await asyncio.wait_for(hass.async_stop(), timeout=2)
    except Exception:
        pass

    out = {
        "ha_version": "2026.11.2",
        "automation_states": auto_states,
        "service_calls": service_calls,
        "traces": traces_data,
        "validated_motion_default_raw_config": serialize(
            getattr(validated_motion, "raw_config", validated_motion)
        ),
        "substituted_zone_notify_config": serialize(substituted_zone),
    }
    print(json.dumps(out, indent=2, ensure_ascii=False), flush=True)
    os._exit(0)


asyncio.run(main())
