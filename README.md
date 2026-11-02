# AgentHome

**A secure home for AI agents on computers and phones.**

AgentHome gives an agent its own persistent space to live in. The agent keeps its files, memory and tools there, but it can't touch your settings, personal data or other apps unless you allow it.

> [!WARNING]
> **Early development: nothing works yet, and this is not secure.** The repository currently holds the design and project skeleton. Do not use it to protect anything until the [attack tests](tests/attacks/README.md) exist and an outside security review has taken place.

## Why

Agent sandboxes are well served on servers and desktops, but they don't focus on phones. Agents that run on phones today often rely on broad device permissions rather than an isolated space. AgentHome aims to provide one security model and one agent package across both.

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
- **Same package everywhere:** one agent package runs on Mac and Android.

## Project status

| Stage | Status |
|---|---|
| Design | Drafted, and open to feedback |
| Android feasibility test | Not started |
| Mac core, broker and secrets | Not started |
| Attack tests | Not started |
| Android app | Not started |

Progress is tracked in the [roadmap](docs/roadmap.md) and [GitHub issues](https://github.com/kashmithnisakya/agenthome/issues).

## Documentation

- [Architecture](docs/architecture.md)
- [Threat model](docs/threat-model.md)
- [Permission model](docs/permissions.md)
- [Roadmap](docs/roadmap.md)
- [Design decisions](docs/decisions/README.md)
- [Agent API draft](wit/agent-home.wit)

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

You need a recent stable [Rust toolchain](https://rustup.rs).

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

## Contributing

Feedback on the design is the most useful contribution right now. See [CONTRIBUTING.md](CONTRIBUTING.md), and please report security problems as described in [SECURITY.md](SECURITY.md) instead of opening a public issue.

## License

[Apache License 2.0](LICENSE)
