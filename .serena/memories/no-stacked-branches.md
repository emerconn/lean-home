Never base a branch (or a PR's base) on another feature branch. Merge the previous PR into `main` first, then branch the next piece of work off `main`.

**Why:** Stacked PRs (e.g., #24 based on #23's branch) are disallowed; branches must start from `main` after prior PRs merge.

**How to apply:** If new work depends on an open PR, finish or merge that PR first (ask the user to merge if needed), then `git switch -c <new> origin/main`. PRs always target `main`.
