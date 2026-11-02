# 0002: Give agents a custom API, not raw WASI access

**Status:** Proposed

## Context

WASI offers interfaces for sockets, HTTP and files. Exposing them directly would leave several paths to check, and some of them could bypass our permissions.

## Decision

Agents import only the [`agent-home`](../../wit/agent-home.wit) API. Everything that leaves the agent's home goes through one broker.

## Consequences

- **Good:** there's one place to enforce policy and write logs, which makes it easier to review and test.
- **Good:** the same API can serve a future microVM backend.
- **Bad:** agents must be built for this API, so existing WASI programs won't run unchanged.
- **Bad:** we have to add and maintain each capability ourselves.
