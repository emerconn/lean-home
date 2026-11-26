# lean-home

A Home Assistant–compatible home automation core in Rust, built to run on hardware stock
Home Assistant can't: a container on a 1 GB ARM64 router, an old SBC, anything small.

> **Not affiliated with or endorsed by Home Assistant / the Open Home Foundation.**
> "Home Assistant" is their trademark; we use it only to describe compatibility.

## Goals

1. **Full API compliance**: the official Home Assistant frontend and Companion apps work
   unchanged, over the same WebSocket and REST APIs.
2. **Plugin support**: existing Python integrations (core and HACS custom components) can run.
3. **As small as possible**: RAM, idle CPU, image size and startup time, with each measured, not guessed.

Goals 1 and 2 are hard requirements. Goal 3 is optimized around them.

## Status

| Roadmap step | Status | What works |
|---|---|---|
| 0. Frontend loads | Done | The official HA frontend logs in, renders the dashboard and controls entities live. Login uses the HA config dir's own `.storage/auth*`, so tokens work in both directions |
| 1. MQTT + discovery | Done | HA's MQTT config entry (TCP or TLS) and registries; lights (default and JSON schema), switches, sensors and binary sensors, discovered per component or per device |
| 2. Automations, scripts, templates | In progress | **Done:** HA's template helpers (`states()`, `float(0)`, `iif`…), checked against HA's own template tests, with live `render_template` and MQTT value templates; `configuration.yaml` with HA's tags (`!include`, `!secret`, `!include_dir_*`), the core config and time zone; scripts (`script.*` entities and services, modes, conditions, `choose`/`if`/`delay`…) and automations (state, MQTT and time triggers, `automation.*` entities and services) with HA's run traces. **Next:** the automation and script editors ([what's left](docs/architecture.md#whats-left-in-step-2)) |
| 3–8. History, `mobile_app`, config flows, Python plugins, more integrations, voice | Not started | See the [roadmap](docs/architecture.md#roadmap) |

Compatibility is checked, not assumed: CI replays sessions recorded from real HA
(`compat/fixtures/`) against lean-home and diffs every message. The HA, frontend and Mosquitto
versions it targets (HA 2026.11.2 today) are pinned in [`compat/versions.env`](compat/versions.env);
[docs/compatibility.md](docs/compatibility.md) says what depends on each and how to upgrade.

Footprint, under the [measurement rules](docs/architecture.md#measurement-rules) (single runs):
0.6 MiB anon memory idle with MQTT connected and 1.4 MiB with the frontend connected (step 1);
0.6 MiB idle with a real config loaded (step 2.3), 0.85 MiB after 20 script runs with their
traces stored (step 2.4, scripts), and a 7.3 MB stripped release binary with automations.

```sh
# config/ must already be onboarded by HA (lean-home has no onboarding yet); MQTT starts if HA
# has the MQTT integration set up there. hass_frontend/ comes from the home-assistant-frontend
# package at the HA_FRONTEND version in compat/versions.env. --demo adds three demo entities.
cargo run --release -- --config config --frontend hass_frontend --listen 0.0.0.0:8123
```

Background research, which motivates the design:

| | Anon memory (cgroup) |
|---|---|
| Stock HA Core 2026.1.1, demo + wemo + frontend | ~324 MiB (OOM-killed at a 256 MiB cap) |
| [yon/homeassistant-rs](https://github.com/yon/homeassistant-rs): Rust core + embedded Python, same config | ~245 MiB (also OOM-killed at 256 MiB) |
| The same Rust core with no Python | **1.3 MiB** |
| A Python plugin host running HA 2026.11.2's real code, 5 integrations (MQTT, Shelly, Hue, ESPHome, Met) | ~222 MiB |

The Rust core is nearly free. **The cost is in loading Python integrations through HA's own code**,
which imports the HTTP/WebSocket server and HA Cloud stacks even when nobody needs them. Details,
methodology and every raw number: [docs/benchmarks.md](docs/benchmarks.md).

## Plan in one paragraph

A single Rust process speaks HA's WebSocket/REST API and serves the official frontend. Common
protocols (MQTT discovery, ESPHome, Matter, Shelly, Hue, Z-Wave JS) are native Rust. Everything
else runs in a separate, lazily started Python plugin host that ships its own thin
`websocket_api`/`http`/`cloud` packages forwarding to Rust, so integrations get HA's APIs
without HA's server stack. Compatibility is proven by differential tests against real HA.
See [docs/architecture.md](docs/architecture.md).

## Repo layout

| Path | What |
|---|---|
| [crates/](crates/) | Rust workspace: the `lean-home` binary (HTTP/auth, WebSocket API, state machine) |
| [docs/architecture.md](docs/architecture.md) | Design, stack choices, roadmap |
| [docs/benchmarks.md](docs/benchmarks.md) | Methodology and results |
| [docs/landscape.md](docs/landscape.md) | Related projects (Moli OS, homeassistant-rs, HOMECORE) |
| [docs/frontend-rendering.md](docs/frontend-rendering.md) | Would Rust/WASM speed up heavy browser views? (G-code preview measurement) |
| [bench/](bench/) | Reproducible benchmark harness, HA patches, raw results |
| [.github/](.github/) | CI, PR title check, Renovate, release-plz |

## Contributing

All changes land on `main` through a squash-merged PR. The PR title becomes the commit, so it must
follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat: …`, `fix: …`, `docs: …`);
[release-plz](https://release-plz.dev/) uses those titles for the version bump and changelog. CI runs
`cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`.

## License

Apache-2.0, the same as Home Assistant, so HA-derived code can be ported with attribution. See [NOTICE](NOTICE).
