# Roadmap

Work is tracked in the [roadmap issue](https://github.com/kashmithnisakya/habitat/issues/41) and grouped into [milestones](https://github.com/kashmithnisakya/habitat/milestones).

| Milestone | Goal |
|---|---|
| **M0: Android feasibility** | Prove Wasmtime can run an agent on a real Android phone. This comes first because it could change the whole plan. |
| **M0b: iPhone and Flutter feasibility** | Prove that Wasmtime's Pulley interpreter can run an agent on a real iPhone, and that Wasmtime works next to Flutter on both phones. Find out whether the App Store allows agents that users install. This comes early for the same reason as M0. |
| **M1: Desktop core** | Run an agent with a persistent home and limits, on macOS, Linux and Windows. |
| **M2: Broker** | Check every outside request, ask for approval and log the result. |
| **M3: Secrets and models** | Call one AI model with keys kept out of the agent's reach. |
| **M4: Attack tests** | Prove the isolation with automated attacks in CI. |
| **M5: Phone app** | Run the same package on Android and iPhone from one [Flutter app](decisions/0008-flutter-phone-app.md). |
| **Later** | Python agents, moving agents between devices and microVMs. |

## Definition of done for the first version

The same agent package works on macOS, Linux, Windows, Android and iPhone, keeps its state after a restart and passes every attack test.

**iPhone may not be possible.** [#40](https://github.com/kashmithnisakya/habitat/issues/40) and M0b have to show that it is, and if they can't, this definition and [decision 0008](decisions/0008-flutter-phone-app.md) need changing.
