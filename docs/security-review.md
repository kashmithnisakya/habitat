# Security review package

This is what an outside reviewer needs to start. **No outside review has happened yet.** Everything here is what we've claimed and tested ourselves, and Habitat shouldn't be called secure until one has, and its findings are dealt with.

## What Habitat claims

An agent is untrusted code, including when it's been tricked by something it read. It should only be able to:

- Use files in its own home, within a disk limit.
- Ask for what its manifest lists, and get it only if the user says yes.
- Use up to its limits on time, memory and tokens.

It should never be able to read another agent's files, read an API key, reach a host that wasn't approved, change its grants or its log, or make the device unusable.

See the [threat model](threat-model.md) for the full list of attacks, and what's out of scope.

## What to review

Most of the risk is in these, roughly in order. They're small on purpose.

| Code | Why it matters |
|---|---|
| [`crates/backend-wasm/src/host.rs`](../crates/backend-wasm/src/host.rs) | Every call from an agent arrives here. It's the edge of the sandbox |
| [`crates/backend-wasm/src/lib.rs`](../crates/backend-wasm/src/lib.rs) | What's linked in, what's refused, and how runs are limited and stopped |
| [`crates/backend-wasm/src/code_shape.rs`](../crates/backend-wasm/src/code_shape.rs), [`compile_limits.rs`](../crates/backend-wasm/src/compile_limits.rs) | Reads untrusted code before compiling it |
| [`crates/core/src/home.rs`](../crates/core/src/home.rs) | Paths from an agent, and every file operation in its home |
| [`crates/core/src/platform.rs`](../crates/core/src/platform.rs) | What each system does differently, and the only `unsafe` code outside of tests, for Windows |
| [`crates/core/src/package.rs`](../crates/core/src/package.rs), [`manifest.rs`](../crates/core/src/manifest.rs) | Untrusted packages and manifests |
| [`crates/broker/src/broker.rs`](../crates/broker/src/broker.rs), [`policy.rs`](../crates/broker/src/policy.rs) | Everything that leaves an agent's home is decided here |
| [`crates/broker/src/gateway.rs`](../crates/broker/src/gateway.rs) | URLs, redirects, name lookups and which addresses count as public |
| [`crates/broker/src/vault.rs`](../crates/broker/src/vault.rs), [`model.rs`](../crates/broker/src/model.rs) | API keys, and where they can go |
| [`crates/broker/src/grants.rs`](../crates/broker/src/grants.rs), [`approval.rs`](../crates/broker/src/approval.rs), [`audit.rs`](../crates/broker/src/audit.rs) | Permissions, prompts and the log |
| [`wit/agent-home.wit`](../wit/agent-home.wit) | The whole agent API |

**Not in scope yet:** the phone apps, which don't exist. Bugs in Wasmtime and Cranelift are out of scope for Habitat, but how Habitat configures them isn't.

## How to check our claims

```bash
cargo test --workspace
```

```bash
cargo test -p habitat-attacks
```

The attack tests are in [`tests/attacks/`](../tests/attacks/README.md), and [`tests/harness.rs`](../tests/attacks/tests/harness.rs) proves that their checks can fail. Fuzzing is in [`fuzz/`](../fuzz/README.md). All of this runs on every pull request, on Linux, macOS and Windows, apart from fuzzing, which runs on Linux.

As well as those, each change was checked by breaking its rules on purpose in a copy, one at a time, and seeing that a test noticed. That was done by hand, and the scripts aren't in this repository. Each pull request says what it found.

## Known weaknesses

These are all the ones we know about.

**Compiling**
- **Memory isn't enforced while compiling,** only limited by what's refused beforehand. A package within the limits has used about 2.2 GB, and a kind of bomb we haven't found could use more. Only a separate process would fix this.
- **An abandoned compile keeps running,** and holds up other loads until it ends.

**Running**
- **The time limit can't interrupt a host call.** A prompt, or a slow network read, can make a run go over.
- **The host's own memory isn't counted** towards an agent's limit, including compiled code and copies of data passed through the API.

**Files**
- **Saving isn't all or nothing across files.** An agent stopped between two writes keeps the first.
- **Other programs running as the same user can reach an agent's folder,** including its grants, log and package. These protect against agents, not against the user's other software.
- **On Windows:** administrators can override an agent's access list, a power cut can lose the latest save, and Ctrl-C isn't tested. Don't run it elevated.

**Network**
- **An approved host can be sent anything** an agent has, including what's in its home.
- **The data-leak rule only reduces that risk.** It starts when a file's read, doesn't know what's private in an agent's home, and starts fresh with each run. See the [threat model](threat-model.md#known-weak-spots).
- **Certificates come from a list built into Habitat,** not from the system.
- **The private address check runs before connecting,** and has only been tested up to that point, as nothing listens on port 443 in tests.

**Secrets**
- **Anything that can run as the user can ask the keystore for a key.**
- **A key is in the host's memory while it's being used,** and a compromised host could read it.
- **The model gateway has never been run against the real API.**

**Approvals**
- **People can approve the wrong thing.** A prompt can't tell what's going to be sent, and an agent's claim can be untrue.
- **The limit on prompts is per run,** so an agent can try again next time.

**The log**
- **Replacing the whole log and its position file isn't detected.**
- **It only covers what goes through the broker,** so an agent's use of its own files isn't logged.

**Platforms**
- **Nothing has run on a real phone,** or on a Windows machine other than a CI runner.
- **Wasmtime's Android and iPhone targets are Tier 3,** which it describes as not production ready, with no promise of security releases.
- **Limits on compiling were measured on one version of Wasmtime,** and need measuring again after an upgrade.

## Results so far

These are from CI at the time of writing, and CI's latest run is what counts.

| What | Where | Result |
|---|---|---|
| Attack tests | [`tests/attacks/tests/`](../tests/attacks/tests) | All pass on Linux, macOS and Windows |
| Attacks on compiling | [`tests/attacks/compile_cost.rs`](../tests/attacks/compile_cost.rs) | All pass, and on an Android emulator |
| Fuzzing | [`fuzz/`](../fuzz/README.md) | Short runs only. See the pull request that added it for what was found |
| An outside review | | **Hasn't happened** |

## What we'd like from a review

1. Can an agent reach anything that it shouldn't, by any route?
2. Can it get hold of a key, or get one sent somewhere else?
3. Can it make the host unusable, or crash it, within its limits?
4. Can it fool the person it's asking, or hide what it did?
5. Is the `unsafe` code in `platform.rs` sound?
6. Is anything we've said here not actually true?

Please report anything you find as [`SECURITY.md`](../SECURITY.md) describes, and not in a public issue.
