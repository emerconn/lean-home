#!/usr/bin/env python3
"""Load compat/packages/ with HA's own config loader (`async_hass_config_yaml`, which merges
`homeassistant: packages:`) and print the automation and script keys it ends with, and the
errors it logs. config.rs must merge the same way.

usage: docker run --rm -i -v "$PWD/compat/packages:/c:ro" --entrypoint python3 "$HA_IMAGE" - \
         < compat/packages-config.py > compat/fixtures/packages/merged.json
"""

import asyncio
import json
import logging

from homeassistant import core, loader
from homeassistant.config import async_hass_config_yaml

errors = []


class Collect(logging.Handler):
    def emit(self, record):
        errors.append(record.getMessage())


logging.getLogger("homeassistant.config").addHandler(Collect(logging.ERROR))


async def main():
    hass = core.HomeAssistant("/c")
    loader.async_setup(hass)
    config = await async_hass_config_yaml(hass)
    kept = {k: v for k, v in config.items() if k.split(" ")[0] in ("automation", "script")}
    print(json.dumps({"config": kept, "errors": errors}, indent=2, ensure_ascii=False))
    await hass.async_stop(force=True)


asyncio.run(main())
