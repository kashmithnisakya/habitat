#!/usr/bin/env bash
# Builds the Rust core, its Kotlin bindings and the example agent for the Android app. Gradle runs this.
set -euo pipefail

NDK="${1:?Usage: build-android-core.sh <ndk-dir> <min-sdk> <output-dir>}"
MIN_SDK="${2:?Usage: build-android-core.sh <ndk-dir> <min-sdk> <output-dir>}"
OUT="${3:?Usage: build-android-core.sh <ndk-dir> <min-sdk> <output-dir>}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET="aarch64-linux-android"
AGENT_TARGET="wasm32-unknown-unknown"
export PATH="$HOME/.cargo/bin:$PATH"

for needed in "$TARGET" "$AGENT_TARGET"; do
    if ! rustup target list --installed | grep -qx "$needed"; then
        echo "Missing a Rust target. Run: rustup target add $needed" >&2
        exit 1
    fi
done

case "$(uname -s)" in
    Darwin) HOST="darwin-x86_64" ;;
    Linux) HOST="linux-x86_64" ;;
    *) echo "Unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
if [ ! -d "$BIN" ]; then
    echo "The Android NDK's tools aren't at $BIN" >&2
    exit 1
fi

export CC_aarch64_linux_android="$BIN/${TARGET}${MIN_SDK}-clang"
export AR_aarch64_linux_android="$BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
# Android 15 and later support devices with 16 KB memory pages.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"

cd "$ROOT"
cargo build --locked --release -p habitat-mobile --target "$TARGET"
LIBRARY="target/$TARGET/release/libhabitat_mobile.so"

rm -rf "$OUT"
mkdir -p "$OUT/jniLibs/arm64-v8a" "$OUT/kotlin" "$OUT/assets"
cp "$LIBRARY" "$OUT/jniLibs/arm64-v8a/"

cargo run --locked -q -p habitat-mobile --features bindgen --bin uniffi-bindgen -- \
    generate --library "$LIBRARY" --language kotlin --out-dir "$OUT/kotlin" --no-format

cargo build --locked --release -p habitat-hello-agent --target "$AGENT_TARGET"
cargo run --locked -q -p habitat-cli -- pack \
    examples/hello-agent/agent.toml \
    "target/$AGENT_TARGET/release/habitat_hello_agent.wasm" \
    "$OUT/assets/hello-agent.habitat"
