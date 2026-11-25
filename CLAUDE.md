# lean-home

Home Assistant–compatible core in Rust for tiny hardware (1 GB ARM64). Design: `docs/architecture.md`; read it before structural changes.

- HA frontend/Companion API compatibility is a hard requirement; memory/CPU/size come second and are measured (`bench/`), not guessed.
- Upstream versions we match (HA, frontend, Mosquitto…) are pinned only in `compat/versions.env`; `docs/compatibility.md` lists what depends on each and how to upgrade.
- Check before PR: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- No pushes to `main`: branch, then PR. Squash merge; PR title = commit, Conventional Commits (`feat:`, `fix:`, `docs:`, `ci:`, `chore:`).
- Don't edit `CHANGELOG.md` or crate versions; release-plz owns them.
