# Examples

Sample agents will live here.

| Example | Status | Purpose |
|---|---|---|
| [hello](hello/src/lib.rs) | Builds and runs in CI | Remembers who has visited. **Start here**, with the [tutorial](../docs/writing-an-agent.md) |
| [sdk-check](sdk-check/src/lib.rs) | Runs in the SDK's tests | Uses the SDK as each message tells it to. It isn't a useful agent or a starting point |
| [research-helper](research-helper/agent.toml) | Manifest draft only | Uses files, web access and a model, so it exercises all the permissions |
| [api-check](api-check/src/lib.rs) | Builds in CI | Uses every API call, to check the bindings. It isn't a useful agent or a starting point |
| [file-agent](file-agent/src/lib.rs) | Runs in tests | Does what each message tells it to with its files, so that tests can drive it. It isn't a useful agent or a starting point |
