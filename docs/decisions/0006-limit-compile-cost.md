# 0006: Limit what compiling can cost by reading the code first

**Status:** Proposed

## Context

Loading a package compiles it, and that happens before any limit on a run applies. A package is untrusted, so it can be built to make compiling as expensive as possible.

[The measurements](../compile-costs.md) show that file size says little about this. A file under 4 KB can use 3 GB of memory and more, while a 127 MB file of data costs almost nothing. The 128 MB limit on a package's code therefore protects very little.

Wasmtime's compiler can't be stopped part way through, and it has no way to limit its own memory. Rust can't kill a thread, and when an allocation fails the process usually ends.

## Decision

The backend does three things, in this order, all on a thread of its own:

1. **Read the code before compiling it.** It reads the component with Wasmtime's own parser and validator, and refuses it with `LoadError::TooCostly` if it's over [limits on what makes compiling expensive](../compile-costs.md#the-limits). The check is quick compared with compiling, and gives the same answer on every device. This is what limits memory.
2. **Stop waiting after a time limit.** If compiling takes more than 120 seconds, `load` returns `LoadError::TooCostly`. The compile keeps running, because it can't be stopped.
3. **Compile one package at a time.** An abandoned compile holds on to its slot until it ends. A later `load` waits for that, and returns `LoadError::Busy` if it takes too long. However often a hostile package is loaded, it can only use one core and one compile's worth of memory.

A limit that doesn't fit one host can be changed with `WasmBackend::set_compile_limits`.

## Options we didn't choose

- **Only a lower limit on file size.** This can't work alone. The bombs that matter most are tens of kilobytes or less.
- **A separate process.** This is the only way to get a true limit on memory and to stop a compile at once. The process could be killed, and on Mac and Linux it could be given a memory limit. It needs a helper program or an Android service that doesn't exist yet, and the main process would have to load the compiled code with Wasmtime's `Component::deserialize`, which is `unsafe` and needs that code to be trusted. It fits well with [compiling once and saving the result](../feasibility-android.md), so it should be looked at again then.
- **Different Cranelift settings.** Turning off optimization cut the arithmetic case 18 times over, and the single-pass register allocator cut one loop case 24 times over, but each left the other cases as they were. They'd also change the code that every honest agent runs on, and we haven't measured by how much. See [the measurements](../compile-costs.md#cranelift-settings).
- **Counting memory with a custom allocator.** An allocator can't make a compile fail cleanly, only end the process. A library also shouldn't choose the allocator for the program that uses it.

## Consequences

- **Good:** the bombs we found are refused in milliseconds, before they cost anything.
- **Good:** the host stays in control of how long it waits, even for a package that gets past the check.
- **Good:** the answer doesn't depend on how fast the device is, except for the time limit.
- **Bad:** the limits are based on what we've measured, so a kind of bomb that we haven't thought of could still use a lot of memory. Nothing stops it once it's compiling.
- **Bad:** a package within the limits can still use about 2.2 GB, which could be too much for a low-end phone.
- **Bad:** the limits could refuse real code that's unusual. We've checked 21 real programs and left room, but that's a small sample.
- **Bad:** packages can't contain nested components at all, so agents composed from several components can't load. Their cost multiplies in a way that's hard to work out, so we'd rather refuse them until an agent needs this.
- **Bad:** the limits need measuring again after a Wasmtime upgrade.

Revisit this when compiling once and saving the result is built, or when real phone measurements are possible.
