# Architecture

This is a design document. None of it is implemented yet, and it will change as the [Android feasibility test](roadmap.md) and first implementation teach us more.

## Goals

The first version must:

- Run the same agent package on Mac, Android and iPhone.
- Keep the agent's files and memory between runs.
- Block access to anything outside its home unless permission is granted.
- Keep API keys out of the agent's reach.
- Record every action and every approval or denial.

See the [threat model](threat-model.md) for what this does and doesn't protect against.

## Components

```text
┌──────────────────────────────────────────────┐
│ Shell: CLI (Mac) or Flutter app (phones)     │
│ Shows approvals, activity log and controls   │
└──────────────────────┬───────────────────────┘
                       │ UniFFI on phones
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
| Shell | `cli`, `app/` | Starts agents, shows approval prompts and displays the activity log |
| Supervisor | `core` | Manages each run's lifecycle and enforces limits |
| Home store | `core` | Stores agent packages, files and state |
| Broker | `broker` | Decides on and carries out every request that leaves the agent's home |
| Backend | `backend-wasm` | Runs the untrusted agent |
| SDK | `sdk` | Helps authors build agents against the agent API |

## Phone app

The plan is one [Flutter app](decisions/0006-flutter-phone-app.md) for Android and iPhone. The Flutter UI talks to a small layer of Kotlin or Swift, which calls the core through UniFFI. That layer also holds what only the phone can do: the OS keystore, background work and the file picker. The core still makes every decision, and the UI only shows what the core sends and returns the user's answer.

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

Each agent has one run at a time. A run holds a lock on `run.lock`, which the system releases when the process dies, so a killed run can't block the next one. An agent can't be removed during a run.

Each run gets an ID and is written to `runs.log` before it starts, and again when it ends. A run that started and never ended was interrupted by the host stopping. The log holds outcomes only, never what the user or the agent said. It stays a bounded size, and it's separate from the planned tamper-evident audit log.

## Backends

The core talks to a backend through two small traits in [`crates/core/src/backend.rs`](../crates/core/src/backend.rs), so that a microVM backend could be added later. `Backend::load` checks a package's code and prepares it. `LoadedAgent::run` runs it for one event, and reports how the run ended.

The Wasmtime backend links only the [agent API](agent-api.md), and refuses to load a component that imports anything else, such as WASI sockets. Each run starts with a new instance, so nothing in memory carries over, even after a crash. A crash is reported as the kind of trap only, because a full error can hold names chosen by the agent.

Today the code is compiled every time it's loaded. Compiling once and saving the result comes later, as the [feasibility report](feasibility-android.md) recommends.

## Storage

```text
agents/<agent-id>/
├── package/        # Read-only agent code and manifest
├── package.sha256  # Hash of the installed package, used to detect changes to it
├── home/           # Agent's files
├── tmp/            # Files being written, which are moved into home/ when complete
├── run.lock        # Held for as long as the agent is in use
├── runs.log        # Record of each run's outcome, only the core can write to this
├── grants.db       # Permissions, only the core can write to this
└── audit.db        # Tamper-evident log, only the core can write to this
```

Only `home/` is visible to the agent. See [Agent packages](packages.md) for the package format.

The audit log includes a hash of the previous entry in each new entry, so edits to earlier entries are detectable. This doesn't stop someone with full control of the device from replacing the whole log.

## Resource limits

Each limit comes from the agent's manifest, up to a maximum set by the host, so a manifest can't raise its own limits. A run that hits a limit is reported as a separate result, and later runs work as normal.

- **Time:** a limit per run, using Wasmtime's [epoch interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html). Wasmtime documents around a 10% slowdown. Running agents check the clock every 50 ms, and a timer thread only exists during a run, so an idle phone isn't woken up. This limits elapsed time, not CPU time, so time spent waiting counts too.
- **Memory:** a limit per run, using Wasmtime's `ResourceLimiter`. All of a run's memories and tables count towards one total, because Wasmtime's built-in limits apply to each memory separately. A run can also create at most 100 instances, 100 memories and 100 tables.
- **Disk:** a cap on each agent's home, enforced by the home store.
- **AI models:** a token budget per run, enforced by the gateway.

These limits have gaps:

- **Compiling isn't limited.** Loading a package compiles it before any of these limits apply, so a hostile package could use a lot of the host's CPU and memory at that point. The only protection today is the 128 MB limit on a package's code.
- **Host calls can't be interrupted.** Epoch interruption only stops WebAssembly code, so a call that blocks inside the host has to enforce its own time limit. Today's calls are short file operations, but the network and model gateways will have to do this.
- **The host's own overhead isn't counted.** The limit covers the agent's memories and tables, not compiled code, Wasmtime's bookkeeping or the host's temporary copies of data passed through the API.

## Platform support

| Platform | Approach | Limitation |
|---|---|---|
| Mac and Linux | Wasmtime | First targets for development |
| Android | Wasmtime | [Tier 3](https://docs.wasmtime.dev/stability-tiers.html) in Wasmtime, which it describes as not production ready |
| iPhone | Wasmtime's [Pulley interpreter](https://docs.wasmtime.dev/examples-pulley.html) | Android's Tier 3 caveat also applies, and Pulley is roughly 10x slower than native code |
| Desktop microVM (later) | Second backend, possibly [libkrun](https://github.com/libkrun/libkrun) | The broker API has to work outside WebAssembly too |

## Biggest risks

| Risk | What to do |
|---|---|
| Wasmtime on Android is less tested | Run the feasibility test first, and add device tests to CI |
| A bug in the broker breaks everything | Keep it small, use safe Rust, fuzz its inputs and get an outside security review |
| Not enough agents can run on it | Ship useful example agents, and make Python an early follow-up |
| Too many approval prompts | Use narrow, reusable grants, and test with real people |
| Phones stop background work | Use the event-based design and each phone's normal background APIs, and treat iPhone wake-ups as best effort |
| Wasmtime next to Flutter is untested | Test on both phones before building the app, and run agents in a separate process on Android if they interfere |
| The App Store may not allow agents that users install | Get an answer early, because [guideline 2.5.2](https://developer.apple.com/app-store/review/guidelines/) restricts downloaded code and the exception in 4.7 doesn't name WebAssembly |

## What to revisit

- The event-based model, if agents need long-running work.
- The custom agent API, if WASI gains the permission controls this design needs.
