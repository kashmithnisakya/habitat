# Habitat

**A secure home for AI agents on computers and phones.**

Habitat gives an agent its own persistent space to live in. The agent keeps its files, memory and tools there, but it can't touch your settings, personal data or other apps unless you allow it.

> [!WARNING]
> **Early development: this is not secure yet.** You can install and run an agent on a Mac or Linux computer, but agents can't do much more than use their own files. Do not use it to protect anything until the [attack tests](tests/attacks/README.md) are finished and an outside security review has taken place.

## Why

Agent sandboxes are well served on servers and desktops, but they don't focus on phones. Agents that run on phones today often rely on broad device permissions rather than an isolated space. Habitat aims to provide one security model and one agent package across both.

## How it will work

The agent runs inside WebAssembly and starts with no outside access. Every request to reach the network, your files, an AI model or a secret goes through one trusted component: the **broker**.

```text
┌──────────────────────────────────────────────┐
│ Shell: CLI (Mac) or Kotlin app (Android)     │
│ Shows approvals, activity log and controls   │
└──────────────────────┬───────────────────────┘
                       │
┌──────────────────────▼───────────────────────┐
│ Rust core (trusted)                          │
│ Supervisor · Broker · Vault · Home store     │
└──────────────────────┬───────────────────────┘
                       │ Only the agent-home API
┌──────────────────────▼───────────────────────┐
│ Backend: Wasmtime now, microVM later         │
│   Agent (untrusted WebAssembly component)    │
└──────────────────────────────────────────────┘
```

Planned guarantees:

- **Nothing shared by default:** access outside the agent's home is denied unless granted.
- **Approvals the agent can't fake:** the trusted core draws every permission prompt.
- **Secrets stay outside:** API keys never enter the agent's space.
- **A full record:** every action, approval and denial goes into a tamper-evident log.
- **Same package everywhere:** one agent package runs on macOS, Windows, Linux and Android, and on iPhone and iPad if that [proves possible](docs/decisions/0007-supported-platforms.md).

## Project status

| Stage | Status |
|---|---|
| Design | Drafted, and open to feedback |
| Android feasibility test | Passing on an emulator, and not yet tested on a real phone |
| Mac core, broker and secrets | In progress: you can install and run an agent from the command line, and it can use files in its home, within time and memory limits. There's no broker yet, so it can't reach anything else |
| Attack tests | Not started |
| Windows | Not started: the code only builds on macOS and Linux today ([#56](https://github.com/kashmithnisakya/habitat/issues/56)) |
| Android app | Not started |
| iPhone and iPad | Not started: [we need to check that it's possible](https://github.com/kashmithnisakya/habitat/issues/40) |

Progress is tracked in the [roadmap issue](https://github.com/kashmithnisakya/habitat/issues/41). The next step is to run the Android feasibility test on a real phone. See the [draft feasibility report](docs/feasibility-android.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Threat model](docs/threat-model.md)
- [Permission model](docs/permissions.md)
- [Agent packages](docs/packages.md)
- [Roadmap](docs/roadmap.md)
- [Design decisions](docs/decisions/README.md)
- [Android feasibility report (draft)](docs/feasibility-android.md)
- [Agent API](docs/agent-api.md)

## Repository layout

```text
crates/
├── core/            # Supervisor and shared types
├── broker/          # Policy, vault, gateway and audit log
├── backend-wasm/    # Wasmtime backend
├── sdk/             # Rust library for agent authors
└── cli/             # Mac and Linux command line
wit/                 # The agent-home API
android/             # Kotlin app and UniFFI bindings
examples/            # Sample agents
tests/attacks/       # Attack tests
docs/                # Design documents
```

## Build

You need a recent stable [Rust toolchain](https://rustup.rs), and the WebAssembly target for building test agents:

```bash
rustup target add wasm32-unknown-unknown
```

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

## Getting started

This installs and runs [`file-agent`](examples/file-agent/src/lib.rs), a sample agent that does what each message tells it to with the files in its home. It only exists for testing, and a friendlier example is [on the way](https://github.com/kashmithnisakya/habitat/issues/13).

As well as the things listed under [Build](#build), you need `wasm-tools` to turn the agent into a WebAssembly component:

```bash
cargo install wasm-tools --locked
```

Build Habitat and the agent:

```bash
cargo build --release -p habitat-cli
```

```bash
cargo build --release -p habitat-file-agent --target wasm32-unknown-unknown
```

```bash
wasm-tools component new target/wasm32-unknown-unknown/release/habitat_file_agent.wasm -o file-agent.wasm
```

Put the agent and its [manifest](examples/file-agent/agent.toml) into a package, and install it:

```bash
target/release/habitat pack examples/file-agent/agent.toml file-agent.wasm -o file-agent.habitat
```

```bash
target/release/habitat install file-agent.habitat
```

Run it, and tell it to save a file:

```bash
target/release/habitat run file-agent "write hello.txt hi there"
```

Each run is a new start, and only the agent's files carry over. This one fails unless the file is still there:

```bash
target/release/habitat run file-agent "expect hello.txt hi there"
```

See what's installed, and how each agent's last run went:

```bash
target/release/habitat list
```

Delete the agent and all its files. This asks you to confirm first:

```bash
target/release/habitat remove file-agent
```

Agents are kept in `~/Library/Application Support/habitat` on a Mac, and `~/.local/share/habitat` on Linux. To use another folder, pass `--root` or set `HABITAT_HOME`.

`habitat --help` lists every command, and these exit codes:

| Code | Meaning |
|---|---|
| 0 | It worked |
| 1 | It failed, or was cancelled |
| 2 | The command line was wrong |
| 3 | The agent ran, and failed, crashed or hit a limit |
| 4 | The agent's code never started |
| 5 | The agent is already running |
| 130 | The agent was stopped by Ctrl-C or a request to stop |

## Contributing

Feedback on the design is the most useful contribution right now. See [CONTRIBUTING.md](CONTRIBUTING.md) for people and [AGENTS.md](AGENTS.md) for AI coding agents, and please report security problems as described in [SECURITY.md](SECURITY.md) instead of opening a public issue.

## License

[Apache License 2.0](LICENSE)
