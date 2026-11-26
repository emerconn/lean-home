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

The full checklist, and what a real house still needs (the maintainer's Home Assistant is the
reference), is in [docs/roadmap.md](docs/roadmap.md).

- [x] **0. Frontend loads.** The official HA frontend logs in, renders the dashboard and controls
  entities live. Login uses the HA config dir's own `.storage/auth*`, so tokens work in both
  directions.
- [x] **1. MQTT + discovery.** HA's MQTT config entry (TCP or TLS) and registries; lights (default
  and JSON schema), switches, sensors and binary sensors, discovered per component or per device.
- [ ] **2. Automations, scripts, templates.** Done: HA's template helpers, checked against HA's own
  template tests; `configuration.yaml` with HA's tags and packages, the core config and time zone;
  scripts and automations (state, MQTT and time triggers) with HA's run traces, on/off kept across
  restarts; the automation and script editors (open, save, delete, with reload). Next: more
  triggers, conditions and actions, then blueprints.
- [ ] **3. The house's basics:** storage dashboards, helpers, template, `command_line` and `rest`
  entities.
- [ ] **4–9.** History, Companion apps, config flows, the Python plugin host, Matter and cameras
  and other native integrations, voice.

## Try it in Docker

A broker, a simulated MQTT device (two lights, a fan, a temperature sensor, a door) and lean-home,
in Docker Compose. Needs Docker with Compose, curl and jq:

```sh
try/setup.sh             # once: onboards try/config with stock HA (asks for a password)
docker compose -f try/compose.yaml up -d --build
```

Open http://localhost:8123 and log in as `dev` (or the username you passed to `setup.sh`). The
device shows up through MQTT discovery, with three example automations and a script from the
compat scenario:

- Turn the fan on: "Fan lights ceiling" turns the ceiling light on (while the door is closed).
- Press the button, which runs `script.blink` on the desk light:
  `docker compose -f try/compose.yaml exec mosquitto mosquitto_pub -t lh/button -m press`
- Open the door: `docker compose -f try/compose.yaml exec mosquitto mosquitto_pub -r -t lh/door/state -m ON`
- Edit, save and run automations and scripts in Settings, and read their traces.

Not there yet: history, every integration but MQTT, translations (the UI shows raw states such as
`on`). `try/setup.sh reset` stops it and deletes `try/config`.

The image on its own: `docker build --build-arg HA_FRONTEND=<from compat/versions.env> -t
lean-home .`, then `docker run -v /path/to/config:/config -p 8123:8123 lean-home`. The config dir
must be one HA has onboarded (lean-home has no onboarding yet). With a copy of a real HA config
dir, only its MQTT devices, automations and scripts come up today. Use a **copy**: lean-home
writes to it (`.storage`, the editors' YAML files).

Compatibility is checked, not assumed: CI replays sessions recorded from real HA
(`compat/fixtures/`) against lean-home and diffs every message. The HA, frontend and Mosquitto
versions it targets (HA 2026.11.2 today) are pinned in [`compat/versions.env`](compat/versions.env);
[docs/compatibility.md](docs/compatibility.md) says what depends on each and how to upgrade.

Footprint, under the [measurement rules](docs/architecture.md#measurement-rules) (single runs):
0.6 MiB anon memory idle with MQTT connected and 1.4 MiB with the frontend connected (step 1);
0.6 MiB idle with a real config loaded (step 2.3), 0.85 MiB after 20 script runs with their
traces stored (step 2.4, scripts), and a 7.6 MB stripped release binary with automations, the editors and packages. The Docker
image holds 186 MB, 177 MB of it the frontend (trimming it is on the roadmap).

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
| [docs/roadmap.md](docs/roadmap.md) | Roadmap, the reference house, known gaps (checklists) |
| [docs/architecture.md](docs/architecture.md) | Design and stack choices |
| [docs/step-2.md](docs/step-2.md) | What step 2 built and how it was checked |
| [Dockerfile](Dockerfile), [try/](try/) | The image, and a Compose setup to try it |
| [docs/benchmarks.md](docs/benchmarks.md) | Methodology and results |
| [docs/landscape.md](docs/landscape.md) | Related projects (Moli OS, homeassistant-rs, HOMECORE) |
| [docs/frontend-rendering.md](docs/frontend-rendering.md) | Would Rust/WASM speed up heavy browser views? (G-code preview measurement) |
| [bench/](bench/) | Reproducible benchmark harness, HA patches, raw results |
| [compat/](compat/) | Recordings of real HA, the replay against lean-home, the upstream version pins |
| [.github/](.github/) | CI, PR title check, Renovate, release-plz |

## Contributing

All changes land on `main` through a squash-merged PR. The PR title becomes the commit, so it must
follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat: …`, `fix: …`, `docs: …`);
[release-plz](https://release-plz.dev/) uses those titles for the version bump and changelog. CI runs
`cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`.

## License

Apache-2.0, the same as Home Assistant, so HA-derived code can be ported with attribution. See [NOTICE](NOTICE).
