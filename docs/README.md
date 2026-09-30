# Frontend docs — Smart PC (current state)

Snapshot of the code as of 2026-09-29 (`main`, `svelte-check` clean).
Top-level planning docs in `/docs/` predate the code and partly contradict it;
these files describe **what the code actually does**.

| File                           | Contents                                                      |
| ------------------------------ | ------------------------------------------------------------- |
| `01-overview.md`               | What the app is, runtime modes, repo map, state snapshot      |
| `02-architecture.md`           | Layers, domains, data flow, Electron bridge, sidecar contract |
| `03-decisions.md`              | Architectural decisions observed in the code (ADR-style)      |
| `04-correctness-audit.md`      | Verified findings with severity and `file:line` refs          |
| `05-testing.md`                | Test inventory, how to run, gaps                              |
| `06-improvements.md`           | Proposed improvements, prioritized                            |
| `07-provider-availability.md`  | Availability watch + auto-start (implemented 2026-09-18)      |
| `08-sync-design.md`            | Supabase cloud: accounts + history mirror (implemented 09-29) |
| `09-voice.md`                  | Local voice input: mic → VAD → whisper → agent                |
| `10-pi-harness.md`             | The pi agent harness: spawn, tools, policy                    |
| `11-supabase-cloud-runbook.md` | Supabase setup, verification and day-to-day operations        |
