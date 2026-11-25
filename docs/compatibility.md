# Upstream versions and compatibility

lean-home copies behaviour from upstream projects at fixed versions. This page lists those
versions, what in lean-home depends on each, and how to move to a new one, so an upstream
release turns into a known list of things to check.

## Versions

Pinned in [`compat/versions.env`](../compat/versions.env), the only place they are set: the
`compat/` scripts and the Compat workflow source it, and a test fails if `HA_VERSION` in
`crates/lean-home/src/main.rs` differs.

| Upstream | Version | Pinned as |
|---|---|---|
| Home Assistant Core | 2026.11.2 | `HA_VERSION`, `HA_IMAGE` |
| HA frontend (`home-assistant-frontend`) | 20261028.6 | `HA_FRONTEND` (the one bundled in the HA image) |
| Mosquitto | 2.1.4 | `MOSQUITTO_IMAGE` (`eclipse-mosquitto:2.1.4-alpine`) |

These come with the HA image, so they change when `HA_VERSION` does. They aren't pinned
separately, but lean-home copies their behaviour, so check them on every upgrade (see below):

| Upstream | Version in the HA image | What lean-home copies |
|---|---|---|
| Python | 3.14.10 | `str()`/`repr()` of values and floats in templates; `zoneinfo`'s handling of DST gaps and overlaps |
| PyYAML | 6.0.3 | YAML 1.1 scalar resolution (`on`, `3:00:00`, `0777`…) |
| annotatedyaml | 1.0.2 | HA's YAML tags, `secrets.yaml` lookup, `!include_dir_*` file order |
| tzdata | 2026.6 | Nothing at build time: lean-home reads the host's zoneinfo at run time |

Benchmark baselines are separate and pinned in `bench/` (HA 2026.1.1 and a
`yon/homeassistant-rs` commit in `bench/setup-hars.sh`). They compare footprints; lean-home's
compatibility doesn't depend on them.

## What depends on them

### Recorded fixtures

All recorded from `HA_IMAGE` with `HA_FRONTEND`; see [`compat/README.md`](../compat/README.md).

| Fixture | Recorded by | Also needs |
|---|---|---|
| `compat/fixtures/frontend-load/` | `compat/capture.sh` | |
| `compat/fixtures/mqtt-discovery/` | `compat/capture-mqtt.sh` | `MOSQUITTO_IMAGE` |
| `compat/fixtures/automations/` (`ws.jsonl` etc.) | `compat/capture-mqtt.sh automations` | `MOSQUITTO_IMAGE` |
| `compat/fixtures/automations/configuration.json` | annotatedyaml in the HA image (command below) | |
| `compat/fixtures/templates/cases.jsonl` | `compat/template-cases.py` in the HA image, from HA's `tests/helpers/template/` at `HA_VERSION` ([command](../compat/README.md#template-cases)) | The HA Core repo at that tag |

### Code that follows upstream source

Paths are in the HA Core repository (`github.com/home-assistant/core`), at the `HA_VERSION` tag.

| lean-home | Follows |
|---|---|
| `auth.rs`, `http.rs` (auth routes) | `homeassistant/auth/auth_store.py`, `auth/providers/homeassistant.py`, `components/auth/{__init__,login_flow,indieauth}.py` |
| `http.rs` (index page) | `components/frontend/__init__.py`, and the frontend package's `index.html` |
| `ws.rs` | `components/websocket_api/{commands,connection,messages,http}.py`, `components/frontend/storage.py`, plus the commands the frontend sends (`ws.jsonl` fixtures) |
| `state.rs` | `homeassistant/core.py` (`State`, `Event`), `components/websocket_api/messages.py` (compressed states and diffs) |
| `registry.rs` | `helpers/entity_registry.py`, `helpers/device_registry.py` (storage formats) |
| `services.rs` | `components/light/services.yaml` and the switch equivalents |
| `template.rs` | `helpers/template/` (the package and its `extensions/`), `helpers/event.py` (`TrackTemplateResultInfo`), `handle_render_template` in `components/websocket_api/commands.py` |
| `config.rs` | annotatedyaml `loader.py` and `constructors.py` (not in HA Core; in the image); PyYAML's resolver; `core_config.py` (core config, units); `config.py` (`extract_domain_configs`); `util/unit_system.py` |
| `mqtt/abbreviations.rs` | `components/mqtt/abbreviations.py`. Says "regenerate", but there is no generator script yet: update it by hand from the diff |
| `mqtt/entity.rs` | `components/mqtt/{switch,binary_sensor,sensor}.py`, `components/mqtt/light/`, `components/mqtt/{entity,models}.py` |
| `mqtt/mod.rs`, `mqtt/tls.rs` | `components/mqtt/{discovery,client}.py`, and the MQTT config entry HA stores |

## Upgrading

Renovate (`.github/renovate.json5`, weekly) watches the pins: for a new HA release it opens one
PR bumping `HA_VERSION`, `HA_IMAGE` and `HA_VERSION` in `main.rs` (stable releases only: no
betas, dev builds or floating tags), and a separate one for Mosquitto. That PR is where the
upgrade happens: it stays red until the steps below are done on its branch. Renovate doesn't
bump `HA_FRONTEND` on its own, since it must match the frontend bundled in the HA image.

For a new HA release (the others follow from it, except Mosquitto):

1. **Read what changed.** Diff the source paths above between the two tags, for example:
   ```bash
   git clone --filter=blob:none https://github.com/home-assistant/core ha-core && cd ha-core
   git diff 2026.11.2 2026.12.0 --stat -- homeassistant/helpers/template homeassistant/components/websocket_api \
     homeassistant/components/mqtt homeassistant/auth homeassistant/components/auth homeassistant/core.py \
     homeassistant/core_config.py homeassistant/config.py homeassistant/helpers/entity_registry.py \
     homeassistant/helpers/device_registry.py homeassistant/components/frontend
   ```
   `bench/.work/ha-src` is an unpacked copy of one release, not a git checkout, so it can't be
   diffed. Also skim the release notes for breaking changes to the WebSocket API, storage
   formats and the YAML loader.
2. **Check the bundled versions** and update the second table above:
   ```bash
   docker run --rm --entrypoint python3 ghcr.io/home-assistant/home-assistant:<new> -c \
     'import sys, yaml; from importlib.metadata import version as v; print(sys.version.split()[0], "PyYAML", yaml.__version__, "annotatedyaml", v("annotatedyaml"), "tzdata", v("tzdata"), "frontend", v("home-assistant-frontend"))'
   ```
   For annotatedyaml or PyYAML changes, read their diffs too (`pip download annotatedyaml==<old>`
   and `<new>`).
3. **Bump `compat/versions.env`** (`HA_VERSION`, `HA_IMAGE`, `HA_FRONTEND`) and `HA_VERSION`
   in `main.rs`. To try a release before bumping, the capture scripts take `HA_VER=<new>`.
4. **Re-record and diff the fixtures.** Run `compat/capture.sh`, `compat/capture-mqtt.sh`
   and `compat/capture-mqtt.sh automations`, then `git diff compat/fixtures`. Differences
   that are only ids, timestamps or set order are noise; anything else is an upstream change
   lean-home must follow. Regenerate `configuration.json`:
   ```bash
   docker run --rm -v "$PWD/compat/automations:/c:ro" --entrypoint python3 "$HA_IMAGE" -c \
     'import json; from pathlib import Path; from annotatedyaml.loader import load_yaml_dict, Secrets; print(json.dumps(load_yaml_dict("/c/configuration.yaml", Secrets(Path("/c"))), indent=2, ensure_ascii=False))' \
     > compat/fixtures/automations/configuration.json
   ```
   Regenerate the template cases from the new release's tests
   ([command](../compat/README.md#template-cases)) and diff them too.
5. **Update the code** for what steps 1–4 turned up, including `mqtt/abbreviations.rs`. Then
   run the replays (`compat/replay.sh` and `compat/compare.py` for each CI scenario) and
   `cargo test --workspace`.
6. **Update this page and the file comments** that name a version (`mqtt/abbreviations.rs`,
   `mqtt/entity.rs`), and the versions in `README.md` and `compat/README.md`.

For Mosquitto: bump `MOSQUITTO_IMAGE`, re-record the two MQTT scenarios and check that
`mqtt.jsonl` and `broker-subscriptions.txt` only changed in noise.
