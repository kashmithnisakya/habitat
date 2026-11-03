# Contributing to Habitat

Thanks for your interest. The project is at an early stage, so feedback on the design is especially useful.

## Ways to help

- Challenge the [architecture](docs/architecture.md) or [threat model](docs/threat-model.md).
- Pick up an issue from the [roadmap](docs/roadmap.md). Issues marked `good first issue` are the best place to start.
- Test on real Android devices once the feasibility test exists.

Please report security problems as described in [SECURITY.md](SECURITY.md), not in a public issue.

## Development

You need a recent stable [Rust toolchain](https://rustup.rs) and the WebAssembly target, which you can add with `rustup target add wasm32-unknown-unknown`. Before opening a pull request, run:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

If you change the phone app, also run the checks in [app/README.md](app/README.md).

## Pull requests

- Link the issue your change addresses.
- Don't add AI tools as co-authors on commits.
- If you use an AI coding agent, point it to [AGENTS.md](AGENTS.md).
- Keep each pull request focused on one change.
- Add tests for new behaviour.
- Add an attack test for every security fix.
- Update the docs if the design changes, and add a [decision record](docs/decisions/README.md) for major decisions.

## Security-sensitive code

Changes to the broker, backend, home store or agent API get closer review. Avoid `unsafe` Rust unless there's no reasonable alternative, and explain why it's sound.

## License

By contributing, you agree that your work is licensed under the [Apache License 2.0](LICENSE).
