#!/usr/bin/env python3
"""Build a throwaway lean-home config dir for compat/replay.sh.

usage: make-config.py <out-dir> [<mqtt-fixture-dir> <broker-host> <broker-port>]

Always writes the auth files (one owner user `dev`, password `dev`, as in the recordings). With an
MQTT fixture dir, also writes the config entries and registries HA wrote during the recording, with
the normalizer's placeholders turned back into valid values.
"""

import json
import sys
from pathlib import Path

# bcrypt (cost 12) of the public test password "dev" that the recordings log in with; not a secret.
DEV_HASH = "JDJiJDEyJG5hT1p6cGJWb0NSN2dEVEpGeHM3UU85cml6N1VBamszYjZRYTRKTkV4aXVPZy92WDVPVGtl"
USER_ID = "0123456789abcdef0123456789abcdef"

# norm.mjs replaced every id and timestamp with the same placeholder, so links between files
# (an entity's device_id, a device's config_entry_id) can't be told apart again. One fixed value
# per kind keeps every link resolvable; lean-home matches devices by identifiers, not by id.
PLACEHOLDERS = {
    '"<ts>"': '"2026-11-24T00:00:00.000000+00:00"',
    "<hex32>": "fedcba9876543210fedcba9876543210",
    "<ulid>": "01KAQ0000000000000000000ZZ",
}


def store(key, data):
    return {"version": 1, "minor_version": 1, "key": key, "data": data}


def main():
    out = Path(sys.argv[1]) / ".storage"
    out.mkdir(parents=True, exist_ok=True)
    write = lambda name, obj: (out / name).write_text(json.dumps(obj, indent=2))
    write(
        "auth_provider.homeassistant",
        store("auth_provider.homeassistant", {"users": [{"username": "dev", "password": DEV_HASH}]}),
    )
    write("auth", store("auth", {
        "users": [{"id": USER_ID, "group_ids": ["system-admin"], "is_owner": True, "is_active": True,
                   "name": "Dev", "system_generated": False, "local_only": False}],
        "groups": [{"id": "system-admin", "name": "Administrators"}],
        "credentials": [{"id": "1" * 32, "user_id": USER_ID, "auth_provider_type": "homeassistant",
                         "auth_provider_id": None, "data": {"username": "dev"}}],
        "refresh_tokens": [],
    }))
    if len(sys.argv) == 2:
        return
    fixtures, host, port = Path(sys.argv[2]), sys.argv[3], int(sys.argv[4])
    for name in ("core.config_entries", "core.entity_registry", "core.device_registry"):
        text = (fixtures / f"{name}.json").read_text()
        for placeholder, value in PLACEHOLDERS.items():
            text = text.replace(placeholder, value)
        data = json.loads(text)
        if name == "core.config_entries":
            for entry in data["data"]["entries"]:
                if entry["domain"] == "mqtt":
                    entry["data"].update(broker=host, port=port)
        write(name, data)


if __name__ == "__main__":
    main()
