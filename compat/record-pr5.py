#!/usr/bin/env python3
"""Record real HA 2026.11.2 behavior for PR 5:
- sun trigger (sunrise/sunset + offset) and sun condition (after/before/offsets)
- checked against HA's astral (v2.2)

Usage (inside ghcr.io/home-assistant/home-assistant:2026.11.2):
  docker run --rm -i --entrypoint python3 ghcr.io/home-assistant/home-assistant:2026.11.2 - \
    < compat/record-pr5.py > compat/fixtures/step2/pr5-sun.json
"""

import asyncio
from datetime import UTC, datetime, timedelta
import json
import logging
import os
from typing import Any
from unittest.mock import patch

from astral import Observer
from homeassistant import bootstrap, core, runner
from homeassistant.helpers import condition, config_validation as cv, sun as sun_helper
from homeassistant.util import dt as dt_util


async def main():
    logging.disable(logging.WARNING)
    cfg_dir = "/tmp/ha-pr5"
    os.makedirs(cfg_dir, exist_ok=True)
    with open(f"{cfg_dir}/configuration.yaml", "w") as f:
        f.write(
            """homeassistant:
  latitude: 51.5074
  longitude: -0.1278
  elevation: 11
  time_zone: UTC
sun:
"""
        )

    hass = await bootstrap.async_setup_hass(runner.RuntimeConfig(config_dir=cfg_dir))
    await hass.async_start()

    # 1. Exact astral event times for London (51.5074, -0.1278, elev=11, UTC) on 2026-11-24
    ref_times = [
        datetime(2026, 11, 24, 0, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 8, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 16, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 23, 0, 0, tzinfo=UTC),
    ]
    astral_next = []
    for t in ref_times:
        for ev in ("sunrise", "sunset"):
            for off_sec in (0, -1800, 900):
                off = timedelta(seconds=off_sec)
                nxt = sun_helper.get_astral_event_next(hass, ev, utc_point_in_time=t, offset=off)
                astral_next.append(
                    {
                        "utc_point_in_time": t.isoformat(),
                        "event": ev,
                        "offset_seconds": off_sec,
                        "next": nxt.isoformat(),
                    }
                )

    # Also test a second location (Austin, TX, America/Chicago) and polar edge case (Svalbard)
    london_date = {
        "sunrise": sun_helper.get_astral_event_date(hass, "sunrise", datetime(2026, 11, 24, tzinfo=UTC)).isoformat(),
        "sunset": sun_helper.get_astral_event_date(hass, "sunset", datetime(2026, 11, 24, tzinfo=UTC)).isoformat(),
    }

    austin_obs = Observer(30.2672, -97.7431, 150)
    austin_next = {
        "sunrise": sun_helper.get_observer_astral_event_next(
            austin_obs, "sunrise", datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC)
        ).isoformat(),
        "sunset": sun_helper.get_observer_astral_event_next(
            austin_obs, "sunset", datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC)
        ).isoformat(),
    }

    # 2. Sun conditions evaluated at key times on 2026-11-24
    # On 2026-11-24 in London, sunrise is ~07:33 UTC and sunset is ~15:59 UTC
    cond_cases = [
        {"condition": "sun", "after": "sunrise"},
        {"condition": "sun", "before": "sunrise"},
        {"condition": "sun", "after": "sunrise", "before": "sunset"},
        {"condition": "sun", "after": "sunset", "before": "sunrise"},
        {"condition": "sun", "after": "sunset", "after_offset": "-01:00:00"},
        {"condition": "sun", "before": "sunrise", "before_offset": "00:30:00"},
    ]
    eval_times = [
        datetime(2026, 11, 24, 6, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 7, 45, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 15, 30, 0, tzinfo=UTC),
        datetime(2026, 11, 24, 17, 0, 0, tzinfo=UTC),
    ]
    sun_conditions = []
    for cfg in cond_cases:
        validated = await condition.async_validate_condition_config(hass, cfg)
        cond_fn = await condition.async_from_config(hass, validated)
        results = {}
        for t in eval_times:
            with patch.object(dt_util, "utcnow", return_value=t), patch.object(dt_util, "now", return_value=t):
                results[t.isoformat()] = bool(cond_fn(hass))
        sun_conditions.append({"config": cfg, "results": results})

    # 3. Sun entity state
    sun_state = hass.states.get("sun.sun")
    sun_state_dict = (
        {
            "entity_id": sun_state.entity_id,
            "state": sun_state.state,
            "attributes": {k: (v.isoformat() if isinstance(v, datetime) else v) for k, v in sun_state.attributes.items()},
        }
        if sun_state
        else None
    )

    await hass.async_stop()

    out = {
        "ha_version": "2026.11.2",
        "location": {"latitude": 51.5074, "longitude": -0.1278, "elevation": 11, "time_zone": "UTC"},
        "london_2026_11_24": london_date,
        "austin_next_from_12_utc": austin_next,
        "astral_next": astral_next,
        "sun_conditions": sun_conditions,
        "sun_entity_sample": sun_state_dict,
    }
    print(json.dumps(out, indent=2, ensure_ascii=False))


asyncio.run(main())
