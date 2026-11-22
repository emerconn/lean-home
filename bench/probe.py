"""Load one layer of the HA Python stack, then idle. argv: ready_file layer [integrations...]
layer: bare | core | helpers | integ  (integ = helpers + every module of each named integration)"""
import asyncio, gc, importlib, pkgutil, sys, tempfile, time

ready, layer, integs = sys.argv[1], sys.argv[2], sys.argv[3:]
failed = []


async def main():
    if layer == "bare":
        return
    from homeassistant.core import HomeAssistant
    hass = HomeAssistant(tempfile.gettempdir())  # live core object: bus, states, services
    if layer == "core":
        return hass
    # The helper surface a typical integration imports.
    for m in ("config_entries", "helpers.entity", "helpers.entity_platform", "helpers.config_validation",
              "helpers.update_coordinator", "helpers.device_registry", "helpers.entity_registry",
              "helpers.aiohttp_client", "helpers.dispatcher", "helpers.event"):
        importlib.import_module(f"homeassistant.{m}")
    for name in integs:
        pkg = importlib.import_module(f"homeassistant.components.{name}")
        for mod in pkgutil.iter_modules(pkg.__path__):
            try:
                importlib.import_module(f"{pkg.__name__}.{mod.name}")
            except Exception as e:  # some optional modules need extras; record, keep going
                failed.append(f"{name}.{mod.name}: {type(e).__name__}")
    return hass


hass = asyncio.run(main())
gc.collect()
with open(ready, "w") as f:
    f.write(f"modules={len(sys.modules)} litellm={'litellm' in sys.modules} failed={failed}\n")
time.sleep(3600)
