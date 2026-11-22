# Benchmark-only: register hass_nabucasa as a bare package so `from hass_nabucasa import remote`
# works without running its __init__ (which eagerly imports litellm/openai).
import importlib.util, sys, types
_spec = importlib.util.find_spec("hass_nabucasa")
if _spec:
    _pkg = types.ModuleType("hass_nabucasa"); _pkg.__path__ = list(_spec.submodule_search_locations); _pkg.__spec__ = _spec
    sys.modules["hass_nabucasa"] = _pkg
