# Attack tests

These tests will act like a hostile agent and confirm that each attack fails. They are planned for [milestone M4](../../docs/roadmap.md), and none exist yet.

| Attack | Expected result |
|---|---|
| Path traversal and symlink escape from the home | Denied |
| Reading a secret | No API exists to do it, and no secret appears in agent-visible data |
| Unapproved network access | Denied and logged |
| Memory, CPU and disk exhaustion | Run stopped, and the host stays healthy |
| Prompt spam | Rate limited |
| Tampering with grants or logs | Not reachable from the agent, and log edits are detected |

Every security fix should add a test here.
