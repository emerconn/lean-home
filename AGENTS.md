# lean-home

Home Assistant–compatible core in Rust for tiny hardware (1 GB ARM64). Design: `docs/architecture.md`; read it before structural changes. Roadmap and known gaps (checklists, kept up to date in every PR): `docs/roadmap.md` (step-2 write-ups in `docs/step-2.md`).

- HA frontend/Companion API compatibility is a hard requirement; memory/CPU/size come second and are measured (`bench/`), not guessed.
- Upstream versions we match (HA, frontend, Mosquitto…) are pinned only in `compat/versions.env`; `docs/compatibility.md` lists what depends on each and how to upgrade. Stay on HA `2026.11.2` until roadmap step 2.4 is done.
- Check before PR: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- No pushes to `main` and no stacked branches: always branch off `origin/main` after the previous PR merges (`git switch -c <new> origin/main`), and target `main`. Squash merge; PR title = commit, Conventional Commits (`feat:`, `fix:`, `docs:`, `ci:`, `chore:`).
- Don't edit `CHANGELOG.md` or crate versions; release-plz owns them.
- Start of session: check `git log`, open PRs, and `docs/roadmap.md`.

## Every PR: Docs and Gaps

Every PR must update the documentation it affects and record every gap it leaves in the same PR. Before opening a PR, check:
- Every new `shortcut:` comment has an item under "Gaps" in `docs/roadmap.md`, and closed gaps are removed.
- The roadmap step's checklist in `docs/roadmap.md` (tick what's done), `docs/step-2.md` for step 2, and the reference house list in `docs/roadmap.md` when a house feature lands.
- `docs/compatibility.md` for new files that follow HA source or hold data from a recording, plus upgrade steps.
- `README.md` status table and footprint line, and `compat/README.md` for scenario or tool changes.
- Measurements (binary size, memory) when code grows, or explicitly note they weren't taken.
