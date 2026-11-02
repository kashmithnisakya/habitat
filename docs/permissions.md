# Permission model

This is a draft of how permissions are planned to work.

## Rules

- **Default deny:** anything not explicitly allowed is blocked.
- **Declaring isn't granting:** the manifest lists what an agent may ask for. It doesn't give the agent access.
- **Narrow grants:** a grant covers one resource and can last once, for one run or until revoked.
- **The agent can't draw the prompt:** the core shows approvals itself, so the agent can't fake or reword what's being asked. The agent's stated reason is clearly marked as its own claim.
- **Protection against prompt spam:** repeated requests are limited, so users don't get worn down into approving them.
- **Data-leak rule:** once an agent has read the user's private data, new network destinations always need fresh approval.

## Manifest

Each agent package ships with an `agent.toml`:

```toml
[agent]
name = "research-helper"
version = "0.1.0"

[limits]
memory_mb = 256
run_seconds = 120
model_budget_tokens = 50000

[requests]
network = ["api.github.com"]
models = ["default"]
files = "ask"
```

The exact format is defined in the manifest issue on the [roadmap](roadmap.md). See the [example manifest](../examples/research-helper/agent.toml).

## What needs approval

| Action | Result |
|---|---|
| Read or write inside the agent's home | Always allowed, within the disk limit |
| Reach a network destination that isn't granted | Asks the user if declared in the manifest, otherwise denied |
| Use an AI model | Allowed if granted, within the token budget |
| Read a user file outside the home | Always asks, unless a grant covers it |
| Read a raw secret | Never allowed |

## Grant lifetimes

| Lifetime | Meaning |
|---|---|
| Once | Covers a single request |
| This run | Ends when the current run exits |
| Until revoked | Lasts until the user removes it |
