# Attack tests

These tests act like a hostile agent and confirm that each attack fails. Most are planned for [milestone M4](../../docs/roadmap.md).

[`compile_cost.rs`](compile_cost.rs) exists already, and tests packages that try to make compiling them cost too much. It's built as a test of `habitat-backend-wasm`, so `cargo test --workspace` runs it. The hostile components are built by [`support/hostile_code.rs`](support/hostile_code.rs), which is shared with the tool that [measures their cost](../../docs/compile-costs.md).

| Attack | Expected result |
|---|---|
| Path traversal and symlink escape from the home | Denied |
| Reading a secret | No API exists to do it, and no secret appears in agent-visible data |
| Unapproved network access | Denied and logged |
| Memory, CPU and disk exhaustion | Run stopped, and the host stays healthy |
| A package that's costly to compile | Refused, or abandoned after a time limit, and the host stays healthy |
| Prompt spam | Rate limited |
| Tampering with grants or logs | Not reachable from the agent, and log edits are detected |
| Tampering with run records, or filling them with hostile text | Not reachable from the agent, and its text is made safe and cut short, as tested in [`supervisor.rs`](../../crates/backend-wasm/tests/supervisor.rs) |

Every security fix should add a test here.
