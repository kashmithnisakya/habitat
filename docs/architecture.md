# Architecture

This says how Habitat is built. The core, broker and command line exist and are tested on macOS, Linux and Windows. The phone apps don't exist yet, so what's here about them is still a plan, and it could change with what the [real phone test](roadmap.md) shows.

## Goals

The first version must:

- Run the same agent package on macOS, Windows, Linux and Android, and on iPhone and iPad if that [proves possible](decisions/0007-supported-platforms.md).
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

The [gateway](../crates/broker/src/gateway.rs) makes these requests, with rules of its own:

- **Only HTTPS, to the default port.** A URL has to be exactly `https://host[:443][/path][?query]`, with a host that follows the manifest's rules. Anything else is refused rather than corrected, and a proxy from the environment is never used.
- **Every hop is checked.** The gateway follows redirects itself, up to 5, and applies the policy to each one. A redirect to something that isn't allowed goes no further, and one to another host loses any headers that could be credentials.
- **Only public addresses.** A name is looked up once, and the connection goes to exactly what was checked, so it can't change its answer in between. If any of a name's addresses are private, link-local or otherwise reserved, none are used.
- **Limits.** A request body can be 16 MB, and a response body 64 MB, which ends in an error so that it can't be mistaken for the whole thing. Each hop stops at the end of the run's time, or after 2 minutes.
- **Headers.** An agent can't set the ones that decide where a request goes, such as `Host`. It never sees `Set-Cookie`, or anything else on a [short list](../crates/broker/src/gateway.rs) that could hold a credential.
- **Logging.** Each hop's host, method and status go in the [audit log](#storage). The path and query don't, as they can hold private data.

These have some limits:

- **The gateway uses `webpki-roots`, not the system's certificates.** It trusts a fixed list built into Habitat, so a certificate that an organisation has installed isn't trusted, and the list only changes when Habitat does.
- **A slow download can outlast the run's time limit a little.** Time is only checked between reads, and each is stopped by the same deadline.
- **An approved host can still be sent anything.** The data-leak rule ([#20](https://github.com/kashmithnisakya/habitat/issues/20)) only reduces this.

## Agent API

Agents can import only the API in [`wit/agent-home.wit`](../wit/agent-home.wit). It deliberately leaves out raw WASI sockets and HTTP, so there is no route around the broker.

## Agent lifecycle

Agents are event-based. Each run follows the same steps:

1. An event arrives, such as a user message or a scheduled wake-up.
2. The supervisor starts the agent with its limits.
3. The agent handles the event and saves any state to its home.
4. The agent exits.

This fits phones, which can pause or stop apps at any time. If a run crashes or hits a limit, the supervisor stops it, records what happened and keeps the last saved state. Each write is atomic, so a crash can't leave a half-written file.

The [supervisor](../crates/core/src/supervisor.rs) does this as follows:

- **One run at a time:** a run holds a lock on its agent, and a second run is refused straight away. The operating system holds the lock for the process, so it works across processes, and a killed host can't leave an agent locked. Different agents can run at the same time.
- **Records:** each run has an ID and a [record](../crates/core/src/run_log.rs), which is on disk before the agent starts. If it can't be saved, the agent doesn't start. A record holds the kind of event and how the run ended, but not the event's text, which can be private. Each agent keeps its latest 1,000 records.
- **Failures:** every kind of failure ends up in the record, including a package that won't load, and the next run starts as normal. A panic in the host is recorded too, as an internal error.
- **Shutting down:** `Supervisor::shut_down` tells running agents to stop, refuses new runs and waits for a time the host chooses. A run that's compiling can't be interrupted, so it doesn't start the agent once it's finished, and the host can exit without waiting for it.
- **Being killed:** a killed host leaves a record with no ending, and the supervisor marks it as interrupted once the agent's lock is free.

These have some limits:

- **Saving isn't all or nothing.** "Saved state" means every write that finished. An agent that's stopped between writing two related files keeps the first, so the agent has to write in an order it can recover from. There's no way yet to save several files at once.
- **Nothing retries a run.** A run that fails or is refused stays that way, and it's up to the shell to ask again. A run does wait up to half a second for the agent's lock first, so something that only holds it for a moment, such as `habitat list` reading its records, doesn't get in the way.
- **The shell has to call `shut_down`.** The CLI does this on Ctrl-C and `SIGTERM`, and gives a compile 5 seconds before exiting without it. Nothing handles Android's lifecycle events yet. Without that, stopping the host counts as being killed, which is safe but gives the agent no chance to stop first.
- **Records aren't the audit log.** They say that a run happened and how it ended. They aren't protected against tampering by someone with access to the device, and old ones get removed. The audit log can use the run ID to link its entries to a run.
- **The lock needs a local file system.** File locks may not work on a network drive, and Habitat doesn't check for that.

## Backends

The core talks to a backend through two small traits in [`crates/core/src/backend.rs`](../crates/core/src/backend.rs), so that a microVM backend could be added later. `Backend::load` checks a package's code and prepares it. `LoadedAgent::run` runs it for one event, and reports how the run ended.

The Wasmtime backend links only the [agent API](agent-api.md), and refuses to load a component that imports anything else, such as WASI sockets. It also refuses one that would [cost too much to compile](#resource-limits). Each run starts with a new instance, so nothing in memory carries over, even after a crash. A crash is reported as the kind of trap only, because a full error can hold names chosen by the agent.

Today the code is compiled every time it's loaded. Compiling once and saving the result comes later, as the [feasibility report](feasibility-android.md) recommends.

## Storage

```text
agents/<agent-id>/
├── package/        # Read-only agent code and manifest
├── package.sha256  # Hash of the installed package, used to detect changes to it
├── home/           # Agent's files
├── tmp/            # Files being written, which are moved into home/ when complete
├── runs/           # A record of each recent run, only the core can write to this
├── grants.db       # Permissions, only the core can write to this
└── audit.db        # Tamper-evident log, only the core can write to this
```

Only `home/` is visible to the agent. See [Agent packages](packages.md) for the package format.

Next to `agents/`, `locks/<agent-id>.lock` is locked for as long as an agent is running. It's outside the agent's folder because Windows can't move a folder with an open file in it, and it stays after an agent is removed, so that the lock always means the same thing.

Removing an agent deletes this whole folder, and is refused while the agent is running. The folder is first moved to a `removed/` folder next to `agents/`, so the agent is gone in one step. If the host dies part way through, a later install under the same name can't inherit what's left of the old agent's home.

The [audit log](../crates/broker/src/audit.rs) records every request that leaves an agent's home, each decision, and how each run ended, along with the run's ID. Each entry includes a hash of the previous one, so editing, removing or reordering entries is detectable, and `audit.head` holds the latest position, so that removing entries from the end is too. `habitat log <agent>` shows it and `habitat log <agent> --verify` checks it.

- **If it can't be logged, it doesn't happen.** A request is refused if the log can't be written or has been changed.
- **Nothing the agent wrote goes in the log.** Its reasons aren't stored, and no field can hold headers or bodies, which keeps API keys out. A malformed target is made safe and cut short.
- **Each run can add 10,000 entries.** After that, requests are refused, so an agent can't fill the disk.

This doesn't stop someone with full control of the device from replacing the whole log and `audit.head`. It's also never shortened, so it grows for as long as the agent stays installed.

## Resource limits

Each limit comes from the agent's manifest, up to a maximum set by the host, so a manifest can't raise its own limits. A run that hits a limit is reported as a separate result, and later runs work as normal.

- **Time:** a limit per run, using Wasmtime's [epoch interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html). Wasmtime documents around a 10% slowdown. Running agents check the clock every 50 ms, and a timer thread only exists during a run, so an idle phone isn't woken up. This limits elapsed time, not CPU time, so time spent waiting counts too.
- **Memory:** a limit per run, using Wasmtime's `ResourceLimiter`. All of a run's memories and tables count towards one total, because Wasmtime's built-in limits apply to each memory separately. A run can also create at most 100 instances, 100 memories and 100 tables.
- **Disk:** a cap on each agent's home, enforced by the home store.
- **AI models:** a token budget per run, enforced by the gateway.
- **Compiling:** loading a package compiles it, before any of the above apply. File size says little about what that costs, as a file under 4 KB can use 3 GB of memory and more. So the backend reads the code first, and refuses it with `LoadError::TooCostly` if it's over [limits on what makes compiling expensive](compile-costs.md#the-limits), such as nested components, or locals times branches. It then compiles on a thread of its own, and stops waiting after 120 seconds. The compiler can't be stopped, so only one package compiles at a time, and a later load waits or returns `LoadError::Busy`. See [decision 0006](decisions/0006-limit-compile-cost.md).

These limits have gaps:

- **Memory isn't enforced while compiling.** The limits on code are what keeps memory down, and nothing stops a compile that uses more than expected. The most we've measured within the limits is about 2.2 GB on a Mac and 1.3 GB on an emulator, which could get the app stopped on a low-end phone. A kind of bomb that we haven't found could use more. Only compiling in a separate process would fix this.
- **An abandoned compile keeps running.** After the time limit, it keeps using one core until it ends, and other packages have to wait for it. Within the limits, a compile takes up to about 95 seconds on a Mac, and could take several times that on a phone.
- **The time limit depends on the device.** Everything else gives the same answer on every device, but a large package could compile in time on a Mac and not on a slow phone.
- **None of this has been measured on a real phone.** [The figures](compile-costs.md) are from a Mac and an Android emulator, and the limits were chosen from those and from 21 real programs.
- **The limits are tied to this version of Wasmtime.** They need measuring again after an upgrade.
- **Host calls can't be interrupted.** Epoch interruption only stops WebAssembly code, so a call that blocks inside the host has to enforce its own time limit. Today's calls are short file operations, but the network and model gateways will have to do this.
- **The host's own overhead isn't counted.** The limit covers the agent's memories and tables, not compiled code, Wasmtime's bookkeeping or the host's temporary copies of data passed through the API.

## Platform support

| Platform | Approach | Limitation |
|---|---|---|
| macOS and Linux | Wasmtime | First targets for development. Apple silicon and Arm Linux are Tier 2 in Wasmtime |
| Windows | Wasmtime | Tested on x86-64, which is Tier 1 in Wasmtime. Windows on Arm is Tier 3. Its safety measures are in [`platform.rs`](../crates/core/src/platform.rs), and what's weaker is in the [threat model](threat-model.md#known-weak-spots) |
| Android | Wasmtime | [Tier 3](https://docs.wasmtime.dev/stability-tiers.html) in Wasmtime, which it describes as not production ready |
| iPhone and iPad (later) | Wasmtime's [Pulley interpreter](https://docs.wasmtime.dev/examples-pulley.html) | Android's Tier 3 caveat also applies, and Pulley is roughly 10x slower than native code |
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
