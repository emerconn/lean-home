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

**Research and benchmarking.** No rewrite code yet. What we know so far:

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
| [docs/architecture.md](docs/architecture.md) | Design, stack choices, roadmap |
| [docs/benchmarks.md](docs/benchmarks.md) | Methodology and results |
| [docs/landscape.md](docs/landscape.md) | Related projects (Moli OS, homeassistant-rs, HOMECORE) |
| [bench/](bench/) | Reproducible benchmark harness, HA patches, raw results |

## License

Apache-2.0, the same as Home Assistant, so HA-derived code can be ported with attribution. See [NOTICE](NOTICE).
