# AGENTS.md

Guidance for AI coding agents working in this repository. [CONTRIBUTING.md](CONTRIBUTING.md) also applies.

## Project

Habitat is a secure home for AI agents on computers and phones. Agents run inside WebAssembly with no outside access, and every outside request goes through one trusted component, the broker.

**Status:** early development. The crates are mostly stubs. Don't describe a feature as working or secure unless it's implemented and tested.

Read these before making design changes:

- [Architecture](docs/architecture.md)
- [Threat model](docs/threat-model.md)
- [Permission model](docs/permissions.md)
- [Design decisions](docs/decisions/README.md)

## Layout

| Path | Purpose |
|---|---|
| `crates/core` | Supervisor, home store and shared types |
| `crates/broker` | Policy, vault, gateway and audit log |
| `crates/backend-wasm` | Wasmtime backend |
| `crates/sdk` | Rust library for agent authors |
| `crates/cli` | Command line |
| `crates/mobile` | What the phone app calls in the core, through UniFFI |
| `wit/` | The agent API |
| `app/` | Flutter app for Android and iPhone, with its Kotlin and Swift layers |
| `examples/` | Sample agents |
| `tests/attacks/` | Attack tests |

## Commands

The tests build a sample agent, so they need the WebAssembly target. Add it with `rustup target add wasm32-unknown-unknown`.

Run all of these before committing:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

To validate changes to the agent API:

```bash
wasm-tools component wit wit/
```

If you change the phone app, also run the checks in [app/README.md](app/README.md).

## Security rules

- Treat agents as untrusted, including their manifests, packages and everything they send to the host.
- Send everything that leaves an agent's home through the broker. Don't give agents raw sockets, raw HTTP, host paths or WASI interfaces that bypass it.
- Never expose raw secrets to an agent, and keep them out of logs and errors.
- Deny by default, including on invalid input, timeouts and errors.
- Avoid `unsafe` Rust unless there's no reasonable alternative, and explain why it's sound.
- Add an attack test in `tests/attacks/` for every security fix.
- Don't weaken a test or security control to make something pass.
- Report vulnerabilities through [SECURITY.md](SECURITY.md), not in public issues or pull requests.

## Working on issues

- Work is tracked in the [roadmap issue](https://github.com/kashmithnisakya/habitat/issues/41). Check an issue's dependencies before starting it.
- An issue is finished only when everything under "Done when" is met.
- Keep each change focused, and link its issue.
- Update the docs if the design changes, and add a decision record for major decisions.

## Commits

- **Never add an AI tool or agent as a co-author.** Don't add `Co-Authored-By` lines for Claude or any other AI tool.
- Write commit messages in the imperative mood, and explain why the change is needed.
