# Python agents

**Python agents don't run on Habitat today.** This records what stops them, from trying it for real, and what the choices are. It's for [#37](https://github.com/kashmithnisakya/habitat/issues/37).

## What was tried

An agent that sends a message and saves a file, built against [`wit/`](../wit/agent-home.wit) with `componentize-py` 0.25.1 on a Mac:

| | |
|---|---|
| Building it | Works, and took about a second |
| Its size | 18 MB, against the 128 MB limit on a package's code |
| Packing and installing it | Works |
| Running it | **Refused** |

```text
error: didn't start: the agent's code is too costly to compile: nested components: 1, over the limit of 0
```

This is the right answer under today's rules. Neither of the things below was changed to see how far it gets, and no library has been tested, because nothing runs yet.

## What stops it

**1. It's made of nested components.** The output is a component holding other components, for Python itself and its libraries. Habitat [refuses those](decisions/0006-limit-compile-cost.md), because the cost of compiling them can multiply at each level, in a way that's hard to work out in advance. A file under 4 KB used 3 GB this way.

**2. It imports WASI, including sockets.** Python's runtime expects to find an operating system underneath. As well as the agent API, this one imports 25 WASI interfaces:

| Group | Interfaces |
|---|---|
| Sockets | `tcp`, `tcp-create-socket`, `udp`, `udp-create-socket`, `network`, `instance-network`, `ip-name-lookup` |
| Files | `filesystem/types`, `filesystem/preopens` |
| Command line | `stdin`, `stdout`, `stderr`, `environment`, `exit`, and five `terminal-*` |
| Other | `random`, `io/streams`, `io/poll`, `io/error`, `clocks/wall-clock`, `clocks/monotonic-clock` |

[`AGENTS.md`](../AGENTS.md) says not to give agents WASI interfaces that bypass the broker, and Habitat refuses any component that imports them, so this would be refused even if the first problem went away.

## The choices

These need deciding, as they change the [security model](decisions/0002-broker-only-agent-api.md).

| Option | How it'd work | The catch |
|---|---|---|
| **Stubs in the host** | The host provides those interfaces, and each one refuses or does nothing. Sockets always fail, and files stay empty | More for the host to get right. Everything's checked against one list today, and "this WASI call is safe because it's a stub" is a harder thing to review |
| **Stubs inside the package** | Tools like `wasi-virt` can fill in those imports inside the component, so what reaches Habitat only imports the agent API | Keeps Habitat's rule exactly as it is, but it adds more nesting, so the limit on that still has to change. It also needs checking that it can stub all of these |
| **Allow some nesting** | Work out the real cost of a nested component, instead of refusing them all | Needed for either of those. It reopens what [decision 0006](decisions/0006-limit-compile-cost.md) closed, and a separate compile process would make it much safer |
| **A different Python** | Something that compiles Python without its usual runtime, or a Python interpreter built for `wasm32-unknown-unknown` against the agent API | Much less compatible with Python's libraries, and native extensions wouldn't work |

**The recommendation is stubs inside the package, with a separate process for compiling,** since that's the only one that leaves the rule about WASI exactly as it is. It hasn't been tried.

## What's still unknown

- **Whether any of this could run on a phone.** At 18 MB, compiling would take a while, and it's done on every run today. Compiling once and saving the result would be needed first.
- **Which libraries work.** Nothing can be tested until something runs. `componentize-py` is known to have limits on what can be imported at runtime, and on native extensions.
