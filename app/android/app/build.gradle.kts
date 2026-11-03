plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// The lowest Android version that Wasmtime has been tested on, in the feasibility test.
val coreMinSdk = 26
val coreOutput = layout.buildDirectory.dir("generated/habitat-core")

android {
    namespace = "io.github.kashmithnisakya.habitat"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        // TODO: Specify your own unique Application ID (https://developer.android.com/studio/build/application-id.html).
        applicationId = "io.github.kashmithnisakya.habitat"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        minSdk = coreMinSdk
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    buildTypes {
        release {
            // TODO: Add your own signing config for the release build.
            // Signing with the debug keys for now, so `flutter run --release` works.
            signingConfig = signingConfigs.getByName("debug")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    // The core is only built for 64-bit ARM, so the app mustn't install anywhere it would fail on every call.
    packaging {
        jniLibs {
            excludes += setOf("lib/armeabi-v7a/**", "lib/x86/**", "lib/x86_64/**")
        }
    }

    sourceSets.getByName("main") {
        jniLibs.srcDir(coreOutput.get().dir("jniLibs").asFile)
        assets.srcDir(coreOutput.get().dir("assets").asFile)
        kotlin.srcDir(coreOutput.get().dir("kotlin").asFile)
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")
}

val buildCore by tasks.registering(Exec::class) {
    commandLine(
        "bash",
        file("../../scripts/build-android-core.sh"),
        android.ndkDirectory,
        coreMinSdk,
        coreOutput.get().asFile,
    )
}

tasks.named("preBuild") {
    dependsOn(buildCore)
}

flutter {
    source = "../.."
}
