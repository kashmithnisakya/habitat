# Roadmap

Work is tracked in the [roadmap issue](https://github.com/kashmithnisakya/habitat/issues/41) and grouped into [milestones](https://github.com/kashmithnisakya/habitat/milestones).

| Milestone | Goal |
|---|---|
| **M0: Android feasibility** | Prove Wasmtime can run an agent on a real Android phone. This comes first because it could change the whole plan. |
| **M1: Desktop core** | Run an agent with a persistent home and limits, on macOS, Linux and Windows. |
| **M2: Broker** | Check every outside request, ask for approval and log the result. |
| **M3: Secrets and models** | Call one AI model with keys kept out of the agent's reach. |
| **M4: Attack tests** | Prove the isolation with automated attacks in CI. |
| **M5: Android app** | Run the same package on a phone, with native approval screens. |
| **Later** | iPhone and iPad, Python agents, moving agents between devices and microVMs. |

## Definition of done for the first version

The same agent package works on macOS, Linux, Windows and Android, keeps its state after a restart and passes every attack test.

iPhone and iPad are part of the goal too, as [decision 0007](decisions/0007-supported-platforms.md) explains, but they come after this, and [#40](https://github.com/kashmithnisakya/habitat/issues/40) has to show that they're possible first.
