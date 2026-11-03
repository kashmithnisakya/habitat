# 0004: Ship only WebAssembly in packages, as a strict tar file

**Status:** Proposed

## Context

Agent packages come from outside the device, so they're untrusted. The [Android feasibility report](../feasibility-android.md) found on an emulator that saved compiled code loads in under 4 ms, against about 0.9 s to compile. It would be tempting to ship compiled code in packages, but Wasmtime doesn't validate precompiled code when it loads it, so a hostile file could run anything. Google Play also restricts downloaded executable code.

Archive formats have a long history of path traversal and link attacks, and of tools disagreeing about what an archive contains.

## Decision

- Packages contain WebAssembly only. Each device compiles the code once and saves the result in its private storage.
- A package is an uncompressed tar file with exactly three regular files, in a fixed order, and everything else is rejected.
- Habitat never asks the tar library to unpack files. It reads each entry itself and picks the file names and modes, and uses only the size from the entry's header once the name and type are checked.
- The package hash is the SHA-256 of `package.toml`, which holds the SHA-256 of the other files.
- Installing over an existing package is refused until upgrades are designed.

See [Agent packages](../packages.md) for the details.

## Consequences

- **Good:** one file runs on every device, and packages can't carry code that skips validation.
- **Good:** a very narrow format leaves little room for parsers to disagree. Ordinary tar tools can still open a package.
- **Bad:** there's a one-time compile on each device, which may be noticeable on slower phones.
- **Bad:** there's no compression, so packages are larger to download. A download can be compressed separately.
- **Bad:** there's no way to upgrade an agent yet.

Revisit this if the phone tests show that compiling on the device is too slow, or when signatures are added.
