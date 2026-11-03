# Agent API

[`wit/agent-home.wit`](../wit/agent-home.wit) defines everything an agent can do. It's the only thing an agent can import, so it's also the edge of the sandbox. See [decision 0002](decisions/0002-broker-only-agent-api.md) for why it's a custom API.

The bindings for both sides are generated from this file. CI builds a [test agent](../examples/api-check/src/lib.rs) that uses every call, and checks what it imports.

The file calls, `now-ms`, `random-bytes`, `request-access` and `http-fetch` work. Everything else that needs the broker returns `denied` until it's built, and that includes `send-message`. `log` currently drops its messages. The version rule below has only been checked by hand, on Wasmtime 50.

## What agents can do

| Calls | What they do | Checked by |
|---|---|---|
| `read-file`, `write-file`, `remove-file`, `list-dir` | Use files in the agent's home | Home store, within the disk limit |
| `now-ms`, `random-bytes` | Get the time and random bytes | Nothing, because they reach nothing outside |
| `send-message` | Show the user a message | Rate limit |
| `log` | Add to the agent's own log | The host can drop messages |
| `model-complete`, `http-fetch` | Use an AI model or the network | Broker |
| `request-access` | Ask for a network host or model | Broker, which draws the prompt |
| `pick-user-file` | Ask the user to choose a file | Broker, using the system file picker |

Agents get a clock and random bytes from this API because they have no WASI. Rust's `wasm32-wasip2` target imports WASI, so Rust agents are built for `wasm32-unknown-unknown` instead.

What's deliberately missing:

- **Raw sockets and raw HTTP clients:** `http-fetch` is a request to the broker, which checks the destination and makes the request itself. An agent never holds a connection.
- **Host paths:** home paths are relative to the home, and agents never see or name a path outside it. A user file is a handle, and its name has no folder.
- **Secrets:** no call returns one. The core adds keys to the requests it makes.

A test lists every call, so adding one means changing that test as well.

## Errors

Each call returns a `home-error`, so an agent can decide what to do next:

| Error | Meaning | What an agent can do |
|---|---|---|
| `not-granted` | It needs approval | Call `request-access`, then try again |
| `denied` | It was refused | Asking again won't help |
| `not-found` | The file doesn't exist | |
| `invalid-request` | The request is malformed | Fix the request |
| `limit-exceeded` | A limit was reached | Use less, or stop |
| `timed-out` | It took too long | Try again if there's time left |
| `unavailable` | An outside service failed | Try again later |
| `internal` | Something went wrong in the host | Stop |

Messages come from the host, and mustn't include host paths or secrets.

`handle-event` returns a plain string on failure, not a `home-error`. That text comes from the agent, so an agent can't pass off a failure as a decision by the host.

## Large data

A hostile agent shouldn't be able to make the host use a lot of memory, and a normal agent shouldn't crash on a large response. Each call that returns a lot of data takes a size from the agent, and the host can return less:

- `read-file` and `user-file.read` take an offset and a maximum, so a large file can be read in pieces.
- `http-response.body` is a `byte-stream`, which is also read in pieces of a chosen size. Dropping it cancels the download.

The host caps each read and can return less than the agent asked for, so an agent has to keep reading until there's nothing left. `read-file` returns at most 1 MB per call, `list-dir` fails with `limit-exceeded` above 10,000 entries, and `random-bytes` fails above 1 KB. The other maximums will be chosen when those calls are implemented.

`write-file` still replaces a whole file in one call, because that's what makes writes atomic. That means a file can't be larger than what the agent can hold in memory.

## Streaming

Model answers use **a stream the agent pulls from**. `model-complete` returns a `model-stream`, and the agent calls `next` until there's no more text:

```text
let answer = model-complete(request)
while let Some(text) = answer.next() {
    send-message(text)
}
answer.usage()
```

- **Why pull:** the agent can't be flooded, because nothing arrives until it asks. It fits event-based agents and needs no callbacks.
- **Why not WASI streams:** they would bring in `wasi:io`, and agents should import nothing from WASI.
- **Why not component model async:** it isn't needed for this, and it's a newer part of Wasmtime. It can be looked at again in a later API version.
- **Only one way to call a model:** there's no separate call for a complete answer, because that would be a second path for the broker to check. The SDK can offer one on top of the stream.
- **Budgets:** the token budget still applies. `next` returns `limit-exceeded` when it runs out.

The host's calls can wait on the network or on the user. Wasmtime's epoch interruption only stops WebAssembly code, so the host has to apply the run's time limit to these calls itself, and return `timed-out`.

## Versions

The API version is the `habitat:host@x.y.z` line in the WIT file. Wasmtime treats two versions as compatible if they match up to the first non-zero number, so `0.1.0` and `0.1.3` are compatible, and `0.1.0` and `0.2.0` aren't.

| Change | Examples | Version change |
|---|---|---|
| Adds something new | A new function or resource method | Patch while below 1.0, such as `0.1.0` to `0.1.1`. Minor from 1.0 on |
| Changes anything existing | Changing a function's parameters, or adding a field to a record | Minor while below 1.0, such as `0.1.1` to `0.2.0`. Major from 1.0 on |
| Adds a case to a variant or enum | A new event or error | Treated as a breaking change, because older agents can't read the new case |

Comments and renamed parameters don't need a new version unless the generated code changes.

- **Older agents keep working:** the host supports the latest version on each compatible track, and agents built for an older version on that track still load.
- **Newer agents fail safely:** an agent built for a newer API than the host fails to load, with an error. It never runs with missing functions.
- **Before 1.0:** the API can still change, and old versions may be dropped without notice. From 1.0 on, the host keeps supporting each major version until a documented end date.

Nothing has been released yet, so `0.1.0` can still change in place until the first release.

## What's left out for now

- **Model tool calls:** a request only holds messages. Each provider handles tools differently, so this waits until the model gateway ([#22](https://github.com/kashmithnisakya/habitat/issues/22)) shows what's needed.
- **Agent-chosen credentials for `http-fetch`:** agents can't ask the broker to add a key to a request yet. This waits for the secrets vault ([#21](https://github.com/kashmithnisakya/habitat/issues/21)).
- **Larger writes:** there's no call to append to a file or write in pieces.
- **Remembered user files:** a file the user picks lasts for one run, so it can't be covered by an until-revoked grant.
- **Python agents:** `componentize-py` builds on WASI, so Python agents may need a very limited set of WASI interfaces that are stubbed out by the host. That's a question for [#37](https://github.com/kashmithnisakya/habitat/issues/37).
