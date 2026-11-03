# Writing an agent

This builds a small agent in Rust, and runs it with Habitat. It's the code in [`examples/hello`](../examples/hello/src/lib.rs), which remembers who has visited.

Follow [getting started](../README.md#getting-started) first, so that you have the `habitat` program, `wasm-tools` and the WebAssembly target.

## How an agent works

An agent wakes up, handles one event and exits. Nothing in its memory carries over, so anything it needs next time has to be saved in its home. Its home is the only place it can reach, and there's no way for it to touch your other files.

An agent can't show you anything yet, because `send-message` needs the [broker](architecture.md), which doesn't exist yet. For now, you can see what an agent did by how its run ended, and by looking at the files in its home.

## The code

Make a library that builds for WebAssembly, with this in `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
habitat-sdk = { git = "https://github.com/kashmithnisakya/habitat" }
serde = { version = "1", features = ["derive"] }
```

Then the agent itself:

```rust
use habitat_sdk::{Agent, BoxError, Event, state};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Visits {
    count: u64,
    last_visitor: String,
}

struct Hello;

impl Agent for Hello {
    fn handle(event: Event) -> Result<(), BoxError> {
        let Event::UserMessage(name) = event else {
            return Err("this agent only handles messages".into());
        };
        if name.trim().is_empty() {
            return Err("tell me who's visiting".into());
        }

        // The agent starts from nothing on each run, so this is the only thing it remembers.
        let mut visits: Visits = state::load("visits")?.unwrap_or_default();
        visits.count += 1;
        visits.last_visitor = name;
        state::save("visits", &visits)?;
        Ok(())
    }
}

habitat_sdk::agent!(Hello);
```

- **`Agent`** is what an agent implements. Returning an error counts as the run failing, and its text is shown as the agent's own claim.
- **`state`** saves and loads values as JSON files in `state/` in its home.
- **`habitat_sdk::agent!`** makes this type the package's agent.

The library also has [`files`](../crates/sdk/src/files.rs) for plain files, plus the clock, random bytes and a log. For everything else, such as asking for network access, use `habitat_sdk::bindings`, which has the whole [agent API](agent-api.md).

## Saving safely

**Each save is all or nothing, but only for that one value.** If your agent is stopped or the host crashes, a value holds what it held before or what was saved, never a mix. Your agent can be stopped at any moment, when it runs out of time or when the host shuts down. So this can go wrong:

```rust
state::save("balance", &new_balance)?;
// If the agent is stopped here, the balance has changed and the history hasn't.
state::save("history", &new_history)?;
```

Keep things that have to match each other in one value:

```rust
state::save("account", &Account { balance: new_balance, history: new_history })?;
```

`state::load` returns an error if a value is there and can't be read, such as after you change its type. That stops an agent from starting over and overwriting it without meaning to, so your agent has to decide what to do about it.

## The manifest

Every agent has an `agent.toml`, which says what it's called and what it can use. A manifest can ask for less than the host allows, but not more. See [the manifest's rules](permissions.md#manifest).

```toml
[agent]
name = "hello"
version = "0.1.0"

[limits]
memory_mb = 64
run_seconds = 10
disk_mb = 1
```

## Build it and run it

From the root of this repository, build the agent and turn it into a component. Agents build for `wasm32-unknown-unknown`, because other targets import things like file and network access that Habitat refuses:

```bash
cargo build --release -p habitat-hello --target wasm32-unknown-unknown
```

```bash
wasm-tools component new target/wasm32-unknown-unknown/release/habitat_hello.wasm -o hello.wasm
```

Package and install it:

```bash
target/release/habitat pack examples/hello/agent.toml hello.wasm -o hello.habitat
```

```bash
target/release/habitat install hello.habitat
```

Run it twice. Each run is a new start:

```bash
target/release/habitat run hello "Ada"
```

```bash
target/release/habitat run hello "Grace"
```

Both print `completed`. To see that it remembered, look at the file it saved. On a Mac:

```bash
cat "$HOME/Library/Application Support/habitat/agents/hello/home/state/visits.json"
```

On Linux:

```bash
cat "$HOME/.local/share/habitat/agents/hello/home/state/visits.json"
```

It holds `{"count":2,"last_visitor":"Grace"}`.

A run that fails shows the agent's message, and leaves what it saved alone:

```bash
target/release/habitat run hello " "
```

This prints `error: failed, and the agent said: tell me who's visiting`, and exits with code 3.

When you're done, this deletes the agent and all its files, after asking you:

```bash
target/release/habitat remove hello
```
