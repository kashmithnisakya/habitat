# 0008: Use Flutter for the phone app

**Status:** Proposed

## Context

Habitat has to run on iPhone as well as Android. The plan was a Kotlin app on Android, with iPhone left for later, so supporting both that way would mean building every screen twice. This replaces the part of [decision 0007](0007-supported-platforms.md) that has iPhone coming after the Android app. The rest of that decision stands, including that iPhone may not be possible.

| Option | For | Against |
|---|---|---|
| Separate Kotlin and SwiftUI apps | Official [UniFFI](https://github.com/mozilla/uniffi-rs) bindings on both, and no second runtime | Every screen is built twice |
| React Native | One UI, using native views | A JavaScript bundle and its npm packages join the trusted code, and its [UniFFI bindings](https://github.com/jhugman/uniffi-bindgen-react-native) are third-party and haven't reached version 1.0 |
| Flutter | One UI, compiled ahead of time in release builds | The Flutter engine and Dart packages join the trusted code, and it draws its own widgets |

## Decision

Build one Flutter app for Android and iPhone, in `app/`.

```text
Flutter UI (Dart)
      │ Typed platform channels (Pigeon)
Kotlin on Android, Swift on iPhone
      │ UniFFI
Rust core
```

Dart doesn't call Rust directly. Background work has to reach the core without starting Dart, and the OS keystores are platform code, so the Kotlin and Swift layers need bindings anyway. Adding [flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge) would mean a second set of bindings for the same core.

The threat model trusts the shell, so the app follows these rules:

- **The core decides:** policy stays in Rust, and the core supplies the wording of every prompt. The UI shows it and returns the user's answer.
- **Agent text stays plain:** agent-supplied text is shown as plain text, never as Markdown or HTML, and the app has no WebView.
- **No over-the-air code updates:** they could replace the approval screens without an app release.
- **Few Dart packages:** each one is trusted code, so each needs a reason, and the lockfile pins its version.
- **Overlays are blocked:** while a prompt is showing, the Android app hides other apps' overlays with `setHideOverlayWindows(true)` on API 31 and above, and sets `filterTouchesWhenObscured` as a [weaker defence](https://mas.owasp.org/MASTG/best-practices/MASTG-BEST-0040/) for older versions.

## Consequences

- **Good:** there's one set of screens, so prompts look and read the same on both phones.
- **Good:** UniFFI supports Kotlin and Swift officially, so the bindings and platform code would carry over if Flutter had to be replaced.
- **Bad:** the app uses four languages, Dart, Kotlin, Swift and Rust, because the keystores, background work and file pickers stay platform code.
- **Bad:** more code is trusted. A bug or hostile package in the shell could misreport the answer to a prompt. Agents can only reach it through the text it displays, so the main risk is the supply chain.
- **Bad:** the Flutter engine and Dart runtime share a process with Wasmtime, and the [feasibility test](../feasibility-android.md) only covered Android's Java runtime. A separate process is the fallback on Android, but iPhone apps generally can't start their own processes.
- **Bad:** it adds memory and app size, which haven't been measured. Memory on low-end phones was already a concern.
- **Bad:** Flutter draws its own widgets, so accessibility and the readability of prompts need testing with real people.
- **Not solved:** [App Store rules](https://developer.apple.com/app-store/review/guidelines/), Pulley's speed and iPhone's memory and background limits are the same with any UI. As checked on 3 November 2026, guideline 2.5.2 restricts downloaded code, and the exception in 4.7 doesn't name WebAssembly. Whether agents that users install are allowed is still an open question.

This record moves to **Accepted** once Wasmtime has been shown to work next to Flutter on a real Android phone and a real iPhone. Revisit it if they interfere or the memory cost is too high. The fallback is separate Kotlin and SwiftUI apps over the same bindings.
