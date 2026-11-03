# What compiling can cost

Loading a package compiles it, and a package is untrusted. This records what a hostile package can make that cost, the limits we chose because of it, and what those limits leave open. [Decision 0006](decisions/0006-limit-compile-cost.md) explains the approach.

## How this was measured

Everything goes through `WasmBackend::load`, using Wasmtime 50.0.1 in a release build. Each component loads in a process of its own, and a compile only uses one core.

| Device | Details |
|---|---|
| Mac | Apple Silicon, 10 cores, 16 GB of memory |
| Android emulator | `sdk_gphone64_arm64`, 4 cores, 2.5 GB of memory, on the same Mac |
| Real phone | **Not measured.** None was available, and a phone could be much slower |

To repeat a measurement, on any device that has `ps`, run this with a time limit in seconds, a memory limit in MB, and a case from [the list](../crates/backend-wasm/examples/compile_cost.rs):

```bash
cargo run --release -p habitat-backend-wasm --example compile_cost -- 300 4000 tiny_functions 100000
```

The tool kills a run that goes over either limit, which the tables call the guard. Setting `HABITAT_COMPILE_LIMITS=1` turns the limits on. The hostile components are built by [`hostile_code.rs`](../tests/attacks/support/hostile_code.rs).

These numbers have some limits of their own:

- **Peak memory is sampled.** It comes from `ps` about every 20 ms, so it can miss a short spike.
- **Peak memory varies on the Mac.** The same component reached between 1.0 and 2.2 GB in different runs, and less again when other programs were using a lot of memory. Treat Mac figures as rough. The same component stayed under 650 MB on the emulator.
- **They're single runs.** They're here to show how large each cost is, not to compare small differences.

## Without limits

Without limits, file size says little about the cost. These are all on the Mac:

| What the package holds | File size | Time | Peak memory |
|---|---|---|---|
| 12 levels of components, each creating 8 instances of the next | Under 4 KB | Over 60 s (guard) | Kept growing |
| The same with 8 levels | Under 4 KB | 7.4 s | 3.0 GB |
| 10,000 loops in a row, passing 2 values | 30 KB | | Over 3 GB (guard) |
| 3,000 nested loops, with 3,000 locals | 35 KB | Over 60 s (guard) | |
| 10 modules of 10 memories, with 2 bytes of data 16 MB apart in each | 2 KB | | Over 3 GB (guard) |
| 100 modules of 100 tables, each of 1 million entries with one set | 0.1 MB | | Over 3 GB (guard) |
| 10,000 functions of 1,000 parameters and results, listed in a table | 0.1 MB | 142 s | 1.1 GB |
| 4,000 instances of a module with 10,000 imports | 0.2 MB | 10 s | 2.8 GB |
| 100,000 blocks in a row, each with a result | 0.6 MB | 2.5 s | 5.2 GB |
| 1,400 loops of 1,400 locals, nested 8 deep | 2.1 MB | 67 s | 1.7 GB |
| 1 million of the smallest possible functions | 5.7 MB | 20 s | 3.6 GB |
| One function of plain arithmetic, the largest allowed | 7.3 MB | 39 s | 4.5 GB |
| 10,000 function types of 1,000 parameters | 9.6 MB | 85 s | 1.2 GB |
| 90,000 instance types of 100 functions | 68 MB | 29 s | 1.4 GB |
| Plain arithmetic in 1 MB functions, up to the 128 MB limit | 127 MB | Over 240 s (guard) | 2.7 GB and growing |
| A data segment, up to the 128 MB limit | 127 MB | 0.1 s | 412 MB |
| A custom section, up to the 128 MB limit | 127 MB | 0.0 s | 130 MB |

The bombs come from a few causes:

- **Multiplication.** Each level of nested components multiplies the instances, because Wasmtime works out every instance when it compiles. Locals times branches, and instances times imports, also multiply.
- **Costs that grow with the square.** Some of the compiler's work grows much faster than the size of a function.
- **Work that isn't in the file.** A few bytes can ask for a memory image or a table to be laid out in advance, or for glue code to be generated.

## Real code

For comparison, we measured 21 real programs, which were CPython, resvg, quickjs, onig, openjpeg, tree-sitter and its parsers, and a Rust library. We also checked the debug and release builds of our sample agents. CPython is the largest, with 4.4 MB of code in a 29 MB file, and it took 2.5 s and 215 MB on the Mac and 2.9 s and 223 MB on the emulator. resvg, with 1.3 MB of code, took 0.7 s and 60 MB.

None of these are Habitat agents, so this is a small sample of how real code tends to look. In particular, we haven't measured a Python agent built with `componentize-py`, which could hold much more than this.

## The limits

These are checked by [reading the code](../crates/backend-wasm/src/code_shape.rs) before anything's compiled. A package over any of them is refused with `LoadError::TooCostly`, and the message holds only our own words and counts. A host can change them with `WasmBackend::set_compile_limits`.

| Limit | Value | Most in real code | Why |
|---|---|---|---|
| Size of `agent.wasm` | 128 MB | 29 MB | The package format's limit |
| Nested components | 0 | 0 | The cost multiplies at each level, and can also multiply through components passed as imports, which is hard to work out in advance |
| Modules | 256 | 3 | Keeps the rest of these limits meaningful |
| Instances of modules times imports of modules | 4,000,000 | 150 | Wasmtime records every import of every instance |
| Functions | 100,000 | 10,465 | Each one costs at least a few KB |
| Bytes of code | 16 MB | 4.4 MB | The cost at each byte is what's left after the rest of these limits |
| Bytes in one function | 512 KB | 372 KB | About 1.1 KB of memory per byte while compiling it |
| Work, explained below | 10¹² | 7.5 × 10¹⁰ | Values tracked through branches |
| Nested loops | 16 | 7 | Makes each value much more costly to track |
| Loops in one function | 1,024 | 327 | The cost grows with the square of this |
| Branch points in one function | 64,000 | 24,686 | |
| Values passed through blocks in one function | 1,024 | 170 | The cost grows with this, times the square of the number of blocks |
| Memories | 16 | 1 | Each can have an image of up to 1 MB, however little data it has |
| Table entries | 1,000,000 | 5,894 | Tables can be laid out in advance |
| Canonical functions | 2,048 | 15 | Each can need an adapter to be generated |
| Declarations | 100,000 | 243 | Types, imports, exports, instances and so on |
| Bytes of declarations | 8 MB | 25 KB | A single type can hold a lot |
| Parameters and results in signatures | 1,000,000 | 34,360 | Each costs up to about 9 µs of generated glue code |
| Time | 120 s | 2.9 s | A backstop, and the only limit that depends on the device |

"Most in real code" for modules and canonical functions comes from our sample agents, because the other programs are single modules. Those figures say the least.

Work adds up over all functions. For each function, it takes the values to track (its locals, its results and the most values on its operand stack), times the places to track them (its branch points and blocks, plus one), and squares the result. It's squared because the time grows that way. The most costly shape took 18 s as one function at the limit, and 35 s as 16 smaller functions at the limit, which is what this should keep it to. Without the square, those 16 functions could have been complex enough to take about 5 minutes.

## Within the limits

These are the costliest components we could find that pass every limit:

| What the package holds | File size | Mac | Emulator |
|---|---|---|---|
| All of the below that can fit at once: nearly all the code, one costly function, 99,000 functions, 90 MB of data, and instances with imports | 106 MB | 76 s, 1.6 GB | 97 s, 1.3 GB |
| Plain arithmetic in functions of 512 KB | 15 MB | 49–66 s, 1.0–2.2 GB | 80 s, 642 MB |
| Plain arithmetic in functions of 64 KB | 15 MB | 43 s, 500 MB | 53 s, 106 MB |
| Plain arithmetic in one function | 0.5 MB | 1.7 s, 572 MB | 2.6 s, 544 MB |
| 700 loops of 700 locals, nested 16 deep, in 1 function | 0.3 MB | 18 s, 497 MB | 16 s, 419 MB |
| 350 loops of 350 locals, nested 16 deep, in 16 functions | 1.0 MB | 35 s, 235 MB | 33 s, 117 MB |
| 99,000 of the smallest possible functions | 0.6 MB | 1.7 s, 540 MB | 2.3 s, 567 MB |
| 99,000 exported functions | 1.5 MB | 4.5 s, 1.3 GB | |
| 99,000 functions with long names | 95 MB | 1.9 s, 888 MB | |
| 256 instances of a module with 15,000 imports | 0.3 MB | 1.1 s, 283 MB | 1.2 s, 264 MB |
| 490 functions of 1,000 parameters and results, listed in a table | Under 0.1 MB | 6.7 s, 34 MB | |
| 900 function types of 1,000 parameters | 0.9 MB | 7.7 s, 126 MB | |
| 7 instance types of 100,000 functions | 7.3 MB | 6.8 s, 316 MB | |
| 1,000 adapters for a deeply nested tuple | 0.1 MB | 4.7 s, 34 MB | 4.9 s, 39 MB |
| A data segment | 127 MB | 0.1 s, 439 MB | 0.1 s, 362 MB |

So the most we've seen is about 2.2 GB, and about 76 s for one component. The first row leaves out glue code, large types and adapters, which would add about 19 s on the Mac if they were all there too, so 95 s is a fairer worst case. On the emulator, that's about 116 s.

A refused package costs almost nothing. The bombs above were refused in under 10 ms on the Mac, because reading stops at the first limit that's broken. Reading and checking all of CPython takes 0.03 s, compared with 2.5 s to compile it.

## Cranelift settings

We also tried Cranelift's settings on the worst shapes, without our limits. All are on the Mac:

| Shape | Default | No optimization | Single-pass allocator | Both |
|---|---|---|---|---|
| One arithmetic function of 1 MB | 3.9 s, 1.2 GB | 0.2 s, 229 MB | 3.6 s, 1.2 GB | 0.1 s, 126 MB |
| 16 MB of arithmetic in 512 KB functions | 57 s, 2.4 GB | 3.0 s, 510 MB | 52 s, 2.4 GB | 1.6 s, 441 MB |
| 700 loops of 700 locals, nested 8 deep | 8.2 s, 499 MB | 8.2 s, 468 MB | 0.3 s, 345 MB | 0.3 s, 215 MB |
| 1,000 nested loops of 1,000 locals | 4.6 s, 231 MB | 4.7 s, 142 MB | 4.6 s, 61 MB | 4.6 s, 81 MB |
| 64,000 blocks in a row, each with a result | 1.1 s, 3.4 GB | 1.0 s, 3.5 GB | 1.0 s, 3.4 GB | 1.0 s, 3.7 GB |
| 100,000 of the smallest possible functions | 1.7 s, 547 MB | 1.4 s, 555 MB | 1.5 s, 657 MB | 1.2 s, 637 MB |

Each setting helps with a few shapes and leaves the rest as they were, so none of them could replace the limits. They'd also change the code that every honest agent runs on, and we haven't measured by how much. Leaving out address maps made no real difference. We left all of these at their defaults. They could be worth another look for hosts that care more about compile time than speed.

Two settings did change. The image for a memory that's mostly empty is limited to 1 MB, down from 16 MB, because a few bytes could otherwise ask for 16 MB. This shouldn't affect real code much, because an image is always allowed when at least half of it is data, though we haven't measured that. And Wasmtime's `parallel-compilation` feature stays off, so that a compile only uses one core.

## What's left open

- **Memory isn't enforced while compiling.** These limits keep it down by refusing what we know to be costly, and nothing stops a compile that uses more than expected. About 2.2 GB could get the app stopped on a low-end phone. A kind of bomb that we haven't found could use more, and we kept finding new ones while doing this work.
- **An abandoned compile keeps running.** It can't be stopped, so it uses one core until it ends, and other packages wait for it. Within the limits that's about 95 s on the Mac. On a phone it could be several times that.
- **Running out of memory ends the process.** If the compiler can't allocate memory, the host process usually ends, and there's no way to recover from that within the process.
- **A separate process would close the first three.** It could be killed at any time and be given a memory limit. See [decision 0006](decisions/0006-limit-compile-cost.md) for why this comes later.
- **The time limit depends on the device.** A package near these limits could compile in time on a Mac and not on a slow phone, so it wouldn't work everywhere.
- **The compiler's panics aren't tested.** A panic while compiling is reported as invalid code, and frees the slot, but we found no input that causes one, so only the freeing is tested.
- **The limits are tied to this version of Wasmtime.** They need measuring again after an upgrade.
- **A compile runs on every load.** So these costs are paid every time, until [compiling once and saving the result](feasibility-android.md) is built.
- **The limits are per backend.** A host should use one `WasmBackend`, because two could compile at once.
