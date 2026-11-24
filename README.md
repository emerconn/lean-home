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

**Roadmap steps 0 and 1 work.** The official HA 2026.11.2 frontend logs in against lean-home,
renders the dashboard and controls entities live. Login uses the HA config dir's own
`.storage/auth*` files, so HA accepts tokens lean-home issues and the reverse. MQTT uses the HA
config dir's MQTT config entry and registries, and discovers lights (default and JSON schema),
switches, sensors and binary sensors. Replaying a recorded HA session gives the same states,
registry data and MQTT commands as HA.
Memory: 0.6 MiB anon idle with MQTT connected, 1.4 MiB with the frontend connected; a 6.3 MB
release binary (single run each, [measurement rules](docs/architecture.md#measurement-rules)).

```sh
# config/ must already be onboarded by HA (lean-home has no onboarding yet); MQTT starts if HA
# has the MQTT integration set up there. hass_frontend/ comes from the home-assistant-frontend
# 20261028.6 package. --demo adds three demo entities.
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
