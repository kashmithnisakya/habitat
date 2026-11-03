# Phone app

One [Flutter](https://flutter.dev) app for Android and iPhone. See [decision 0006](../docs/decisions/0006-flutter-phone-app.md) for the design and the rules it has to follow.

## Status

**Early development, and not secure.** The screens have widget tests and the Android app builds, but it hasn't been run on a device or emulator yet. Wasmtime next to Flutter is still [untested](../docs/roadmap.md).

| Feature | Android | iPhone |
|---|---|---|
| Install an agent from a file, or the example agent | Built | Not connected |
| List agents, with their limits | Built | Not connected |
| Send an agent a message and see its reply | Built | Not connected |
| See an agent's recent runs | Built | Not connected |
| Remove an agent | Built | Not connected |

On iPhone the screens show that the core isn't connected, because the Swift layer doesn't exist yet. Agents can only use their own files and reply, because the broker doesn't exist yet. Conversations aren't saved.

## How it fits together

```text
lib/                 Flutter screens. They only show what the core sends
pigeons/             The API between Dart and the platform, which generates core_api.g.dart and CoreApi.g.kt
android/.../kotlin/  Passes calls on to Rust, and holds what needs Android, such as the file picker
scripts/             Builds the Rust core, its Kotlin bindings and the example agent for Android
```

The Rust side is in [`crates/mobile`](../crates/mobile/src/lib.rs). Android builds run the script for you.

## Setup

You need Flutter and Rust. Android also needs the Android SDK, JDK 17 and these Rust targets:

```bash
rustup target add aarch64-linux-android wasm32-unknown-unknown
```

iPhone needs Xcode. `flutter doctor` lists anything else that's missing.

The Rust core is only built for 64-bit ARM, so use an arm64 phone or emulator.

## Checks

Run these from this folder before committing:

```bash
dart format --output=none --set-exit-if-changed .
```

```bash
flutter analyze
```

```bash
flutter test
```

## Build

```bash
flutter build apk --debug
```

```bash
flutter build ios --debug --no-codesign
```

If you change the API in `pigeons/`, regenerate its code:

```bash
dart run pigeon --input pigeons/core_api.dart
```
