# Agent packages

A package is one file that holds an agent's code and manifest. The same file is meant to run on Mac, Android and iPhone, because it holds WebAssembly and nothing built for a specific device.

Packages are untrusted. Installing one checks everything in it first, and nothing in the package decides where its files go.

## Format

A package is an uncompressed tar archive with exactly these three regular files, in this order:

| File | Limit | Contents |
|---|---|---|
| `package.toml` | 4 KB | The format version and the SHA-256 of the other two files |
| `agent.toml` | 64 KB | The [manifest](permissions.md#manifest) |
| `agent.wasm` | 128 MB | A WebAssembly component |

`package.toml` looks like this:

```toml
format = 1

[files]
"agent.toml" = "<SHA-256 in lowercase hex>"
"agent.wasm" = "<SHA-256 in lowercase hex>"
```

The same contents always give the same package file, because file times, owners and modes are fixed when it's written.

## Package hash

The **package hash** is the SHA-256 of `package.toml`. Since that file holds the hash of every other file, the package hash covers the whole package.

## What's rejected

A package is rejected if it:

- has any entry other than the three files above, or has them in a different order
- has an entry name that isn't an exact match, such as `../agent.wasm`, `/agent.wasm` or `./agent.wasm`
- has a link, directory, device, sparse file, long-name entry or extended header entry
- has a file over its limit, which is checked before the file is read
- has anything other than zeros after its last entry
- is cut short or fails the tar header checksum
- has a file that doesn't match its hash
- has a `package.toml` with an unknown field, or an unsupported format version
- has an invalid manifest
- has an `agent.wasm` that doesn't start with the WebAssembly component header

The component header check is only a quick one. The component is fully validated when it's compiled on the device, which isn't built yet ([#9](https://github.com/kashmithnisakya/habitat/issues/9)).

## Installing

Installing unpacks the three files into the agent's `package/` folder as read-only files. It then records the package hash in `package.sha256`, next to that folder and not inside it.

- **Interrupted installs:** the hash is recorded last, so the agent doesn't count as installed until everything else is saved. Running the install again starts over.
- **The agent's name:** a package only installs into the folder for the name in its manifest.
- **The agent's home:** installing doesn't touch it.
- **Upgrades:** installing over an existing package is refused. Upgrades aren't designed yet, because a new package with the same name would inherit the agent's home and grants, and nothing can prove yet that it comes from the same author.

## Detecting changes after install

Loading an installed package reads all its files and checks them against the recorded hash. Loading fails if a file has changed, a file is missing, there's an extra file, or a file has been replaced by something other than a regular file.

The check returns the bytes it verified, so the code that runs is the code that was checked.

## Signatures

There are no signatures yet, and the format leaves room for them in two ways:

- A signature can cover the package hash, which already covers every file.
- A new format version can add signature fields or files. Readers check the version first, so an older reader reports an unsupported format.

## Limits

- **Hashes aren't proof of who made a package.** Someone who can change a package can also update the hashes inside it. Hashes catch corruption, and they catch a package that differs from one whose hash you already know.
- **Changes after install are detected, not prevented.** Someone who can write to the agent's folder can also change `package.sha256`. On a phone, the app's private storage protects that folder. On a computer, other programs running as the same user can reach it.
- **Packages are held in memory** while they're read, so a package can use up to about 128 MB during install and load. This is acceptable for now, but may need revisiting on small phones.
- **Package files don't count towards `disk_mb`,** which only limits the agent's home.
