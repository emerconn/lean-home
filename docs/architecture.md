# Architecture

## The core idea: be compatible at the wire, not in Python internals

The official HA frontend is a separate TypeScript app that talks to the backend only through the
**WebSocket API** (plus a few REST endpoints for auth and onboarding). The Companion apps use REST
plus the `mobile_app` webhooks. If those APIs are implemented faithfully, the full UI, dashboards
and apps come for free. Compatibility is defined and tested at that boundary.

The HA API is the **native** API, not an optional layer. A second "own" API would double what we
maintain for little gain.

## What "compatible" means, in priority order

1. **WebSocket and REST APIs**: frontend and Companion apps work unchanged.
2. **On-disk formats**: `configuration.yaml` (`!include`, `!secret`, `!include_dir_*`), the
   `.storage/*.json` registries, and the recorder's SQLite schema. Both implementations can then run
   against the same config dir, which gives a migration path and a way back.
3. **Automations, scripts, templates**: existing YAML runs unchanged.
4. **Integrations**: native Rust for the common protocols, a Python plugin host for the rest.

Out of scope: Supervisor/add-ons (use your container platform) and HA Cloud. HACS **frontend** cards
work, since they're just JavaScript served to the frontend.

## Components

```
                 official HA frontend / Companion apps / HA API clients
                                  │  WebSocket + REST (HA wire format)
┌─────────────────────────────────▼─────────────────────────────────────────┐
│ Rust core (single process)                                                │
│  api · auth · state machine · event bus · services · registries           │
│  automations/scripts (minijinja templates) · recorder (SQLite, HA schema) │
│  native integrations: MQTT discovery, ESPHome, Matter, Shelly, Hue, ZWJS  │
└─────────────────────────────────┬─────────────────────────────────────────┘
                                  │ unix socket, compact binary RPC
                                  │ (only started if a configured integration needs it)
┌─────────────────────────────────▼─────────────────────────────────────────┐
│ Python plugin host (separate process, can be capped/restarted on its own) │
│  HA's real helpers + integrations, but thin replacement packages for      │
│  websocket_api / http / cloud that forward registrations to the Rust core │
└───────────────────────────────────────────────────────────────────────────┘
```

### Why the plugin host replaces `websocket_api`, `http` and `cloud`

Measured on HA 2026.11.2 (see [benchmarks](benchmarks.md#5-the-slim-plugin-host-experiments)):
entity-platform components such as `weather`, `light` and `persistent_notification` register
WebSocket commands **at import time**. Loading almost any integration therefore pulls in
`websocket_api` → `http` (aiohttp server) → `helpers.network` → `hass_nabucasa` (HA Cloud) → boto3.
Making individual imports lazy inside HA only helps an empty host (112 → 81 MiB). With five real
integrations loaded, every variant was still 222 MiB. In our design the Rust core owns HTTP,
WebSocket and Cloud, so the host can ship thin packages under those names that keep the
registration APIs (`async_register_command`, `HomeAssistantView`, …) and forward to Rust.
This is the next thing to build and measure.

### Integration strategy

| Integration | Python cost (2026.11.2) | Plan |
|---|---|---|
| MQTT (+ discovery) | +45 MiB | Native. Covers Zigbee2MQTT, Z-Wave JS UI, Tasmota, Frigate… |
| ESPHome | +88 MiB | Native (protobuf API) |
| Shelly | +82 MiB | Native |
| Hue | +7 MiB | Native (common), cheap in Python too |
| Matter | — | Client of the Matter Server sidecar first, `rs-matter` controller later (see [Radio sidecars](#radio-sidecars-matter-z-wave-zigbee)) |
| Cloud/HTTP polling integrations (e.g. Met) | +1 MiB | Leave in the Python host |

The protocol integrations are both the most popular and the most expensive in Python, so porting
them gives the biggest win. Port the rest based on which ones users actually load.

### Radio sidecars: Matter, Z-Wave, Zigbee

Stock HA doesn't contain the Matter or Z-Wave stacks. Its `matter` and `zwave_js` integrations are
**WebSocket clients** of separate servers. lean-home does the same first, so both work on day one
with no protocol code of our own, then replaces sidecars only where it pays off.

| | Sidecar today (license) | Rust path | Effort |
|---|---|---|---|
| **Matter** | `python-matter-server` (Apache 2.0, Open Home Foundation), on the Matter SDK `connectedhomeip` (Apache 2.0) | `rs-matter` (Apache 2.0, under project-chip). Mostly device-side; how complete its **controller** side is (commissioning over BLE, CASE sessions, fabric management, cluster client) is unverified, so check before planning on it. Cluster definitions are generated from the SDK's XML | Large, with a head start |
| **Z-Wave** | Z-Wave JS: driver (MIT), `zwave-js-server` (Apache 2.0) or Z-Wave JS UI (MIT, adds a web UI) | Only small early crates exist. The hard part is device quirks: Z-Wave JS's **device config database** (thousands of MIT-licensed JSON files) is reusable from Rust rather than rewritten. Serial API spec public since 2020; S0/S2 security needed | Very large; keep the sidecar |
| **Zigbee** | Zigbee2MQTT (GPL-3.0) | Covered by native MQTT discovery; ZHA is Python and stays in the plugin host | None needed |

Order: speak both servers' WebSocket APIs → Matter in Rust (`rs-matter` controller) → Z-Wave last
or never. Rough sidecar footprints, **estimates, not measured** under our rules: Matter Server
~60–120 MB, Z-Wave JS Server ~50–100 MB, Z-Wave JS UI ~100–200 MB. Native Rust replacements would
likely be a few MB each; measure before claiming.

### Voice: Assist pipeline and Wyoming

HA's voice assistant (Assist) chains wake word → speech-to-text → intent → text-to-speech. Local
engines plug in over the **Wyoming protocol**: JSON-lines events plus raw PCM chunks over TCP,
simple enough to implement natively in Rust as a client. Satellites (HA Voice Preview Edition,
FutureProofHomes Satellite1, ESP32-S3-BOX-3) are **ESPHome** devices, so native ESPHome covers
them; Raspberry Pi satellites use `wyoming-satellite`.

STT candidates for small hardware:

- **Cactus Whistle** (Cactus Compute, Apache 2.0): a 16.9 MB model, 7 languages, word timestamps,
  run by the Cactus C++ engine via `pip install cactus-compute` (`cactus.transcribe(wav)`, 16 kHz).
  No Wyoming server exists yet; a thin Wyoming wrapper (Python today, Rust around the C++ engine
  later) is needed. ARM64 Linux support and accuracy are unverified.
- **faster-whisper** via `wyoming-faster-whisper`: HA's standard local path; models from ~75 MB
  to GB, slow on a Pi 4 beyond the smallest.
- **Speech-to-Phrase** (Open Home Foundation): under a second on a Pi 4, but only HA's built-in commands.

Plan: implement the Assist pipeline and a Wyoming client in the core; treat STT/TTS/wake-word
engines as Wyoming sidecars.

## Rust stack

- **Runtime:** `tokio` with `current_thread`: home automation is I/O-bound, and one thread keeps memory and
  context switches down. CPU-heavy work (recorder writes) goes to `spawn_blocking`.
- **HTTP/WS:** `axum` (built-in WebSocket support), `tower-http` for static frontend files.
- **JSON:** `serde_json`. Serialize each state change once and fan the same `Bytes` out to all
  subscribers.
- **State:** interned entity IDs and attribute keys (`Arc<str>`), and `Arc<State>`, so a change swaps a pointer.
  `tokio::sync::broadcast` for the bus.
- **Templates:** `minijinja` + HA's globals and filters (`states()`, `is_state`, `state_attr`, `iif`…).
- **YAML:** `saphyr` or `serde_yaml_ng` (`serde_yaml` is unmaintained) + HA's custom tags.
- **Recorder:** `rusqlite` against HA's schema, WAL, batched commits (configurable interval, which matters for
  flash wear).
- **MQTT:** `rumqttc`. **Allocator:** system allocator unless measurements show `mimalloc` helps.
- **Hygiene:** a cargo workspace, `clippy -D warnings`, `cargo-deny`, and static musl builds in `FROM scratch` images.

## Image size

The binary is small. The **frontend assets** are the big part: the `home-assistant-frontend` package
is ~241 MB, with ES5 and modern builds, gzip and brotli copies, and every locale. Ship only the modern
build, brotli only, and the locales needed. The Python plugin host is a separate, optional image
(glibc-based: some integration dependencies have no musllinux aarch64 wheels).

## Testing: differential against real HA

Run real HA next to lean-home on the same config. Record WebSocket/REST traffic, replay it
against both, and diff the responses. Mine HA's own pytest fixtures for template and automation
cases. "Compatible" means passing those diffs, not looking right.

## Roadmap

0. **Frontend loads**: auth/onboarding, `get_states`, `subscribe_events`, `subscribe_entities`,
   `get_config`, `get_services`, `call_service`, demo entities. This proves the approach.
1. MQTT + discovery: real devices.
2. Automations, scripts, templates.
3. Recorder, history, logbook, statistics: graphs and the energy dashboard.
4. `mobile_app`: Companion apps connect.
5. Config flows for native integrations (UI setup).
6. Python plugin host with thin `websocket_api`/`http`/`cloud`. Measure, then load real HACS integrations.
7. ESPHome, Shelly, Hue native; Matter and Z-Wave JS as WebSocket clients of their servers, then
   Matter natively via `rs-matter`.
8. Assist pipeline + Wyoming client (STT/TTS/wake-word sidecars; ESPHome voice satellites).

Steps 0 and 1 are done (MQTT covers light, switch, sensor and binary_sensor, per-component and
device-based discovery, plain TCP and TLS). CI replays the recorded HA sessions in
`compat/fixtures/` against lean-home on every PR (`compat/replay.sh`, `compat/compare.py`).

### Next: step 2 (templates, automations, scripts)

Same method as steps 0 and 1: record real HA first, build against the recording, add the scenario
to the CI replay.

1. **Record.** (Done: `compat/capture-mqtt.sh automations`, `compat/fixtures/automations/`.) A scenario: a few automations and scripts in YAML
   (state, time and MQTT triggers; conditions; service calls, delays, `choose`), the frontend's
   automation and script pages, and template rendering (`render_template` with `states()`).
2. **Templates.** HA's globals and filters on minijinja: `states()`, `is_state`, `state_attr`,
   `float(0)`/`int(0)` with defaults, `iif`, `is_defined`, `now()`… Use HA's own template tests
   as cases. This also fixes MQTT value templates that use them (see "Deferred" below) and
   `render_template` with live listeners.
3. **Config.** Read `configuration.yaml` with HA's tags (`!include`, `!secret`,
   `!include_dir_*`), then `automations.yaml` and `scripts.yaml`.
4. **Automations and scripts.** Triggers (state, time, MQTT first), conditions, actions; the
   `automation.*`/`script.*` entities and services; the WebSocket commands the editor uses.

Measure memory and binary size before and after, as for steps 0 and 1.

### When it should run on the router: a container image

Not a roadmap step: do it whenever lean-home should run on real hardware.

1. **Static build:** musl, aarch64 (and x86_64), in a `FROM scratch` image, built in CI.
2. **Trimmed frontend.** The `home-assistant-frontend` package is 283 MB; the image needs far less
   (estimate: 20–30 MB, measure it):

   | Part | Size | Keep? |
   |---|---|---|
   | `frontend_latest` | 78 MB | One compressed copy of each file instead of three; drop the 28 MB of `.map` debug files |
   | `frontend_es5` (old browsers) | 85 MB | Probably not; old wall tablets and Android WebViews may need it, so make it opt-in |
   | `static/translations` | 87 MB | Brotli copies only: 9.4 MB for all languages, 0.25 MB for English |
   | `static/locale-data` | 24 MB | Likely cut down per language too |

   Check before dropping gzip: Chromium is believed to accept brotli only over HTTPS, and most
   people open the hub over plain HTTP on the LAN, so the gzip copies (~8 MB) may have to stay.
   Test it rather than guess.
3. **Measure on the target** (1 GB ARM64): memory, idle CPU, startup time and image size, under
   the [measurement rules](#measurement-rules). Until now everything was measured on the dev PC.

### Deferred, and when to do it

Known gaps we chose not to close yet. Each also has a `shortcut:` comment at the code it affects.

| Gap | Do it | Why then |
|---|---|---|
| Templates have no HA helpers (`states()`, `float(0)`, `is_defined`…), so MQTT value templates using them fail and the message is dropped | Step 2 | One template engine serves automations, scripts and MQTT; build it once there |
| Registries are read-only: new MQTT entities aren't written to `core.entity_registry` (HA later picks the same ids, normally) | Step 5 | Needed once lean-home renames entities or assigns areas itself, i.e. with config flows; writing HA's file needs care not to corrupt it |
| HA's opt-in IP ban (`http: login_attempts_threshold`, `ip_bans.yaml`) | With `configuration.yaml` parsing | Off by default in HA too; lean-home already serializes password checks and backs off per IP |
| MQTT websocket transport | When a user needs it | Rare; brokers behind a reverse proxy |
| MQTT template-schema lights (`schema: template`) | When a device needs it | Rare compared to the default and JSON schemas |
| MQTT device discovery `child_devices` (HA 2026.x) | When a firmware sends it | New; nothing we know of uses it yet |
| MQTT v5 (HA's default for new entries) | Not planned | 3.1.1 works with every broker HA supports; v5 adds nothing we use |
| MQTT platforms beyond the four (cover, climate, fan, button, number, select…) | By demand | Order by what MQTT users' devices expose |

## Measurement rules

All measurements run on a dev PC under cgroup limits that resemble the target (1 CPU,
capped memory), using cgroup **anonymous** memory, not RSS or `docker stats`, which count file cache.
Target hardware is only used once the project is much further along. See [benchmarks](benchmarks.md).
