# 0001: Use Wasmtime as the first runtime

**Status:** Proposed

## Context

Agents need strong isolation on both computers and phones. MicroVMs give strong isolation on desktops, but apps generally can't rely on them on phones. WebAssembly is the option most likely to work everywhere.

## Decision

Use [Wasmtime](https://github.com/bytecodealliance/wasmtime) behind a backend interface, so that a microVM backend can be added later.

## Consequences

- **Good:** it's active, Rust-based and has a clear permission model, and it includes an interpreter for platforms without JIT.
- **Bad:** Android and iOS are [Tier 3](https://docs.wasmtime.dev/stability-tiers.html), which it describes as not production ready, so we need our own device testing.
- **Bad:** existing agents like Claude Code expect a normal computer and won't run without major changes.

The Android feasibility test decides whether this record moves to **Accepted**.
