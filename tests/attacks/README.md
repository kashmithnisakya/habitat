# Attack tests

These tests act like a hostile agent and confirm that each attack fails. They run on every pull request, on Linux, macOS and Windows, as part of `cargo test --workspace`.

[`src/lib.rs`](src/lib.rs) has an `Arena`: a whole host, with the real supervisor, backend and broker, and only the network and the keystore swapped for fakes. It installs [a hostile agent](../../examples/hostile-agent/src/lib.rs) and plants things around it that it should never reach. **Nothing trusts what the agent says about itself.** A check looks at the host's files, the audit log and everything the agent could have seen.

[`tests/harness.rs`](tests/harness.rs) proves that the checks can fail, by running them against a host that does everything wrong.

## Adding an attack

1. If the hostile agent can't do it yet, add a command to [`examples/hostile-agent/src/lib.rs`](../../examples/hostile-agent/src/lib.rs). It should report what it got, so that anything it manages to get hold of ends up where the harness can see it.
2. Add a test in [`tests/`](tests), which follows this shape:

   ```rust
   #[test]
   fn an_agent_cant_read_outside_its_home() {
       let arena = Arena::new();
       let before = arena.snapshot();

       let report = arena.attack("read ../secret.txt");

       no_breach(arena.check_no_leak(&report));
       no_breach(arena.check_unchanged(&before));
       no_breach(arena.check_host_still_works());
   }
   ```

3. Check that it can fail. Break what it's testing on purpose, and see that the test notices.

Every security fix has to add a test here.

[`compile_cost.rs`](compile_cost.rs) exists already, and tests packages that try to make compiling them cost too much. It's built as a test of `habitat-backend-wasm`, so `cargo test --workspace` runs it. The hostile components are built by [`support/hostile_code.rs`](support/hostile_code.rs), which is shared with the tool that [measures their cost](../../docs/compile-costs.md).

| Attack | Expected result |
|---|---|
| Path traversal and symlink escape from the home | Denied |
| Windows device names, alternate data streams, short names and junctions | Denied, on every system, and tested in [`home.rs`](../../crates/core/src/home.rs) |
| Reading a secret | No API exists to do it, and no secret appears in agent-visible data |
| Unapproved network access | Denied and logged |
| Memory, CPU and disk exhaustion | Run stopped, and the host stays healthy |
| A package that's costly to compile | Refused, or abandoned after a time limit, and the host stays healthy |
| Prompt spam | Rate limited, and tested in [`broker.rs`](../../crates/broker/src/broker.rs) |
| Rewriting an approval prompt with terminal escapes | Neutralized and labelled, and tested in [`cli.rs`](../../crates/cli/tests/cli.rs) |
| Tampering with grants or logs | Not reachable from the agent, and log edits are detected |
| Tampering with run records, or filling them with hostile text | Not reachable from the agent, and its text is made safe and cut short, as tested in [`supervisor.rs`](../../crates/backend-wasm/tests/supervisor.rs) |

Every security fix should add a test here.
