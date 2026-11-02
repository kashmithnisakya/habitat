# Android feasibility test

This throwaway app checks whether Wasmtime can be embedded in an Android app ([#1](https://github.com/kashmithnisakya/habitat/issues/1)), whether it can run a WebAssembly component under normal app restrictions ([#2](https://github.com/kashmithnisakya/habitat/issues/2)), and what that costs in time and memory ([#3](https://github.com/kashmithnisakya/habitat/issues/3)).

The [test component](rust/guest.wat) imports host functions and exports `run`, which calls one of them. The app runs the component in eight modes. In each one, it confirms that the host function is called, that an out-of-bounds memory read traps without crashing the app, and that the engine still works afterwards.

It isn't the real Habitat app.

## Results

| Check | Result |
|---|---|
| Library tests on macOS (arm64) | Pass |
| Cross-compile for `aarch64-linux-android` | Pass, with no patches |
| Library size | 20.2 MB, or about 8.5 MB without the embedded test agents |
| APK size | 20.2 MB |
| 16 KB page alignment | Pass, checked with `zipalign -c -P 16` |
| Runs on an arm64 emulator (Android 16, API 36, 4 KB pages) | Pass in all modes below |
| Loads and runs on a real arm64 phone | **Not tested yet** |

### Modes on the emulator

| Mode | Where the code is compiled | Host call and trap | Android blocked it? |
|---|---|---|---|
| `CraneliftOnDevice` | On the device, to machine code | Pass | No |
| `CraneliftPrecompiled` | At build time, to machine code | Pass | No |
| `PulleyOnDevice` | On the device, to Pulley bytecode | Pass | No |
| `PulleyPrecompiled` | At build time, to Pulley bytecode | Pass | No |
| `CraneliftCompileFirst` | On the device, to machine code, then saved and loaded from the file | Pass | No |
| `CraneliftCompileAgain` | Not compiled. Loads the file saved by an earlier process | Pass | No |
| `PulleyCompileFirst` | On the device, to Pulley bytecode, then saved and loaded from the file | Pass | No |
| `PulleyCompileAgain` | Not compiled. Loads the file saved by an earlier process | Pass | No |

The app was a non-debuggable build installed normally, and SELinux was enforcing. Logs from the check runs showed no SELinux denials, crashes or native library errors. That includes loading precompiled machine code from a file in the app's private storage. Java null checks and garbage collection still worked after Wasmtime had run.

### Limits of these results

- **It's an emulator:** this doesn't cover real phone hardware or manufacturer changes to Android, and it used 4 KB memory pages, so 16 KB pages haven't been tested at runtime.
- **Pulley isn't proven without a compiler:** the library still includes Cranelift, because the on-device modes need it. A library with only the Pulley interpreter hasn't been built.
- **The Java runtime check is limited:** it can't confirm that the null checks went through the Android runtime's signal handler.
- **The guest is minimal:** it's a hand-written component, not a real agent, and it uses no WASI or strings.

Precompiled code must only be loaded from a trusted source, because Wasmtime doesn't validate it. The real project will need signing or similar protection if it ships precompiled agents.

## Measurements

[#3](https://github.com/kashmithnisakya/habitat/issues/3) stays open: these numbers come from an emulator, not a phone, and battery use hasn't been measured.

| Item | Value |
|---|---|
| Device | Android emulator, `sdk_gphone64_arm64`, 4 cores, 2.5 GB memory |
| Host | macOS on Apple Silicon |
| OS | Android 16, API 36, 4 KB pages, SELinux enforcing |
| Runs | 15 for each mode and agent. Every value is a median |
| Raw data | [results/emulator-arm64-api36.csv](results/emulator-arm64-api36.csv) |

### Startup and memory

| Mode | Agent | File | Agent cold start (range) | Warm start | Memory added |
|---|---|---:|---:|---:|---:|
| `CraneliftOnDevice` | Hello | 1 KB | 5.0 ms (2.4–6.5) | 10 µs | 5.6 MB |
| `CraneliftPrecompiled` | Hello | 195 KB | 1.2 ms (0.7–2.1) | 11 µs | 2.9 MB |
| `PulleyOnDevice` | Hello | 1 KB | 4.5 ms (1.8–10.0) | 6 µs | 5.9 MB |
| `PulleyPrecompiled` | Hello | 67 KB | 0.8 ms (0.5–54.0) | 9 µs | 2.5 MB |
| `CraneliftOnDevice` | Large | 2,693 KB | 850 ms (751–2,211) | 4 µs | 79.8 MB |
| `CraneliftPrecompiled` | Large | 4,979 KB | 3.7 ms (1.7–10.0) | 9 µs | 6.6 MB |
| `PulleyOnDevice` | Large | 2,693 KB | 924 ms (805–1,307) | 4 µs | 74.0 MB |
| `PulleyPrecompiled` | Large | 4,018 KB | 1.2 ms (0.8–5.6) | 9 µs | 2.9 MB |

The app itself took 57–108 ms to start before any agent code ran, and used 113 MB before the agent loaded. Neither depends on the mode, so the differences in that range are noise.

### Workloads

| Mode | Agent | CPU workload, CPU time | CPU workload, elapsed | CPU time during 1 s of waiting |
|---|---|---:|---:|---:|
| `CraneliftOnDevice` | Hello | 226 ms | 230 ms | 0.5 ms |
| `CraneliftPrecompiled` | Hello | 230 ms | 235 ms | 0.5 ms |
| `PulleyOnDevice` | Hello | 1,415 ms | 1,442 ms | 3.9 ms |
| `PulleyPrecompiled` | Hello | 1,433 ms | 1,450 ms | 3.8 ms |
| `CraneliftOnDevice` | Large | 218 ms | 221 ms | 0.5 ms |
| `CraneliftPrecompiled` | Large | 229 ms | 232 ms | 0.5 ms |
| `PulleyOnDevice` | Large | 1,425 ms | 1,448 ms | 4.1 ms |
| `PulleyPrecompiled` | Large | 1,404 ms | 1,423 ms | 4.3 ms |

### What the numbers show

- **Compiling on the device is the expensive step:** for the large agent, it took about 0.9 s and 74–80 MB. Loading precompiled code took under 4 ms and 3–7 MB.
- **Pulley is about 6 times slower on CPU-heavy work:** that's on this one workload, and the gap will differ on others.
- **Pulley costs little while waiting:** it used about 8 times as much CPU as Cranelift, but that's only about 4 ms per second of waiting.
- **Precompiled files are larger:** the 2.7 MB agent became 5.0 MB of machine code or 4.0 MB of Pulley bytecode.

An earlier run of the same test gave similar results, with on-device cold starts for the large agent within 3% and CPU times within 4%. Cold starts for small agents varied more, by up to about 2 ms. Its data isn't committed, because it used a broader SELinux denial count. That run counted 2 denials, both logged against Google Photos, and the final run counted none for this app.

### Compile once

The [draft feasibility report](../../docs/feasibility-android.md) recommends compiling each agent once on the device and saving the result. This test runs that flow: one process compiles, saves and loads the agent, and the next process only loads the saved file. These figures are for the large agent, from a separate run of 15 repeats, with precompiled modes alongside for comparison ([raw data](results/emulator-arm64-api36-compile-once.csv)):

| Mode | Saved or loaded file | Agent start (range) | Memory added | CPU workload, CPU time |
|---|---:|---:|---:|---:|
| `CraneliftCompileFirst` | 4,979 KB | 815 ms (751–983) | 79.9 MB | 210 ms |
| `CraneliftCompileAgain` | 4,979 KB | 2.7 ms (1.2–4.3) | 6.6 MB | 232 ms |
| `CraneliftPrecompiled` | 4,979 KB | 5.7 ms (4.2–7.9) | 6.8 MB | 229 ms |
| `PulleyCompileFirst` | 4,018 KB | 856 ms (779–1,617) | 74.1 MB | 1,357 ms |
| `PulleyCompileAgain` | 4,018 KB | 1.1 ms (0.5–2.2) | 3.0 MB | 1,402 ms |
| `PulleyPrecompiled` | 4,018 KB | 1.2 ms (0.6–2.1) | 2.9 MB | 1,393 ms |

- **It works, and Android blocked nothing:** there were no SELinux denials or crashes in any of the 180 runs.
- **Later starts match build-time precompiling:** memory and CPU time are about the same. Load times are similar too, and the gap between the Cranelift modes is small enough to be noise.
- **The cost is one slow first start:** on this emulator, that's about 0.8 s and 74–80 MB, and it needs 4–5 MB of storage per agent.
- **Bad files are rejected in two cases:** a file saved for the other engine, and a file cut in half, both fail to load. The app runs this check on the device, and it's the `BadSavedFiles` line in its report.

This doesn't cover every bad file. Wasmtime doesn't check the code inside a saved file, so changing a byte of it wouldn't be detected, and the design relies on nothing else being able to write to private storage. Upgrading the app or Wasmtime also wasn't tested. This test gives each build its own file names, so a new build would compile again.

### Limits of these measurements

- **Emulator speed doesn't predict phone speed:** the emulator runs on a fast Mac and has no thermal limits, so a phone's numbers could be much worse. The ratios may also change.
- **Battery isn't measured:** the emulator's battery is simulated, so CPU time is the only related figure here. It isn't a battery estimate.
- **Older phones aren't covered:** no phones have been tested yet.
- **The large agent is synthetic:** its size is a rough choice, and most of its code is generated functions that never run. Android loads file pages when they're used, so a precompiled agent that runs all of its code would use more memory than this.
- **Agent memory is small:** every agent uses one 64 KB page of memory, while a real agent would use more.
- **Waiting is simulated:** the host sleeps instead of using a network, so this doesn't include any network or TLS work.
- **Cold start isn't a first start:** each run uses a new process, but the APK and agent files are probably already in the operating system's cache.
- **Warm start is minimal:** it creates a new instance and calls one small function, and doesn't touch the agent's memory.
- **Memory is the peak resident size:** it includes pages shared with other processes, so it can overstate what the app alone costs.

### How to repeat them

With one device connected and a built APK, from the repository root:

```bash
android/feasibility/measure.sh android/feasibility/results/my-device.csv 15
```

The output file must not already exist. Keep the device otherwise idle and its screen on, and avoid heavy work on the computer if you're using an emulator. By default, this runs all eight modes, which should take about 25 minutes on the emulator and may take longer on a phone.

To run only some modes, set `MODES`, and keep each `CompileAgain` mode after its `CompileFirst` mode:

```bash
MODES="CraneliftCompileFirst CraneliftCompileAgain" android/feasibility/measure.sh android/feasibility/results/my-device.csv 15
```

To print the table again from a results file:

```bash
android/feasibility/measure.sh --summary android/feasibility/results/my-device.csv
```

The script installs the APK without incremental install, then discards one launch that extracts the agent files. Each repeat then runs every mode and agent in turn, so slow changes affect them all equally. Each run is a new process, which does the following:

1. **App start:** records the time from process start until the native library is loaded.
2. **Settling:** waits 2 seconds so that Android's drawing isn't counted as agent memory. This wait isn't part of any timing, so agents don't compete with app startup here as they would in real use.
3. **Agent cold start:** creates the engine, loads the agent from a file, makes the first instance and calls `run`.
4. **Warm start:** runs a new instance and `run` 101 times, and keeps the median.
5. **CPU workload:** hashes all of the agent's memory 10,000 times, and checks the result so that every mode must do the same work.
6. **Waiting:** runs 10 waits of 100 ms, with a little work after each one.
7. **Memory:** subtracts the peak resident size before the agent loaded from the peak at the end.

CPU time is measured with `CLOCK_THREAD_CPUTIME_ID`, and each row also records the SELinux denials logged for this app.

### Measuring battery on a phone

This method hasn't been tested yet. It uses wireless debugging, because USB would charge the phone during the test:

1. Reset the statistics with `adb shell dumpsys batterystats --reset`.
2. Run `measure.sh` over wireless debugging.
3. Read the app's estimated use with `adb shell dumpsys batterystats --charged io.github.kashmithnisakya.habitat.feasibility`.

## Tested versions

| Tool | Version |
|---|---|
| Host | macOS on Apple Silicon |
| Rust | 1.98.1 |
| Wasmtime | 50.0.1 |
| Android NDK | 30.0.16248370 |
| Android platform | `android-36`, minimum API 26 |
| Build tools | 36.1.0 |
| JDK | 17.0.21 |

## Patches and workarounds

No Wasmtime patches were needed. The build uses these settings:

- **No default Wasmtime features:** it enables only `component-model`, `cranelift`, `pulley`, `runtime` and `std`.
- **Precompiling at build time:** `rust/build.rs` compiles each component for the target CPU and for Pulley. This has only been tested on an Apple Silicon host.
- **Embedded test agents:** the library contains the agent files and extracts them to private storage on first launch, which keeps the APK build simple. A real app wouldn't ship agents this way.
- **16 KB memory pages:** the library links with `max-page-size=16384`, and the APK is aligned with `zipalign -P 16`.
- **No Gradle:** `build.sh` calls the Android tools directly, which keeps this test small.
- **One debug key:** the throwaway signing key is kept at `~/.android/habitat-feasibility-debug.keystore`, because Android rejects updates signed with a different key.
- **Centered text:** on API 35 and later, content draws behind the title bar, which hid text placed near the top of the screen.

## Set up

These steps are for macOS with [Homebrew](https://brew.sh):

```bash
rustup target add aarch64-linux-android
```

```bash
brew install openjdk@17
```

```bash
brew install --cask android-commandlinetools
```

```bash
export JAVA_HOME="$(brew --prefix openjdk@17)/libexec/openjdk.jdk/Contents/Home"
```

```bash
export PATH="$JAVA_HOME/bin:$PATH"
```

Review and accept the Android SDK licences:

```bash
sdkmanager --sdk_root="$HOME/Library/Android/sdk" --licenses
```

Install the Android packages:

```bash
sdkmanager --sdk_root="$HOME/Library/Android/sdk" "ndk;30.0.16248370" "platform-tools" "platforms;android-36" "build-tools;36.1.0"
```

The Android packages use about 3.2 GB. The script looks for the SDK in `$ANDROID_HOME`, then in `~/Library/Android/sdk`.

## Build

From the repository root:

```bash
android/feasibility/build.sh
```

The APK is written to `android/feasibility/build/habitat-feasibility.apk`.

## Run on a phone

Turn on USB debugging on an arm64 Android phone and connect it, then run:

```bash
"$HOME/Library/Android/sdk/platform-tools/adb" install -r android/feasibility/build/habitat-feasibility.apk
```

```bash
"$HOME/Library/Android/sdk/platform-tools/adb" logcat -c
```

```bash
"$HOME/Library/Android/sdk/platform-tools/adb" shell am start -n io.github.kashmithnisakya.habitat.feasibility/.MainActivity
```

```bash
"$HOME/Library/Android/sdk/platform-tools/adb" logcat -d -s HabitatFeasibility
```

The phone's screen and the log should both show ten lines that start with `PASS`, and no lines that start with `FAIL`:

```text
PASS CraneliftOnDevice: run(20)=41, host called, trap caught, load 88184 us, run 219 us
PASS CraneliftPrecompiled: run(20)=41, host called, trap caught, load 236 us, run 44 us
PASS PulleyOnDevice: run(20)=41, host called, trap caught, load 3488 us, run 109 us
PASS PulleyPrecompiled: run(20)=41, host called, trap caught, load 123 us, run 1414 us
PASS CraneliftCompileFirst: run(20)=41, host called, trap caught, load 50219 us, run 38 us
PASS CraneliftCompileAgain: run(20)=41, host called, trap caught, load 60 us, run 7 us
PASS PulleyCompileFirst: run(20)=41, host called, trap caught, load 1209 us, run 23 us
PASS PulleyCompileAgain: run(20)=41, host called, trap caught, load 22 us, run 11 us
PASS BadSavedFiles: other engine's file and cut-short file rejected
PASS AndroidRuntime: null checks and garbage collection still work
```

Also check for blocked operations:

```bash
"$HOME/Library/Android/sdk/platform-tools/adb" logcat -d | grep -E 'avc: *denied|FATAL EXCEPTION|Fatal signal|UnsatisfiedLink'
```

## Run on an emulator

The emulator is optional. Install it and create a device with:

```bash
sdkmanager --sdk_root="$HOME/Library/Android/sdk" "emulator" "cmdline-tools;latest" "system-images;android-36;google_apis;arm64-v8a"
```

```bash
echo no | ANDROID_HOME="$HOME/Library/Android/sdk" "$HOME/Library/Android/sdk/cmdline-tools/latest/bin/avdmanager" create avd --name habitat_feasibility --package "system-images;android-36;google_apis;arm64-v8a" --device pixel_6
```

Start it with a small data partition if disk space is tight. The system image and emulator use about 5.5 GB, and by default the device also asks for 12 GB of free space:

```bash
ANDROID_HOME="$HOME/Library/Android/sdk" "$HOME/Library/Android/sdk/emulator/emulator" -avd habitat_feasibility -no-window -no-audio -no-snapshot -no-boot-anim -gpu swiftshader_indirect -partition-size 2048
```

Then use the `adb` commands from the phone section. Homebrew's `avdmanager` didn't find the SDK in our test, so use the copy inside the SDK.

## Remove it

Delete this folder once [the feasibility decision](https://github.com/kashmithnisakya/habitat/issues/4) has been made.
