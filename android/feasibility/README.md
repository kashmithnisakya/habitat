# Android feasibility test

This throwaway app checks whether Wasmtime can be embedded in an Android app ([#1](https://github.com/kashmithnisakya/habitat/issues/1)) and run a WebAssembly component under normal app restrictions ([#2](https://github.com/kashmithnisakya/habitat/issues/2)).

The [test component](rust/guest.wat) imports one host function and exports `run`, which calls it. The app runs the component in four modes. In each one, it confirms that the host function is called, that an out-of-bounds memory read traps without crashing the app, and that the engine still works afterwards.

It isn't the real Habitat app.

## Results

| Check | Result |
|---|---|
| Library tests on macOS (arm64) | Pass |
| Cross-compile for `aarch64-linux-android` | Pass, with no patches |
| Library size | 9.8 MB |
| APK size | 9.8 MB |
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

The app was a non-debuggable build installed normally, and SELinux was enforcing. Logs from two runs showed no SELinux denials, crashes or native library errors. Java null checks and garbage collection still worked after Wasmtime had run.

### Limits of these results

- **It's an emulator:** this doesn't cover real phone hardware or manufacturer changes to Android, and it used 4 KB memory pages, so 16 KB pages haven't been tested at runtime.
- **Timings aren't measurements:** the app prints them, but they vary widely between runs. Real numbers belong to [#3](https://github.com/kashmithnisakya/habitat/issues/3).
- **Pulley isn't proven without a compiler:** the library still includes Cranelift, because the on-device modes need it. A library with only the Pulley interpreter hasn't been built.
- **The Java runtime check is limited:** it can't confirm that the null checks went through the Android runtime's signal handler.
- **The guest is minimal:** it's a hand-written component, not a real agent, and it uses no WASI or strings.

Precompiled code must only be loaded from a trusted source, because Wasmtime doesn't validate it. The real project will need signing or similar protection if it ships precompiled agents.

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

- **No default Wasmtime features:** it enables only `component-model`, `cranelift`, `pulley`, `runtime`, `std` and `wat`.
- **Precompiling at build time:** `rust/build.rs` compiles the component for the target CPU and for Pulley. This has only been tested on an Apple Silicon host.
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

The phone's screen and the log should both show five lines that start with `PASS`, and no lines that start with `FAIL`:

```text
PASS CraneliftOnDevice: run(20)=41, host called, trap caught, load 4737 us, run 73 us
PASS CraneliftPrecompiled: run(20)=41, host called, trap caught, load 4625 us, run 55 us
PASS PulleyOnDevice: run(20)=41, host called, trap caught, load 4240 us, run 65 us
PASS PulleyPrecompiled: run(20)=41, host called, trap caught, load 3222 us, run 14 us
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
