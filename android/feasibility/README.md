# Android feasibility test

This throwaway app checks whether Wasmtime can be embedded in an Android app ([issue #1](https://github.com/kashmithnisakya/habitat/issues/1)). It compiles a small WebAssembly `add` function on the phone with Cranelift, runs it and shows the result.

It isn't the real Habitat app.

## Results

| Check | Result |
|---|---|
| Library tests on macOS (arm64) | Pass |
| Cross-compile for `aarch64-linux-android` | Pass, with no patches |
| Library size | 7.6 MB |
| APK size | 7.6 MB |
| 16 KB page alignment | Pass, checked with `zipalign -c -P 16` |
| Runs on an arm64 emulator (Android 16, API 36, 4 KB pages) | Pass, returned `42` with Cranelift |
| Loads and runs on a real arm64 phone | **Not tested yet** |

The emulator result is an early check, not a replacement for the phone test. It doesn't cover real phone hardware or manufacturer changes to Android, and it used 4 KB memory pages, so 16 KB pages haven't been tested at runtime.

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

- **No default Wasmtime features:** it enables only `cranelift`, `runtime`, `wat` and `std`.
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

The phone's screen and the log should both say:

```text
PASS: Wasmtime returned 40 + 2 = 42
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
