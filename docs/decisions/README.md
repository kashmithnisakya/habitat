# Design decisions

These records explain why major decisions were made. To change one, add a new record that replaces it, and don't rewrite the old one.

| Record | Decision | Status |
|---|---|---|
| [0001](0001-use-wasmtime.md) | Use Wasmtime as the first runtime | Proposed |
| [0002](0002-broker-only-agent-api.md) | Give agents a custom API, not raw WASI access | Proposed |
| [0003](0003-event-based-agents.md) | Run agents as events, not long-running processes | Proposed |
| [0004](0004-webassembly-only-packages.md) | Ship only WebAssembly in packages, as a strict tar file | Proposed |
| [0005](0005-pull-based-streaming.md) | Stream data by having agents pull it | Proposed |
| [0006](0006-limit-compile-cost.md) | Limit what compiling can cost by reading the code first | Proposed |
| [0007](0007-supported-platforms.md) | Support macOS, Windows, Linux, Android and iPhone. 0008 changes when iPhone comes | Proposed |
| [0008](0008-flutter-phone-app.md) | Use Flutter for the phone app | Proposed |

All of these stay **Proposed** until the Android feasibility test is complete. Record 0008 also needs tests on a real iPhone.
