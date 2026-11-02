# Design decisions

These records explain why major decisions were made. To change one, add a new record that replaces it, and don't rewrite the old one.

| Record | Decision | Status |
|---|---|---|
| [0001](0001-use-wasmtime.md) | Use Wasmtime as the first runtime | Proposed |
| [0002](0002-broker-only-agent-api.md) | Give agents a custom API, not raw WASI access | Proposed |
| [0003](0003-event-based-agents.md) | Run agents as events, not long-running processes | Proposed |

All three stay **Proposed** until the Android feasibility test is complete.
