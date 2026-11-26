Every PR must update the documentation it affects and record every gap it leaves, in the same PR.

**How to apply:** Before opening a PR, check:
- Every new `shortcut:` comment has an item under "Gaps" in `docs/roadmap.md`, and closed gaps are removed.
- The roadmap step's checklist (tick what's done) and, for step 2, its write-up in `docs/step-2.md`; the reference house list in `docs/roadmap.md` when a house feature lands.
- `docs/compatibility.md`: new files that follow HA source, or that hold data from a recording, plus the upgrade steps.
- `README.md` status table and footprint line, `compat/README.md` for scenario or tool changes.
- Measurements (binary size, memory) when code grows, or say they weren't taken.
