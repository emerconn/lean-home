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
| Matter | — | Native via `rs-matter` |
| Cloud/HTTP polling integrations (e.g. Met) | +1 MiB | Leave in the Python host |

The protocol integrations are both the most popular and the most expensive in Python, so porting
them gives the biggest win. Port the rest based on which ones users actually load.

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
7. ESPHome, Matter, Shelly, Hue, Z-Wave JS native.

## Measurement rules

All measurements run on a dev PC under cgroup limits that resemble the target (1 CPU,
capped memory), using cgroup **anonymous** memory, not RSS or `docker stats`, which count file cache.
Target hardware is only used once the project is much further along. See [benchmarks](benchmarks.md).
