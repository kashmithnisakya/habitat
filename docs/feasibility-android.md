# Android feasibility report

**Status: draft.** The decision below is provisional, because all of the evidence comes from an emulator. [#4](https://github.com/kashmithnisakya/habitat/issues/4) stays open until the [phone tests](#before-this-report-is-final) are done, and decision records 0001 to 0003 stay **Proposed** until then.

The [test app](https://github.com/kashmithnisakya/habitat/tree/a4f298bb04686c3984abf66d3f378ed3e96c63ed/android/feasibility) behind these results is no longer in the repository, because the [Flutter app](decisions/0006-flutter-phone-app.md) replaced the `android/` folder. It's kept at that link, and the phone tests below can use it from there.

## Decision

**Continue with Wasmtime on Android. Don't switch runtime, and don't change the core design.**

We found nothing on the emulator that blocks the plan. One part of the plan does need to change: agent packages should contain WebAssembly, not precompiled code, and the device should compile each agent once and keep the result.

These results would reopen the decision:

| Result on a real phone | What it would mean |
|---|---|
| Wasmtime crashes or is blocked by Android | Try Pulley only. If that also fails, look for another runtime. |
| Traps interfere with the Android runtime | Run agents in a separate process, or use Pulley only. |
| Compiling a typical agent takes too long or runs out of memory | Compile on a computer for trusted agents, or use Pulley. |
| Battery use is too high for agents that wake often | Reduce how often agents wake, or change the event design. |

## Evidence

| Issue | Question | Emulator result | Still needed |
|---|---|---|---|
| [#1](https://github.com/kashmithnisakya/habitat/issues/1) | Can Wasmtime be embedded in an Android app? | Yes, with no Wasmtime patches | Real phone |
| [#2](https://github.com/kashmithnisakya/habitat/issues/2) | Can it run a component under normal app rules? | Yes, in all four modes, with no SELinux denials | Real phone, 16 KB pages |
| [#3](https://github.com/kashmithnisakya/habitat/issues/3) | What does it cost? | Measured below | Real phones, battery |

### Costs

These figures are for a generated 2.7 MB test agent, and each is the median of 15 runs:

| Mode | Time to start | Memory added | CPU-heavy task |
|---|---:|---:|---:|
| Cranelift, compiled on the device | 850 ms | 79.8 MB | 218 ms |
| Pulley, compiled on the device | 924 ms | 74.0 MB | 1,425 ms |
| Cranelift, precompiled | 3.7 ms | 6.6 MB | 229 ms |
| Pulley, precompiled | 1.2 ms | 2.9 MB | 1,404 ms |

- **Compiling is the expensive step:** it took about 0.9 s and 74–80 MB. Loading compiled code took under 4 ms and 3–7 MB.
- **Starting again is very cheap:** a new instance and one call took 4–11 µs once the agent was loaded.
- **Pulley is about 6 times slower on CPU-heavy work:** this comes from one workload, so the gap will differ on others.
- **Waiting costs little in either mode:** Pulley used about 4 ms of CPU per second of waiting, and Cranelift used about 0.5 ms.

The [feasibility README](https://github.com/kashmithnisakya/habitat/blob/a4f298bb04686c3984abf66d3f378ed3e96c63ed/android/feasibility/README.md#measurements) has the full tables, the method and the limits of these numbers. The most important limit is that a phone could be much slower than an emulator on a fast Mac. The large agent also never runs most of its code, so a real precompiled agent would use more memory.

## How agents should run on Android

**Recommendation:** packages contain WebAssembly. The device compiles an agent once with Cranelift, when it's installed or first run, and saves the compiled code in the app's private storage. Later starts load the saved code.

| Option | Security | Google Play | Cost |
|---|---|---|---|
| **Compile once on the device** (recommended) | Wasmtime validates the WebAssembly, and only our app writes the saved code | Appears to fit the VM exception, but isn't confirmed | One slow first start, then fast starts and full speed |
| Compile on the device every time | Same | Same | Slow starts every time |
| Download precompiled machine code | Wasmtime doesn't validate it, so a bad file could escape the sandbox | Likely conflicts with the rule on downloading code | Fast starts, but needs a trusted builder and signing |
| Download precompiled Pulley bytecode | Wasmtime doesn't validate this either | Better fit for the interpreter exception, but isn't confirmed | Fast starts, but about 6 times slower, and needs a trusted builder and signing |

Agent packages are untrusted, so loading precompiled code from them would break our [security rules](../AGENTS.md#security-rules). A package could carry anything, and Wasmtime only checks WebAssembly when it compiles it. Precompiled packages are only an option if a builder we trust compiles and signs them, which would add infrastructure we don't otherwise need.

**This flow works on the emulator.** Compiling, saving and reloading in a new process passed in all 180 measured runs, with no SELinux denials or crashes. For the 2.7 MB test agent:

| Step | Cranelift | Pulley |
|---|---:|---:|
| First start: compile, save and load | 815 ms, 79.9 MB | 856 ms, 74.1 MB |
| Later starts: load the saved file | 2.7 ms, 6.6 MB | 1.1 ms, 3.0 MB |
| Saved file | 5.0 MB | 4.0 MB |

Later starts cost about the same as loading code compiled at build time. The [feasibility README](https://github.com/kashmithnisakya/habitat/blob/a4f298bb04686c3984abf66d3f378ed3e96c63ed/android/feasibility/README.md#compile-once) has the full comparison.

**Saved code needs protecting.** Wasmtime rejected a file saved for the other engine and a file that was cut short, but it doesn't check the code inside a saved file, so a changed byte wouldn't be detected. The design relies on nothing else being able to write to the app's private storage. The real app also needs to recompile after an upgrade, which wasn't tested, and to decide when compiling happens, because the first start is the most memory-hungry step and could be a problem on low-end phones.

**Pulley stays the fallback.** Android doesn't need it if Cranelift works on phones, but iPhone is expected to, so we should keep it working. A library with only the Pulley interpreter hasn't been built yet.

## Android and Wasmtime rules

These were checked on 2 November 2026. They can change, and this is our reading of them, not legal advice or a ruling from Google.

- **Third-party VMs aren't available:** the [Android Virtualization Framework API](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/heads/main/libs/framework-virtualization/README.md) is a system API. It needs a permission that's limited to preinstalled apps from Android 15, and to privileged apps on Android 14. Other apps can only get it through `adb` for development. It's also [optional and limited to ARM64 devices](https://source.android.com/docs/core/virtualization). This doesn't change the plan, because microVMs were never part of the Android design. It does confirm that WebAssembly is the right choice for a normal app.
- **Google Play restricts downloaded code:** [its policy](https://support.google.com/googleplay/android-developer/answer/9888379?hl=en) bans downloading executable code from outside Google Play. There's an exception for code in a VM or interpreter that only reaches Android APIs indirectly. Habitat agents only reach the outside through the broker, so WebAssembly appears to fit. Google's example is JavaScript in a browser, though, and it doesn't mention WebAssembly. Code loaded at runtime must also not allow other policy violations, so agents that users install could raise further questions. Releasing through Google Play would need a closer review.
- **Wasmtime doesn't fully support this platform:** its [support tiers](https://docs.wasmtime.dev/stability-tiers.html) put Android and iOS on Tier 3, which it calls not production ready, because they lack CI testing and a full-time maintainer. Pulley is on Tier 2. Our tests don't remove this risk, so we'd need our own device tests in CI, and we'd need to recheck each Wasmtime upgrade on Android.

## Decision records

| Record | Evidence so far | Proposed outcome |
|---|---|---|
| [0001: Use Wasmtime](decisions/0001-use-wasmtime.md) | Works on the emulator in all modes, and no VM alternative exists for normal apps | Accept after phone tests |
| [0002: Broker-only API](decisions/0002-broker-only-agent-api.md) | Components can run with only custom host functions and no WASI, but the broker doesn't exist yet | Accept after phone tests. This test doesn't really challenge it |
| [0003: Event-based agents](decisions/0003-event-based-agents.md) | Starting again takes microseconds, which suits short runs, but saving state and battery use are untested | Accept after phone tests |

## Before this report is final

- [ ] Run [#1](https://github.com/kashmithnisakya/habitat/issues/1) and [#2](https://github.com/kashmithnisakya/habitat/issues/2) on a real arm64 phone.
- [ ] Test compiling, saving and reloading an agent on a real phone. This passes on the emulator, and the test app's checks and `measure.sh` already include it.
- [ ] Repeat [#3](https://github.com/kashmithnisakya/habitat/issues/3) on a recent phone and, if possible, an older one, including battery use.
- [ ] Run on a device or emulator with 16 KB memory pages.
- [ ] Update this report with those results, and state the final decision.
- [ ] Move records 0001 to 0003 to **Accepted**, or replace them.
- [ ] Add the compile-once design to the [architecture](architecture.md), and update its 10x figure for Pulley if phones agree with our result.
