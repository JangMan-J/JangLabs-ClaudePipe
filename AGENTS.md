# AGENTS.md — claude-pipe

Agent-facing guidance for this repo. (Claude Code also reads this file.)

## Agent skills

### Issue tracker

Issues and PRDs are tracked in **GitHub Issues** (`JangMan-J/JangLabs-ClaudePipe`) via the `gh` CLI; external PRs are **not** a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical triage roles, using the **default** label strings (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

**Single-context** layout: one root `CONTEXT.md` + `docs/adr/`. See `docs/agents/domain.md`.
