# bench

Reproducible benchmark harness behind [docs/benchmarks.md](../docs/benchmarks.md).
Requires Linux with cgroup v2, a systemd user session, Docker, `uv`, `cargo`, `git`, `curl`.
Working files go to `$WORK` (default `bench/.work/`, gitignored).

| Script | What it does |
|---|---|
| `setup-hars.sh` | Builds [yon/homeassistant-rs](https://github.com/yon/homeassistant-rs) at the pinned commit (HA 2026.1.1) with and without Python, plus a venv with the needed integration libraries |
| `run.sh` | End-to-end: starts each target under a 1 CPU / `$MEM` cap and measures with `bench.sh` |
| `bench.sh` | Waits for HTTP, settles, prints anon/peak memory, idle CPU and OOM kills from the cgroup |
| `probe.py` | Loads one layer of HA's Python stack (bare / core / helpers / integrations) and idles |
| `run-probe.sh` | Runs the probe layers. `MODE=venv` uses HA 2026.1.1 from `setup-hars.sh`; `MODE=docker` uses the official image (`HA_VER`, default 2026.11.2) |
| `make-slim.sh` | Builds the slim 2026.11.2 overlay from the image + `patches/` (`LEVEL=a` or `b`) |
| `stub/sitecustomize.py` | Neutralizes hass_nabucasa's eager `litellm` import (HA ≤ 2026.10) |
| `patches/` | Lazy-import patches against HA 2026.11.2 (Apache-2.0, see NOTICE) |
| `results/` | Raw output of every run in the docs |

## Reproduce

```bash
bench/setup-hars.sh                                   # ~10 min: clone, venv, two release builds
docker pull ghcr.io/home-assistant/home-assistant:2026.1.1
docker pull ghcr.io/home-assistant/home-assistant:2026.11.2

bench/run.sh                                          # §1: hars-nopy, hars-py, ha-core @ 1 GiB
MEM=256M TARGETS="hars-py ha-core" bench/run.sh       # §1: the 256 MiB cap
bench/run-probe.sh; STUB=1 bench/run-probe.sh         # §2: plugin-host layers, 2026.1.1
TARGETS="ha-core ha-core-nollm" bench/run.sh          # §3: lazy LLM import end-to-end
MODE=docker bench/run-probe.sh                        # §4: layers on 2026.11.2
LEVEL=a bench/make-slim.sh && MODE=docker SLIM=1 bench/run-probe.sh   # §5 slim-A
bench/make-slim.sh && MODE=docker SLIM=1 bench/run-probe.sh           # §5 slim-B
TARGETS="ha-2611 ha-2611-slim" bench/run.sh           # §5 end-to-end
```

Ports used: 18123 (homeassistant-rs) and 18124 (HA containers, `--network host`), so an existing HA
on 8123 isn't disturbed. Probe containers are named `probe-d`, end-to-end ones `bench-ha`. Don't run
two scripts of the same kind at once.
