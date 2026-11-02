#!/usr/bin/env bash
# Builds the feasibility APK without Gradle. See README.md for the required tools.
set -euo pipefail

NDK_VERSION="30.0.16248370"
PLATFORM="android-36"
MIN_API="26"
TARGET="aarch64-linux-android"
PACKAGE_PATH="io/github/kashmithnisakya/habitat/feasibility"

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
NDK="$SDK/ndk/$NDK_VERSION"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
BUILD_TOOLS="$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)"
OUT="$HERE/build"

case "$(uname -s)" in
    Darwin) HOST="darwin-x86_64" ;;
    Linux) HOST="linux-x86_64" ;;
    *) echo "Unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"

for path in "$BIN" "$ANDROID_JAR" "$BUILD_TOOLS/aapt2"; do
    if [ ! -e "$path" ]; then
        echo "Missing $path. See README.md." >&2
        exit 1
    fi
done
command -v javac >/dev/null || { echo "javac not found. Set PATH to include JDK 17." >&2; exit 1; }

echo "==> Building Rust library for $TARGET"
export CC_aarch64_linux_android="$BIN/${TARGET}${MIN_API}-clang"
export AR_aarch64_linux_android="$BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
# Android 15 and later support devices with 16 KB memory pages.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"
cargo build --release --locked --target "$TARGET" --manifest-path "$HERE/rust/Cargo.toml"
SO="$HERE/rust/target/$TARGET/release/libhabitat_feasibility.so"

rm -rf "$OUT"
mkdir -p "$OUT/classes" "$OUT/dex" "$OUT/lib/arm64-v8a"

echo "==> Compiling Java"
javac --release 11 -classpath "$ANDROID_JAR" -d "$OUT/classes" \
    "$HERE/app/java/$PACKAGE_PATH"/*.java
"$BUILD_TOOLS/d8" --release --min-api "$MIN_API" --lib "$ANDROID_JAR" \
    --output "$OUT/dex" "$OUT/classes/$PACKAGE_PATH"/*.class

echo "==> Packaging APK"
"$BUILD_TOOLS/aapt2" link -o "$OUT/unaligned.apk" -I "$ANDROID_JAR" \
    --manifest "$HERE/app/AndroidManifest.xml" \
    --min-sdk-version "$MIN_API" --target-sdk-version "${PLATFORM#android-}"
cp "$SO" "$OUT/lib/arm64-v8a/"
(cd "$OUT/dex" && zip -q "$OUT/unaligned.apk" classes.dex)
# The library is stored uncompressed, so Android can load it directly from the APK.
(cd "$OUT" && zip -q -0 "$OUT/unaligned.apk" lib/arm64-v8a/libhabitat_feasibility.so)
"$BUILD_TOOLS/zipalign" -P 16 -f 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"

# A throwaway key: this app is never published. It's kept outside the build folder,
# because Android rejects updates signed with a different key.
KEYSTORE="$HOME/.android/habitat-feasibility-debug.keystore"
if [ ! -f "$KEYSTORE" ]; then
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android \
        -alias debug -keyalg RSA -keysize 2048 -validity 365 \
        -dname "CN=Habitat feasibility debug" 2>/dev/null
fi
"$BUILD_TOOLS/apksigner" sign --ks "$KEYSTORE" --ks-pass pass:android \
    --out "$OUT/habitat-feasibility.apk" "$OUT/aligned.apk"
"$BUILD_TOOLS/apksigner" verify "$OUT/habitat-feasibility.apk"

echo "==> Built $OUT/habitat-feasibility.apk"
ls -lh "$SO" "$OUT/habitat-feasibility.apk"
