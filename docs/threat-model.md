# Threat model

This says what Habitat defends against, and where that's tested. **No outside review has happened yet**, so this is what we've claimed and tested ourselves. See [the review package](security-review.md) for what's in scope and what's known to be weak.

## What we protect

- The user's files, settings and personal data.
- API keys and other secrets.
- Other agents' homes.
- The device's availability, including CPU, memory, disk and battery.
- The integrity of the permission records and audit log.

## Who we assume is hostile

**The agent is untrusted.** That includes malicious agents and normal agents tricked by prompt injection through a web page, document or tool result.

The trusted parts are the Rust core, the shell and the operating system.

## Attacks in scope

| Attack | Defence |
|---|---|
| Reading files outside the agent's home | WebAssembly isolation, and a home store that refuses traversal, links and names that mean something else on another system. Tested in [`files.rs`](../tests/attacks/tests/files.rs) |
| Stealing API keys | Keys stay in the [OS keystore](../crates/broker/src/vault.rs) and are only used by the core. Nothing in the agent API can return one, and each can only be sent to hosts it's bound to. Their type can't be printed, so they can't reach a log or error by accident |
| Unapproved network access | No raw sockets, and every request goes through the broker, which checks each redirect, and only connects to public addresses. Tested in [`network.rs`](../tests/attacks/tests/network.rs) |
| Sending private data to an allowed destination | After a run reads a user's file, anywhere it wants to reach needs fresh approval. This reduces the risk but doesn't remove it, as explained below |
| Faking an approval prompt | The core supplies the wording, and the agent's reason is cleaned and labelled as its own claim. Tested in [`approvals.rs`](../tests/attacks/tests/approvals.rs) |
| Wearing the user down with prompts | 5 prompts per run, and something refused isn't asked about again in that run |
| Exhausting CPU, memory, disk or model budget | Limits per run and per agent, and on what the host accepts. Tested in [`resources.rs`](../tests/attacks/tests/resources.rs) |
| Making the compiler use up CPU or memory, with a package that's cheap to send | [Limits checked before compiling](compile-costs.md), a time limit, and one compile at a time. Memory isn't enforced once compiling has started |
| Changing grants or logs | Stored outside the agent's home, and the log is hash-chained, which `habitat log --verify` checks |
| Hiding what a run did by crashing or being stopped, or filling the disk with records | A run is recorded before it starts, and records are outside the agent's home. They hold a limited amount of the agent's text, and only the latest 1,000 are kept |
| Attacking the broker with malformed input | Safe Rust, strict validation and [fuzzing](../fuzz/README.md) |
| Hostile agent packages | A [strict package format](packages.md) with size limits and no paths taken from the archive, and packages contain only WebAssembly |

## Out of scope

- A device or operating system that's already compromised.
- Spectre-style attacks. Wasmtime calls this [ongoing research](https://docs.wasmtime.dev/security.html), so don't run mutually distrusting agents together on the assumption that they're fully separated.
- Bad choices an agent makes within permissions the user granted.
- Data given to an AI model provider. Hosted models are treated as an outside destination.
- Bugs in Wasmtime itself, though we should keep it updated and respond quickly to its advisories.

## Known weak spots

- Wasmtime's Android and iOS targets are [Tier 3](https://docs.wasmtime.dev/stability-tiers.html), which it describes as not production ready.
- The shell is trusted, and on phones it's [planned to include](decisions/0006-flutter-phone-app.md) the Flutter engine and every Dart package in the app. A bug or hostile package there could misreport the answer to a prompt.
- On Windows:
  - An agent's folder has an access list that only lets its owner in. Administrators can override that, and if Habitat runs elevated, Windows makes the Administrators group the owner of what it creates. Don't run Habitat elevated.
  - A power cut can lose the latest save, because a folder can't be synced there. It can't leave a mix of the old and new file, and being killed loses nothing.
  - Only Windows on x86-64 is tested. Windows on Arm is Tier 3 in Wasmtime.
  - Ctrl-C and closing the console aren't tested, because CI has no console to send them from.
- Approval prompts depend on people reading them.
- The data-leak rule only reduces the risk of private data being sent somewhere:
  - The user can approve the destination, because the prompt can't tell what's going to be sent.
  - It starts from the moment an agent is given a file, so it can't help with what's already in the agent's home. An agent can save private data in its home and send it in a later run, when the rule has started fresh.
  - It knows nothing about what's private in the agent's own files, or in what comes back from the network.
  - An agent can read a file, ask for approval for a destination it had a reason to reach, and hide what it read in an ordinary-looking request.
- The audit log can't detect full replacement by someone who controls the device.
