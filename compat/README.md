# compat

Recorded traffic from real Home Assistant, used as the reference that lean-home must match.
The HA, frontend and Mosquitto versions are pinned in [`versions.env`](versions.env); see
[`docs/compatibility.md`](../docs/compatibility.md) for upgrading them.

`fixtures/frontend-load/` is one frontend session against HA **2026.11.2** (frontend **20261028.6**):
log in through the real login form, wait for the dashboard, toggle `light.kitchen`.
`http.jsonl` holds the `/`, `/auth/*`, `/api/*` and `/.well-known/*` requests, `ws.jsonl` every WebSocket frame
(`send`/`recv`), and `index.html` the page as HA renders it. Tokens, auth codes, PKCE values,
timestamps (ISO → `<ts>`, epoch → `0`), ULIDs and 32-hex ids are replaced with placeholders.
HA coalesces WS messages into arrays depending on timing, so compare per message, not per frame.

Re-record with `compat/capture.sh` (needs docker, curl, jq, node, `/usr/bin/chromium`, and
`npm i puppeteer-core` in `bench/.work/node`). Config and temp files go to `bench/.work/capture`.

## MQTT discovery

`fixtures/mqtt-discovery/` is the same HA with the MQTT integration on a Mosquitto broker
(`eclipse-mosquitto:2`) and five retained discovery configs on one device "Demo Device": a
default-schema light with brightness, a JSON-schema light (sent with abbreviated keys and `~`),
a switch, a temperature sensor with a `value_template`, and a door binary_sensor. A small
responder echoes each command to its state topic, like a real device. The browser toggles both
lights and the switch and sets the first light to 50 %; then the sensor and door change and the
device goes offline (`lh/status` → `offline`).

- `ws.jsonl`, `http.jsonl`: the frontend session, as above.
- `mqtt.jsonl`: every MQTT message in broker order (`topic`, `payload`, `retain`, `qos`).
- `broker-subscriptions.txt`: the topics HA subscribed to, in order, with QoS.
- `core.config_entries.json`, `core.entity_registry.json`, `core.device_registry.json`: the
  `.storage` files HA wrote, which lean-home must read.

Re-record with `compat/capture-mqtt.sh` (same requirements; temp files and the broker log go to
`bench/.work/capture-mqtt-discovery`).

## Automations, scripts and templates

`fixtures/automations/` is the MQTT setup above plus the automations and script in
`compat/automations/` (copied into HA's config): a state trigger with a state condition, an MQTT
trigger that runs a script with a field, and a time trigger at 03:00 with a template condition, which
never fires during the recording (its entity, config and `automation.turn_off` still are). The
script turns a light on, waits 1 s and `choose`s by the fan's state. The browser opens a
`render_template` subscription (`states()`, `is_state`, `state_attr`, `float(0)`, `iif`) that stays
open while the steps run, and five that show how HA reports problems: `'abc' | float` with and
without `report_errors`, `nope.attr` (undefined), `nope` in strict mode, and `{{ nope }} x`
(renders, with a warning). Then: fan on (fires the state automation), an MQTT button press (fires the
script), fan off, door open, `automation.trigger` (now blocked by its condition), `script.blink`
(takes the `default` branch). Then it opens the automation list, editor and trace pages and the
script list and editor. Same files as `mqtt-discovery`.

`configuration.json` is HA's own loader's result for `compat/automations/configuration.yaml`
(`annotatedyaml` in the HA image); `config.rs` tests that lean-home's loader gives the same.

Re-record with `compat/capture-mqtt.sh automations`. Two recordings differ only in set-ordered data,
e.g. `render_template`'s `listeners.entities`. Not in CI yet: lean-home doesn't run automations.

## Template cases

`fixtures/templates/cases.jsonl` is every literal template in HA's own template tests
(`tests/helpers/template/` at `HA_VERSION`), each rendered by HA with the states its test set
and a frozen clock (2026-11-24 12:00 UTC): the text, the parsed result, the error, the
listeners and the names it uses. `template_ha_cases.rs` replays them through lean-home.
Regenerate with:

```bash
git clone -q --filter=blob:none --sparse --depth 1 --branch "$HA_VERSION" https://github.com/home-assistant/core bench/.work/ha-core-tests
git -C bench/.work/ha-core-tests sparse-checkout set tests/helpers/template
docker run --rm -v "$PWD/bench/.work/ha-core-tests/tests/helpers/template:/tests:ro" \
  -v "$PWD/compat/template-cases.py:/gen.py:ro" --entrypoint python3 "$HA_IMAGE" /gen.py /tests \
  > compat/fixtures/templates/cases.jsonl
```

The generator only parses the test files (Python's `ast`); it never runs them.

## Replay and compare

`compat/replay.sh <scenario>` runs the same scenario against lean-home (same login, browser steps,
and for MQTT the same broker, devices and responder, from `mqtt-scenario.sh`) into
`bench/.work/replay/<scenario>/`; `compat/make-config.py` builds its config dir from the fixtures.
`compat/compare.py <scenario> compat/fixtures/<scenario> bench/.work/replay/<scenario>` then
checks it per message against the recording and exits non-zero on any difference. Its tolerances
(set-ordered data in HA, the demo entities, unimplemented commands) are listed in the script. CI
runs both scenarios (`.github/workflows/compat.yaml`). Locally, set `FRONTEND` to a
`hass_frontend` dir and `CHROMIUM` if Chromium isn't at `/usr/bin/chromium`.
