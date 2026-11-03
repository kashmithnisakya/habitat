# 0005: Stream data by having agents pull it

**Status:** Proposed

## Context

Model answers should appear as they're produced, and large downloads shouldn't have to fit in an agent's memory all at once. A hostile agent also shouldn't be able to make the host hold large amounts of data for it.

WASI has stream types, and the component model has async support. Using WASI streams would add a `wasi:io` import to every agent, and [decision 0002](0002-broker-only-agent-api.md) keeps agents away from WASI. Async support is newer, and would make both sides more complicated.

## Decision

Streams are resources that agents read from one call at a time. `model-stream.next` returns the next piece of text, and `byte-stream.read` returns up to a size that the agent picks. Dropping a stream cancels the work behind it. File reads take an offset and a maximum size for the same reason.

See [Agent API](../agent-api.md) for the details.

## Consequences

- **Good:** the agent controls how much data it receives at a time, so it can't be flooded.
- **Good:** there are no WASI imports, callbacks or async code in the API.
- **Bad:** a call blocks until data arrives, so an agent can't wait for two things at once.
- **Bad:** Wasmtime's epoch interruption can't stop a call that's waiting inside the host, so the host has to apply the time limit itself.

Revisit this if agents need to run several requests at the same time, or when component model async is well established on all the targets Habitat supports.
