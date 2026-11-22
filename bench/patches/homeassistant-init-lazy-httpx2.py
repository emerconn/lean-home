"""Init file for Home Assistant — benchmark variant: httpx2 alias installed lazily.

Same behavior as 2026.11.2's __init__.py, except httpx2.alias_httpx() and the certifi
SSL setup run on the first import of httpx/httpcore/httpx2/httpcore2 instead of at startup.
"""

import importlib.abc
import importlib.util
import sys

import probatio
from probatio import BuildPolicy, set_build_policy
from probatio.compat import install_as_voluptuous

install_as_voluptuous()
set_build_policy(BuildPolicy.LAZY)
_ = probatio.to_field_list
_ = probatio.to_openapi


class _LazyHttpxAlias(importlib.abc.MetaPathFinder, importlib.abc.Loader):
    """One-shot finder: first import of any HTTP-client name installs the alias, then steps aside."""

    _NAMES = frozenset({"httpx", "httpcore", "httpx2", "httpcore2"})

    def __init__(self):
        self._stash = {}

    def find_spec(self, name, path=None, target=None):
        if name not in self._NAMES:
            return None
        sys.meta_path.remove(self)
        import httpx2  # noqa: PLC0415

        httpx2.alias_httpx()
        from .util.httpx2_ssl import setup_certifi_ssl_context  # noqa: PLC0415

        setup_certifi_ssl_context()
        # The requested name is now loaded. If it stays in sys.modules, importlib swaps in its
        # real spec and executes it a second time; pop it and hand the same object back instead.
        module = sys.modules.pop(name, None)
        if module is None:
            return None
        self._stash[name] = (module, module.__spec__, module.__loader__)
        return importlib.util.spec_from_loader(name, self)

    def create_module(self, spec):
        return self._stash[spec.name][0]

    def exec_module(self, module):
        # module_from_spec overwrote these with ours; restore the originals so it looks like stock.
        _, module.__spec__, module.__loader__ = self._stash.pop(module.__spec__.name)


sys.meta_path.insert(0, _LazyHttpxAlias())
