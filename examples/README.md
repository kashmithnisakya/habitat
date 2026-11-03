# Examples

Sample agents will live here.

| Example | Status | Purpose |
|---|---|---|
| [research-helper](research-helper/agent.toml) | Manifest draft only | Uses files, web access and a model, so it exercises all the permissions |
| [api-check](api-check/src/lib.rs) | Builds in CI | Uses every API call, to check the bindings. It isn't a useful agent or a starting point |
| [file-agent](file-agent/src/lib.rs) | Runs in tests, and in the README's [getting started](../README.md#getting-started) | Does what each message tells it to with its files, so that tests can drive it. It isn't a useful agent or a starting point |
