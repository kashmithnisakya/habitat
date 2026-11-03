# Fuzz tests

These feed random input to everything that reads something untrusted, and check more than just that nothing crashes: each one also checks what has to be true of whatever gets accepted.

| Target | What it reads | What it checks |
|---|---|---|
| `manifest` | `agent.toml` | What's accepted holds to the rules, and limits stay within the host's. Errors are safe to print |
| `package` | A package file | What's accepted round trips and installs. Errors are safe to print |
| `home_path` | A path from an agent | What's accepted is plain names, inside the home, and nothing outside it changes |
| `url` | A URL or a redirect | What's accepted is exactly what was checked, and only an absolute `https://` URL changes host |
| `policy` | A target from an agent | A malformed or undeclared one is denied, whatever grants claim |
| `code_shape` | A package's code | Reading it before compiling can't crash, and gives the same answer each time |
| `saved_files` | `grants.db` and `audit.db` | A corrupted file is refused or read safely, and can't allow anything invalid |

They need a nightly compiler and `cargo-fuzz`:

```bash
rustup toolchain install nightly
```

```bash
cargo install cargo-fuzz --locked
```

To run one for a minute, from the root of this repository:

```bash
cargo +nightly fuzz run manifest -- -max_total_time=60
```

CI runs each for a short time on every pull request, and for longer every week. A crash leaves its input in `fuzz/artifacts/`. Fix it, and add it as a regression test in the crate it's about.
