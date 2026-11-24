# compat

Recorded traffic from real Home Assistant, used as the reference that lean-home must match.

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
`bench/.work/capture-mqtt`).
