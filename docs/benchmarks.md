# Benchmarks

All numbers were measured 2026-11-22 on an x86_64 Linux dev PC (cgroup v2), never on target
hardware. Raw output is in [`bench/results/`](../bench/results); to reproduce, see [`bench/`](../bench).

## Methodology

- Each target runs in **its own cgroup** (a `systemd-run --scope` or a Docker container) capped at
  **1 CPU** and **1 GiB** unless noted.
- **Memory = cgroup `memory.stat` `anon`**: heap and stacks, no file cache. RSS and `docker stats`
  were not used because they swing with page cache. Peak = `memory.peak`.
- **Idle CPU** = the cgroup `cpu.stat` `usage_usec` delta over 30 s, after a 60 s settle, with no clients.
- **Ready** = time until the HTTP port answers.
- End-to-end workload: [homeassistant-rs](https://github.com/yon/homeassistant-rs)'s test config
  (`demo` + `wemo` config entries) + `frontend:`, identical for every target.
- Each table is 2 runs. Runs agreed within ~1–3%; the tables show representative values.

## 1. End-to-end: HA Core vs homeassistant-rs

HA 2026.1.1 (the version homeassistant-rs pins). [Raw](../bench/results/01-e2e-1G-ha2026.1.1-vs-hars.tsv)

| Target | Ready | Anon | Peak | Idle CPU | Entities |
|---|---|---|---|---|---|
| homeassistant-rs, no Python (Rust core only) | 1.0 s | **1.3 MiB** | 2.6 MiB | 0.00% | 6 states |
| homeassistant-rs + embedded Python (demo, wemo) | 8.0 s | **~245 MiB** | ~263 MiB | 0.09% | 76 states |
| HA Core 2026.1.1 (official image) | 6.5 s | **~324 MiB** | ~416 MiB | 0.02% | 63 registry entries |

- At a **256 MiB cap both Python stacks were OOM-killed**. [Raw](../bench/results/02-e2e-256M-cap.txt)
- Disk: homeassistant-rs ≈ 0.9 GB (19 MB binary, 697 MB venv of which 241 MB is the frontend,
  54 MB HA modules, 114 MB CPython) vs a 3.37 GB HA image.
- Entity counts aren't strictly equal (states vs registry entries); the workloads are comparable,
  not identical. homeassistant-rs failed to set up `device_tracker` and `image_processing` (shim gaps).
- For scale: one real HA 2026.11.2 install (14 config entries, 115 entities) used 484 MiB anon and
  0.25% CPU.

**Takeaway:** embedding Python in-process and running HA's real code saves only ~25%. The Rust core
is nearly free; the Python layer is the cost.

## 2. Plugin-host floor (HA 2026.1.1)

[`bench/probe.py`](../bench/probe.py) loads one layer of HA's Python stack and idles:
`bare` CPython → `core` (a live `HomeAssistant()` object) → `helpers` (the modules integrations import)
→ one integration (every module in its package; **import cost only, no devices**).
[Raw](../bench/results/03-plugin-host-layers-ha2026.1.1.txt)

| Layer | Stock 2026.1.1 | LLM import stubbed |
|---|---|---|
| bare CPython 3.13 | 9.9 | 9.9 |
| + core | 214.8 | **66.2** |
| + helpers | 216.2 | 67.6 |
| + Met | 216.7 | 68.1 |
| + Hue | 221.6 | 73.0 |
| + MQTT | 244.4 | 102.2 |
| + Shelly | 261.2 | 108.4 |
| + ESPHome | 278.6 | 130.4 |
| + all five | 289.9 | **147.7** |

## 3. Finding: an eager LLM import cost ~148 MiB (fixed upstream in 2026.11)

On 2026.1.1, constructing `HomeAssistant()` ran this import chain:

```
core.py:407 Config() → core_config.py:541 `from .components.zone import DEFAULT_RADIUS`
  → zone → helpers.collection → websocket_api → http → helpers.network
  → `from hass_nabucasa import remote` → hass_nabucasa/__init__ → .llm → litellm → openai
```

That's ~1,700 modules and ~148 MiB on every install, cloud or not. Neutralizing just the `litellm` import
([`bench/stub/sitecustomize.py`](../bench/stub/sitecustomize.py)) in the stock 2026.1.1 container
([raw](../bench/results/04-e2e-ha2026.1.1-lazy-llm.tsv)):

| | Anon | Peak | Ready |
|---|---|---|---|
| Stock 2026.1.1 | ~317 MiB | ~413 MiB | ~7.3 s |
| LLM import neutralized | **~233 MiB** | ~295 MiB | **~3.5 s** |

Same 63 entities, no errors. **HA 2026.11.2 already fixed both parts**: `DEFAULT_RADIUS` is no longer
imported through the `zone` package, and hass-nabucasa 2.8.1 no longer imports `litellm` on load.

## 4. Plugin-host layers on current HA (2026.11.2)

The same probe inside the official 2026.11.2 image (Python 3.14; HA now aliases `httpx`→`httpx2`
and `voluptuous`→`probatio` in `homeassistant/__init__.py`).
[Raw](../bench/results/05-plugin-host-layers-ha2026.11.2.txt)

| Layer | Anon MiB | Increase |
|---|---|---|
| bare CPython | 12.0 | |
| + core | 63.2 | +51 |
| + helpers | 112.5 | **+49** |
| + Met | 113.2 | +1 |
| + Hue | 119.8 | +7 |
| + MQTT | 157.6 | +45 |
| + Shelly | 194.6 | +82 |
| + ESPHome | 200.9 | +88 |
| + all five | **222.0** | |

The +49 MiB helpers jump is almost entirely `helpers.aiohttp_client` (+48.6), which nearly every
integration uses. It imports `components.zeroconf` → `helpers.network` → `hass_nabucasa` account/Cognito → **boto3**,
and also zeroconf's websocket module → `websocket_api` → `http`.

## 5. The slim plugin-host experiments

Behavior-preserving lazy imports applied to 2026.11.2 ([`bench/patches/`](../bench/patches)):

- **slim-A**: the `httpx2` alias and certifi SSL setup are installed by a one-shot import hook on
  the first `import httpx/httpcore/httpx2/httpcore2`, instead of at startup. Verified identical to
  stock after the trigger: `httpx is httpx2`, a single module copy, the original spec/loader, and the
  `voluptuous` shim intact.
- **slim-B**: slim-A + `from hass_nabucasa import remote` moved into the only function that uses it
  (`helpers/network.py`, already guarded by `"cloud" in hass.config.components`) + zeroconf's
  `websocket_api` import moved into `async_setup`.

[Raw A](../bench/results/06-plugin-host-layers-ha2026.11.2-slimA.txt) ·
[Raw B + end-to-end](../bench/results/07-plugin-host-slimB-and-e2e-ha2026.11.2.txt)

| Anon MiB | Stock | slim-A | slim-B |
|---|---|---|---|
| core | 63.2 | **52.7** | 52.7 |
| helpers (empty host) | 112.5 | 102.8 | **81.4** |
| + Met | 113.2 | 103.3 | 105.1 |
| + Hue | 119.8 | 110.0 | 110.2 |
| + MQTT | 157.6 | 157.5 | 157.5 |
| + Shelly | 194.6 | 184.8 | 184.8 |
| + ESPHome | 200.9 | 191.3 | 191.2 |
| **all five** | **222.0** | **222.1** | **222.1** |
| Full HA end-to-end | ~328 | — | ~331 (79 entities, 0 errors, same as stock) |

**Why integrations give it all back:** they import HA's entity-platform components (Met → `weather`,
Hue → `persistent_notification`), which register WebSocket commands at module level. That pulls in
`websocket_api` → `http` → HA Cloud → boto3 again. Patching HA import by import doesn't scale.
The design response is for the plugin host to provide its own thin `websocket_api`/`http`/`cloud`
packages that forward to the Rust core ([architecture](architecture.md)). **Next measurement.**

## Takeaways so far

1. A Rust core with the HA API costs single-digit MiB.
2. A Python plugin host running HA's real code costs ~105–110 MiB for one light integration and ~222 MiB for
   five on 2026.11.2. That's too much for a 1 GB router shared with other services.
3. Protocol integrations (MQTT, Shelly, ESPHome: +45–88 MiB each in Python) should be native Rust.
4. Most of the remaining Python cost is HA's server-side stacks being imported by integrations.
   That's what the thin replacement packages target.
