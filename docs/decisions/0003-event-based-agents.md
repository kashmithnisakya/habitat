# 0003: Run agents as events, not long-running processes

**Status:** Proposed

## Context

Android and iOS can pause or stop apps, so an agent can't stay active all the time. Snapshotting a running WebAssembly instance across devices would also be complex.

## Decision

An agent wakes up, handles one event, saves its state to its home and exits. State is saved explicitly by the agent.

## Consequences

- **Good:** this fits phone background rules, and makes crash recovery and moving between devices simpler.
- **Bad:** there are no always-running agent loops.
- **Bad:** agent authors have to manage saving their own state, although the SDK should make that easy.

Revisit this if agents need long-running work.
