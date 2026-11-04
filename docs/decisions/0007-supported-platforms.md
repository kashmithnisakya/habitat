# 0007: Support macOS, Windows, Linux, Android and iPhone

**Status:** Proposed

## Context

The first plan covered Mac and Android, with Linux along the way and iPhone as a later investigation. The goal is now for Habitat to run on all of macOS, Windows, Linux, Android and iPhone or iPad, with the same security model and the same agent packages.

These aren't equal amounts of work, and Wasmtime [doesn't support them equally](https://docs.wasmtime.dev/stability-tiers.html):

| Platform | Wasmtime target | Wasmtime's tier |
|---|---|---|
| Linux | `x86_64-unknown-linux-gnu` | Tier 1 |
| Linux on Arm | `aarch64-unknown-linux-gnu` | Tier 2 |
| Mac with Intel | `x86_64-apple-darwin` | Tier 1 |
| Mac with Apple silicon | `aarch64-apple-darwin` | Tier 2 |
| Windows | `x86_64-pc-windows-msvc` | Tier 1 |
| Windows on Arm | `aarch64-pc-windows-msvc` | Tier 3 |
| Android | `aarch64-linux-android` | Tier 3 |
| iPhone and iPad | `aarch64-apple-ios` | Tier 3 |

Wasmtime describes Tier 1 as production ready, and promises security releases for it. It describes Tier 3 as not production ready, with no testing or fuzzing required.

## Decision

Support all five, with one agent API and one package format. A package holds only WebAssembly, so it doesn't need to change for any of them. What changes is the trusted code underneath, and each platform has to be at least as safe as the others, or the difference has to be written down in the [threat model](../threat-model.md).

- **Windows comes next, before the broker** ([#56](https://github.com/kashmithnisakya/habitat/issues/56)). The core only builds on Unix today, and it relies on Unix behaviour for its safety. Running every test on Windows from now on means that new features are built for it from the start, and the broker doesn't add to the work.
- **Android stays as planned,** subject to the real phone test in [M0](../feasibility-android.md).
- **iPhone and iPad come after the Android app.** Apps generally can't use a JIT there, so agents would run on Wasmtime's Pulley interpreter, which is roughly 10x slower. [#40](https://github.com/kashmithnisakya/habitat/issues/40) has to answer whether this is allowed and usable before the rest can be planned.

## Consequences

- **Good:** an agent's author only has one target to think about, and new features can't quietly depend on one platform.
- **Bad:** the home store has to be written and attacked separately for Windows, whose links, paths, permissions and locks all work differently.
- **Bad:** CI takes longer, and each change has to pass on three operating systems.
- **Bad:** a Tier 3 target gets less from Wasmtime, including no promise of security releases. That applies to Android and iPhone, and so to the two platforms where this matters most.
- **Risk:** **iPhone may not be possible.** App Store rules on running downloaded code haven't been checked yet, and they could rule this out. If so, this decision needs replacing, and nothing here promises otherwise.

Revisit this when the real phone test and #40 report back.
