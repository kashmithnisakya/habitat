# Architecture

This is a design document. None of it is implemented yet, and it will change as the [Android feasibility test](roadmap.md) and first implementation teach us more.

## Goals

The first version must:

- Run the same agent package on Mac and Android.
- Keep the agent's files and memory between runs.
- Block access to anything outside its home unless permission is granted.
- Keep API keys out of the agent's reach.
- Record every action and every approval or denial.

See the [threat model](threat-model.md) for what this does and doesn't protect against.

## Components

```text
┌──────────────────────────────────────────────┐
│ Shell: CLI (Mac) or Kotlin app (Android)     │
│ Shows approvals, activity log and controls   │
└──────────────────────┬───────────────────────┘
                       │ UniFFI on Android
┌──────────────────────▼───────────────────────┐
│ Rust core (trusted)                          │
│                                              │
│  Supervisor ── starts, limits and stops runs │
│  Broker ────── checks every request          │
│   ├─ Policy engine                           │
│   ├─ Secrets vault (OS keystore)             │
│   ├─ Model and network gateway               │
│   └─ Audit log                               │
│  Home store ── agent files and state         │
└──────────────────────┬───────────────────────┘
                       │ Only the agent-home API
┌──────────────────────▼───────────────────────┐
│ Backend: Wasmtime now, microVM later         │
│  ┌────────────────────────────────────────┐  │
│  │ Agent (untrusted WebAssembly component)│  │
│  └────────────────────────────────────────┘  │
└──────────────────────────────────────────────┘
```

| Component | Crate | Responsibility |
|---|---|---|
| Shell | `cli`, `android/` | Starts agents, shows approval prompts and displays the activity log |
| Supervisor | `core` | Manages each run's lifecycle and enforces limits |
| Home store | `core` | Stores agent packages, files and state |
| Broker | `broker` | Decides on and carries out every request that leaves the agent's home |
| Backend | `backend-wasm` | Runs the untrusted agent |
| SDK | `sdk` | Helps authors build agents against the agent API |

## Request flow

Example of a network request:

1. The agent asks to fetch `api.github.com`.
2. The broker checks the policy, then allows it, denies it or asks the user.
3. If allowed, the core makes the request and adds any needed API key itself.
4. The agent gets only the response, and the log records the action.

The agent never receives a socket, an API key or direct access to the host's file system.

## Agent API

Agents can import only the API in [`wit/agent-home.wit`](../wit/agent-home.wit). It deliberately leaves out raw WASI sockets and HTTP, so there is no route around the broker.

## Agent lifecycle

Agents are event-based. Each run follows the same steps:

1. An event arrives, such as a user message or a scheduled wake-up.
2. The supervisor starts the agent with its limits.
3. The agent handles the event and saves any state to its home.
4. The agent exits.

This fits phones, which can pause or stop apps at any time. If a run crashes or hits a limit, the supervisor stops it, logs what happened and keeps the last saved state. Writes must be atomic, so a crash can't leave half-written state.

## Storage

```text
agents/<agent-id>/
├── package/        # Read-only agent code and manifest
├── home/           # Agent's files
├── tmp/            # Files being written, which are moved into home/ when complete
├── grants.db       # Permissions, only the core can write to this
└── audit.db        # Tamper-evident log, only the core can write to this
```

Only `home/` is visible to the agent.

The audit log includes a hash of the previous entry in each new entry, so edits to earlier entries are detectable. This doesn't stop someone with full control of the device from replacing the whole log.

## Resource limits

- **CPU:** Wasmtime [epoch interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html) plus a time limit per run. Wasmtime documents around a 10% slowdown.
- **Memory:** a cap per run. Wasmtime's `ResourceLimiter` is the expected mechanism, to be confirmed during implementation.
- **Disk:** a cap on each agent's home, enforced by the home store.
- **AI models:** a token budget per run, enforced by the gateway.

## Platform support

| Platform | Approach | Limitation |
|---|---|---|
| Mac and Linux | Wasmtime | First targets for development |
| Android | Wasmtime | [Tier 3](https://docs.wasmtime.dev/stability-tiers.html) in Wasmtime, which it describes as not production ready |
| iPhone (later) | Wasmtime's [Pulley interpreter](https://docs.wasmtime.dev/examples-pulley.html) | Android's Tier 3 caveat also applies, and Pulley is roughly 10x slower than native code |
| Desktop microVM (later) | Second backend, possibly [libkrun](https://github.com/libkrun/libkrun) | The broker API has to work outside WebAssembly too |

## Biggest risks

| Risk | What to do |
|---|---|
| Wasmtime on Android is less tested | Run the feasibility test first, and add device tests to CI |
| A bug in the broker breaks everything | Keep it small, use safe Rust, fuzz its inputs and get an outside security review |
| Not enough agents can run on it | Ship useful example agents, and make Python an early follow-up |
| Too many approval prompts | Use narrow, reusable grants, and test with real people |
| Phones stop background work | Use the event-based design and Android's normal background APIs |

## What to revisit

- The event-based model, if agents need long-running work.
- The custom agent API, if WASI gains the permission controls this design needs.
