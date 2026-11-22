# Related projects

Checked 2026-11-22 (READMEs, code size, ADRs, commit history; not every project was built).

| | [yon/homeassistant-rs](https://github.com/yon/homeassistant-rs) | HOMECORE (in [ruvnet/RuView](https://github.com/ruvnet/RuView)) | [rudy-io/moli-os](https://github.com/rudy-io/moli-os) |
|---|---|---|---|
| Same goal as lean-home? | **Yes**: "drop-in replacement… existing Python integrations via embedded interpreter… identical APIs" | Half: HA API yes, HA plugins no | No: a clean-room alternative |
| Serves HA WebSocket/REST + official frontend | Yes (README ✅) | Planned (ADR-130, *Proposed*); `subscribe_entities` is P3 | No. Its `moli-homeassistant` crate is a "temporary migration bridge" (a client) |
| Python integrations | PyO3 **in-process**, vendors real `home-assistant/core` + ~5.7k-line shim. "Python Integration Entity Setup 🚧" | **No.** WASM-only plugins on Wasmtime (ADR-128, *Proposed*); migrated entries "do not install or execute Python HA integrations" | No. ~61 native Rust integrations |
| Size / activity | ~54k lines Rust, 30 commits, dormant since 2026-02 (pinned `cf38e88`) | ~94 `.rs` files; last API commit 2026-07 | ~422k lines Rust, active (checked at `7ed5ba7`) |
| Footprint | Measured by us: 1.3 MiB without Python, ~245 MiB with ([benchmarks](benchmarks.md)) | Not published (<50 MB target) | Self-reported: 4–7 MiB anon, 12–17 MiB RSS, 0.2–0.4% idle CPU, 12–30 MB image |
| License | **None** (all rights reserved): read, don't copy | MIT | Apache-2.0 |

## What we take from each

- **homeassistant-rs**: the closest design and the best reference for the shim layer, comparison
  tests and config-entry lifecycle. No license, so we write our own code. Issues we hit
  running it:
  - `/api/states` answered **without auth**
  - `ServicesWrapper.async_register()` rejects `description_placeholders`, so `zone` and `device_tracker` fail
  - a storage helper `TypeError: '_asyncio.Future' object is not callable`
- **Moli OS**: proof that a Rust home-automation core can live in single-digit MiB. Its native drivers
  are Apache-2.0 and can be borrowed with attribution. Also its **measurement discipline**: cgroup
  anonymous memory, idle CPU from the cgroup counter over 30 s with no clients connected, an image-size
  budget, and a performance log per deploy.
- **HOMECORE**: the WASM plugin route doesn't run existing Python integrations, and Wasmtime's JIT
  is heavy for small devices, so it doesn't meet lean-home's plugin requirement.

Nobody has both pieces yet: a lean core **and** HA plugin compatibility.
