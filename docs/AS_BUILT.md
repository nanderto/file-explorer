# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**Current milestone:** pre-M0 (no code yet — repo contains plan, requirements, and tooling).

## Components

### Workspace / build tooling
- Quality gate (`scripts/gate.sh`), Claude Code hooks (`.claude/settings.json`),
  CI (`.github/workflows/ci.yml`), branch protection script
  (`scripts/setup-branch-protection.sh`). No cargo workspace yet.

### fs-core
- Not started.

### theme
- Not started.

### app (GPUI)
- Not started.

## Deviations from the plan

- None yet.

## Change log

| Date | PR | Change |
|---|---|---|
| 2026-08-22 | — | Repo bootstrapped: plan, CLAUDE.md, quality gate, hooks, CI, this file. |
