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

Each step targets the upstream versions in [compatibility](compatibility.md).


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
2. **Templates.** (Done: `crates/lean-home/src/template.rs`.) HA's globals and filters on
   minijinja: `states` (call, `states.domain.x`, iteration), `is_state`, `is_state_attr`,
   `state_attr`, `has_value`, `float`/`int`/`bool` with defaults, `is_number`, `iif`,
   `is_defined`, `round`, `now()`/`utcnow()`/`as_timestamp`, `to_json`/`from_json`; Python-style
   output (`True`, `None`, `['a']`) and HA's result parsing. MQTT value and command templates
   use it, and `render_template` re-renders on the entities, domains or minute it read. Cases
   come from HA's source and the `automations` recording (the live template's 7 results match in
   order). The ha-src checkout has no `tests/`, so HA's own template tests are still to mine.
   Measured on the dev PC: stripped binary +144 KiB (6.32 → 6.47 MB); process RSS (a quick
   check, not the cgroup method of the [measurement rules](#measurement-rules)) unchanged at
   idle (~6.6 MB) and ~0.3 MB more with 200 live `render_template` subscriptions.
3. **Config.** (Done: `crates/lean-home/src/config.rs`.) `configuration.yaml` as HA's loader
   (`annotatedyaml` on PyYAML) reads it: YAML 1.1 scalars (unquoted `on` is `true`, `3:00:00`
   is 10800, `1e5` a string), anchors, `<<` merge keys, duplicate keys (last wins), and the
   tags `!include`, `!include_dir_list`/`_merge_list`/`_named`/`_merge_named`, `!secret`
   (nearest `secrets.yaml` up to the config dir), `!env_var`. Automations come from every
   `automation`/`automation <label>` key and scripts from `script`/`script <label>`; they are
   collected here and run in step 4. The **core config** (defaults, then
   `.storage/core.config`, then `homeassistant:` keys) now fills `get_config`, including
   `config_source`, the units and the **time zone**: templates' `now()` is local time in it, and
   naive times in `as_timestamp` are read in it. Time zones come from the system zoneinfo, like
   HA's Python `zoneinfo`; UTC needs none. Checked against HA: the `automations` scenario's
   config parses to exactly what HA's loader makes of it
   (`compat/fixtures/automations/configuration.json`), the scalar cases are PyYAML's own
   results, and DST gaps and overlaps match Python's `zoneinfo`. Measured on the dev PC under the
   [measurement rules](#measurement-rules) (`bench/bench.sh`, 1 CPU, 1 GiB): anon memory
   unchanged at 0.6 MiB idle, peak 1.2 MiB; stripped binary +237 KiB (6.47 → 6.71 MB).
   `chrono-tz` was tried first and dropped: its embedded time-zone database cost +2.1 MB of
   binary and +2 MB RSS.
4. **Automations and scripts.** Start with two checks on the template engine from 2.2, since
   conditions and action data now depend on it, so a template bug is fixed in the step it would
   break:
   - **HA's template tests.** Fetch `tests/helpers/template/` from HA core at the release we
     match (not in the ha-src checkout) and port the cases for the helpers 2.2 built. Fix what
     they catch.
   - **Template errors.** Add a failing template (with `report_errors`) to the `automations`
     scenario and re-record, so `render_template`'s error events are checked against HA. Today
     they follow HA's source, unverified.

   Then: triggers (state, time, MQTT first), conditions, actions; the `automation.*`/`script.*`
   entities and services; the WebSocket commands the editor uses. Add the template helpers
   automations need from "Deferred" below.

Measure memory and binary size before and after, as for steps 0 and 1.

### When it should run on the router: a container image

Not a roadmap step: do it whenever lean-home should run on real hardware.

1. **Static build:** musl, aarch64 (and x86_64), in a `FROM scratch` image, built in CI. Copy
   the system's `/usr/share/zoneinfo` into it (a few MB on disk, nothing in memory: only the
   configured zone is read), or a `time_zone` other than UTC fails to load.
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
| Template helpers beyond the core set: datetime arithmetic (`timedelta`, `today_at`, `relative_time`), areas/devices/labels, `expand`, regex, `state_translated` | With 2.4, by what automations use | Each is small; add them against HA's tests when a case needs one |
| `render_template` has no rate limit (HA: 1/s for domain listeners, 1/min for all states) | If a dashboard iterating all states proves costly | Re-rendering on every change is correct, only more work |
| Registries are read-only: new MQTT entities aren't written to `core.entity_registry` (HA later picks the same ids, normally) | Step 5 | Needed once lean-home renames entities or assigns areas itself, i.e. with config flows; writing HA's file needs care not to corrupt it |
| HA's opt-in IP ban (`http: login_attempts_threshold`, `ip_bans.yaml`) | Now unblocked (2.3 parses `configuration.yaml`); when a user asks | Off by default in HA too; lean-home already serializes password checks and backs off per IP |
| Packages (`homeassistant: packages:`): merged into the config by HA; lean-home warns and ignores them, so automations in packages are missing | Before automations ship to users (with or right after 2.4) | Power users' configs use them; HA's merge rules per domain need care |
| Blueprints: `!input` loads as a `{"!input": name}` marker, `use_blueprint` automations aren't expanded | With 2.4 or after, by demand | Blueprint automations are common, but need the automation engine first |
| YAML corner cases: non-string keys become Python's `str()` of them (`1:` → `"1"`), `.inf`/`.nan` become strings, dates become strings rather than date objects | When a config needs them | JSON has no such keys or numbers; HA configs quote dates where they matter |
| `homeassistant:` keys beyond location and units (`customize`, `allowlist_external_dirs`, `media_dirs`, `auth_providers`…) | With the features that use them | Nothing reads them yet |
| MQTT websocket transport | When a user needs it | Rare; brokers behind a reverse proxy |
| MQTT template-schema lights (`schema: template`) | When a device needs it | Rare compared to the default and JSON schemas |
| MQTT device discovery `child_devices` (HA 2026.x) | When a firmware sends it | New; nothing we know of uses it yet |
| MQTT v5 (HA's default for new entries) | Not planned | 3.1.1 works with every broker HA supports; v5 adds nothing we use |
| MQTT platforms beyond the four (cover, climate, fan, button, number, select…) | By demand | Order by what MQTT users' devices expose |

## Measurement rules

All measurements run on a dev PC under cgroup limits that resemble the target (1 CPU,
capped memory), using cgroup **anonymous** memory, not RSS or `docker stats`, which count file cache.
Target hardware is only used once the project is much further along. See [benchmarks](benchmarks.md).
