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
disk_mb = 100
model_budget_tokens = 50000

[requests]
network = ["api.github.com"]
models = ["default"]
files = "ask"
```

The manifest is untrusted, so it's validated strictly. A manifest is rejected if it has an unknown field, a repeated field or a value outside the rules below, or if it's larger than 64 KB. See the [example manifest](../examples/research-helper/agent.toml).

### `[agent]` (required)

| Field | Required | Rules |
|---|---|---|
| `name` | Yes | Up to 64 lowercase letters, digits and single hyphens. Starts with a letter, and doesn't end with a hyphen |
| `version` | Yes | A [semantic version](https://semver.org), such as `0.1.0` |

### `[limits]` (required)

The host can enforce lower limits than these, so an agent can't raise its limits above the host's maximum.

| Field | Required | Allowed values | Meaning |
|---|---|---|---|
| `memory_mb` | Yes | 1 to 4,096 | Memory for one run |
| `run_seconds` | Yes | 1 to 3,600 | Time for one run |
| `disk_mb` | Yes | 1 to 10,240 | Disk space for the agent's home |
| `model_budget_tokens` | No, defaults to 0 | 0 to 100,000,000 | AI model tokens for one run. Must be above 0 if `models` is requested |

### `[requests]` (optional)

These list what an agent may ask for, and they don't grant anything. If a field is left out, the agent can't ask for that thing.

| Field | Default | Rules |
|---|---|---|
| `network` | `[]` | Up to 64 different host names, such as `api.github.com` |
| `models` | `[]` | Up to 16 different model names of up to 64 characters. They use lowercase letters, digits, `-`, `.` and `_`, and start with a letter or digit |
| `files` | `"none"` | `"none"` or `"ask"` |

Network hosts must be lowercase and contain a dot. Schemes, ports, paths and wildcards are rejected. So are `localhost` and IP addresses, because an agent has no reason to ask for the user's own device or local network by address. Internationalized names must use their `xn--` form, so that lookalike characters can't appear in an approval prompt.

## What needs approval

| Action | Result |
|---|---|
| Read or write inside the agent's home | Always allowed, within the disk limit |
| Reach a network destination that isn't granted | Asks the user if declared in the manifest, otherwise denied |
| Use an AI model | Allowed if granted, within the token budget |
| Read a user file outside the home | Always asks, and the user chooses one file for the current run |
| Read a raw secret | Never allowed |

The [policy engine](../crates/broker/src/policy.rs) makes these decisions, and it's tested against this table. It holds no I/O, and anything that the table doesn't cover is denied. A few more details:

- **Malformed requests are denied first.** A target has to follow the same rules as the manifest, and it has to match a declared name exactly. For example, `API.GITHUB.COM` and `api.github.com.` aren't `api.github.com`.
- **A grant only counts while it's declared.** If a package no longer lists something, an old grant for it stops working.
- **There's no asking for something that can't happen.** A run that has used up its token budget is denied, instead of prompting the user.

## Approval prompts

An agent can't change what a prompt asks, only fill in two places: what it's asking for, which has to be a name that its manifest declares, and its reason, which is labelled as its own claim. Anything in these that a terminal would act on is replaced, and they're cut short. The shell draws the prompt, and [`approval.rs`](../crates/broker/src/approval.rs) doesn't depend on which shell it is.

- **Only a clear yes counts.** No answer within 2 minutes, no shell to ask or an answer that isn't recognised all mean no.
- **Refused means refused.** In one run, something the user said no to isn't asked about again.
- **Each run can ask 5 times at most.** After that, requests are denied without asking, so a user can't be worn down.

These have some limits:

- **The limit is per run, so an agent can try again in its next run.** Nothing limits prompts across runs yet.
- **A prompt can make a run overrun its time limit,** because the time limit can't stop a call that's waiting on the host. It's stopped as soon as the prompt is answered or times out.
- **Text on a terminal can be misread.** An agent's reason can still say untrue things, so it's labelled as a claim.

## Grant lifetimes

| Lifetime | Meaning |
|---|---|
| Once | Covers a single request |
| This run | Ends when the current run exits |
| Until revoked | Lasts until the user removes it |

The [grant store](../crates/broker/src/grants.rs) keeps these as follows:

- **Once and this run are only ever in memory,** so no crash can make one outlive its run. A "once" grant is used up as soon as the request it covers is allowed.
- **Until revoked is saved in `grants.db`,** which is outside the agent's home, and read again for every request. So revoking one takes effect on the next request, even from another process. If the file can't be read, nothing in it counts.
- **A grant is for one exact name.** A grant for `api.github.com` doesn't cover `github.com` or `uploads.github.com`.
- **`habitat grants <agent>` lists what's saved, and `habitat revoke <agent> --network <host>` or `--model <name>` removes it.** Once and this-run grants belong to a run in another process, so these commands can't see or revoke them. They end with the run anyway.
- **Anyone who can write to the agent's folder can add a grant.** This keeps agents out, not other programs running as the same user.
