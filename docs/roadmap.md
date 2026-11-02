# Roadmap

Work is tracked in the [roadmap issue](https://github.com/kashmithnisakya/agenthome/issues/41) and grouped into [milestones](https://github.com/kashmithnisakya/agenthome/milestones).

| Milestone | Goal |
|---|---|
| **M0: Android feasibility** | Prove Wasmtime can run an agent on a real Android phone. This comes first because it could change the whole plan. |
| **M1: Mac core** | Run an agent with a persistent home and limits. |
| **M2: Broker** | Check every outside request, ask for approval and log the result. |
| **M3: Secrets and models** | Call one AI model with keys kept out of the agent's reach. |
| **M4: Attack tests** | Prove the isolation with automated attacks in CI. |
| **M5: Android app** | Run the same package on a phone, with native approval screens. |
| **Later** | Python agents, moving agents between devices, microVMs and iPhone. |

## Definition of done for the first version

The same agent package works on both Mac and Android, keeps its state after a restart and passes every attack test.
