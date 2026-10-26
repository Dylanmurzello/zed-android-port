import java.util.Properties

plugins {
    id("com.android.application")
    kotlin("android")
}

// Release signing config. `signing.properties` and `release.keystore` are
// gitignored: contributors who clone the repo can still build the debug
// variant, but a release build that produces a signed APK requires the
// keystore + properties to be present locally (or provided by CI via
// env-var injected secrets, future work).
val signingPropsFile = file("signing.properties")
val signingProps = Properties().apply {
    if (signingPropsFile.exists()) {
        signingPropsFile.inputStream().use { load(it) }
    }
}
val hasReleaseSigning = signingPropsFile.exists()

// zd-exec bundling.
//
// `zd-exec` is the Rust spawn wrapper (in crates/zdroid_runtime/src/
// bin/zd-exec.rs) the editor invokes as `terminal.shell` for chroot
// mode, and as the symlink target for `$PREFIX/zd-runtime/<name>`
// in chroot+other modes. It MUST be in the APK so fresh installs
// have it. Without bundling, end users hit
// `failed to spawn $PREFIX/bin/zd-exec — no such file or directory`
// the first time they open the integrated terminal in chroot mode.
//
// Build flow:
//   1. `buildZdExec` runs `cargo ndk … build --release -p
//      zdroid_runtime --bin zd-exec` from the workspace root, with
//      $ANDROID_NDK_HOME pointed at the same NDK the lib build uses.
//   2. The resulting ELF at
//      `target/aarch64-linux-android/release/zd-exec` is copied to
//      `app/src/main/assets/zd-exec`.
//   3. `preBuild` depends on it, so gradle picks the asset up during
//      `mergeAssets` before APK packaging.
//
// Rust-side counterpart: `gpui_android::zd_exec_install::ensure_installed`
// reads this asset at boot and extracts to `$PREFIX/bin/zd-exec` with
// 0755 perms when missing or out of date.
val workspaceRoot = file("../../../../../..").canonicalFile
val zdExecBin = file("${workspaceRoot}/target/aarch64-linux-android/release/zd-exec")
val zdExecAsset = file("src/main/assets/zd-exec")
val zdExecSrc = fileTree("${workspaceRoot}/crates/zdroid_runtime/src") {
    include("**/*.rs")
}

tasks.register<Exec>("buildZdExec") {
    description = "Build zd-exec via cargo-ndk and stage into assets/."
    group = "build setup"

    workingDir(workspaceRoot)
    commandLine(
        "cargo",
        "ndk",
        "-t",
        "arm64-v8a",
        "-P",
        "26",
        "build",
        "--release",
        "-p",
        "zdroid_runtime",
        "--bin",
        "zd-exec",
    )

    // Honor whatever the developer's `cargo ndk` lib build uses.
    // ANDROID_NDK_HOME has to be the same NDK or the prebuilts mismatch.
    providers.environmentVariable("ANDROID_NDK_HOME").orNull?.let { ndk ->
        environment("ANDROID_NDK_HOME", ndk)
    }

    inputs.files(zdExecSrc)
    inputs.file("${workspaceRoot}/crates/zdroid_runtime/Cargo.toml")
    outputs.file(zdExecBin)
}

tasks.register<Copy>("stageZdExecAsset") {
    description = "Copy the freshly-built zd-exec into assets/ for APK packaging."
    group = "build setup"

    dependsOn("buildZdExec")
    from(zdExecBin)
    into(zdExecAsset.parentFile)
    rename { zdExecAsset.name }
    // Don't bother re-running on every gradle invocation when the
    // source binary hasn't changed.
    inputs.file(zdExecBin)
    outputs.file(zdExecAsset)
}

tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn("stageZdExecAsset")
}

// zed-askpass-helper bundling.
//
// `zed-askpass-helper` is the SSH_ASKPASS relay binary that ssh
// (running inside the Kali chroot) exec's to prompt the user for a
// password via Zed's in-process modal. The helper MUST be statically
// linked: ssh inside the chroot can't resolve bionic's dynamic linker
// (`/system/bin/linker64` lives outside the chroot fs), so a
// dynamically-linked build fails exec with the shell's misleading
// "not found".
//
// The helper crate at `examples/zed_android/askpass-helper/` is its
// own workspace and ships a `.cargo/config.toml` setting
// `target-feature=+crt-static` for `aarch64-linux-android`. By running
// `cargo ndk` with `workingDir` set to that crate's directory, gradle
// guarantees the static-link config is in scope for every APK build
// — no chance of a contributor accidentally shipping a dynamic
// binary by building from the wrong cwd or hand-editing the asset.
val askpassHelperDir = file("${workspaceRoot}/crates/gpui_android/examples/zed_android/askpass-helper")
val askpassHelperBin = file("${askpassHelperDir}/target/aarch64-linux-android/release/zed-askpass-helper")
val askpassHelperAsset = file("src/main/assets/zed-askpass-helper")
val askpassHelperSrc = fileTree("${askpassHelperDir}/src") {
    include("**/*.rs")
}

tasks.register<Exec>("buildAskpassHelper") {
    description = "Build zed-askpass-helper (static aarch64) via cargo-ndk and stage into assets/."
    group = "build setup"

    workingDir(askpassHelperDir)
    commandLine(
        "cargo",
        "ndk",
        "-t",
        "arm64-v8a",
        "-P",
        "26",
        "build",
        "--release",
    )

    providers.environmentVariable("ANDROID_NDK_HOME").orNull?.let { ndk ->
        environment("ANDROID_NDK_HOME", ndk)
    }

    inputs.files(askpassHelperSrc)
    inputs.file("${askpassHelperDir}/Cargo.toml")
    inputs.file("${askpassHelperDir}/.cargo/config.toml")
    outputs.file(askpassHelperBin)
}

tasks.register<Copy>("stageAskpassHelperAsset") {
    description = "Copy the freshly-built zed-askpass-helper into assets/ for APK packaging."
    group = "build setup"

    dependsOn("buildAskpassHelper")
    from(askpassHelperBin)
    into(askpassHelperAsset.parentFile)
    rename { askpassHelperAsset.name }
    inputs.file(askpassHelperBin)
    outputs.file(askpassHelperAsset)
}

tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn("stageAskpassHelperAsset")
}

// Rootfs engine loader.
//
// `libzdload.so` is a static, libc-free aarch64 executable (crates/
// zd_loader, its own workspace) that the kernel execs for every guest
// process. It is packaged as a native library so the installer extracts
// it to ApplicationInfo.nativeLibraryDir, the one app-owned location an
// untrusted app may exec from at any targetSdk. `verifyZdLoader` fails
// the build if a flag change produces a PIE, a dynamic interpreter, or a
// different image base, because the engine's seccomp filter only allows
// syscalls from the fixed window the loader is linked into.
val zdLoaderDir = file("${workspaceRoot}/crates/zd_loader")
val zdEngineCoreDir = file("${workspaceRoot}/crates/zd_engine_core")
val zdLoaderBin = file("${zdLoaderDir}/target/aarch64-linux-android/release/zd_loader")
val zdLoaderJniLib = file("src/main/jniLibs/arm64-v8a/libzdload.so")
val ndkHomeForVerify = providers.environmentVariable("ANDROID_NDK_HOME")

tasks.register<Exec>("buildZdLoader") {
    description = "Build the static rootfs-engine loader via cargo-ndk."
    group = "build setup"

    workingDir(zdLoaderDir)
    commandLine(
        "cargo",
        "ndk",
        "-t",
        "arm64-v8a",
        "-P",
        "26",
        "build",
        "--release",
    )

    providers.environmentVariable("ANDROID_NDK_HOME").orNull?.let { ndk ->
        environment("ANDROID_NDK_HOME", ndk)
    }

    inputs.files(fileTree("${zdLoaderDir}/src") { include("**/*.rs") })
    inputs.files(fileTree("${zdEngineCoreDir}/src") { include("**/*.rs") })
    inputs.file("${zdLoaderDir}/Cargo.toml")
    inputs.file("${zdLoaderDir}/.cargo/config.toml")
    inputs.file("${zdEngineCoreDir}/Cargo.toml")
    outputs.file(zdLoaderBin)
}

tasks.register<Exec>("verifyZdLoader") {
    description = "Check the loader ELF is static, non-PIE, 16 KiB aligned and linked at the engine's fixed base."
    group = "verification"

    dependsOn("buildZdLoader")
    doFirst {
        if (!ndkHomeForVerify.isPresent) {
            throw GradleException("verifyZdLoader needs ANDROID_NDK_HOME (the same NDK as the lib build)")
        }
    }
    commandLine("sh", "${zdLoaderDir}/verify.sh", ndkHomeForVerify.getOrElse(""), zdLoaderBin.path)
    inputs.file(zdLoaderBin)
}

tasks.register<Copy>("stageZdLoaderJniLib") {
    description = "Copy the verified loader into jniLibs as libzdload.so."
    group = "build setup"

    dependsOn("verifyZdLoader")
    from(zdLoaderBin)
    into(zdLoaderJniLib.parentFile)
    rename { zdLoaderJniLib.name }
    inputs.file(zdLoaderBin)
    outputs.file(zdLoaderJniLib)
}

tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn("stageZdLoaderJniLib")
}

android {
    namespace = "com.zdroid"
    compileSdk = 35

    // Pin the NDK explicitly so reproducibility doesn't depend on whatever
    // `sdkmanager --list_installed` happens to surface. Bionic's
    // `forkpty()` is in API 23+, so any NDK ≥ r21 is sufficient; we use r27
    // because that's the one we shipped L1 with and `+fp16` codegen
    // (gemm-f16) wants a recent toolchain.
    ndkVersion = "27.0.12077973"

    defaultConfig {
        applicationId = "com.zdroid"
        // minSdk = 26 enforces bionic ≥ Oreo. `forkpty()` is on the symbol
        // table from API 23, but cpal/livekit transitive crates require
        // libaaudio which is API 26.
        minSdk = 26
        // targetSdk = 28 is the linchpin of the bundled Termux runtime:
        // SELinux puts us in the `untrusted_app_27` domain where
        // `execute_no_trans` on `app_data_file` is permitted, so we can
        // execve $PREFIX/bin/* directly. Pinning > 28 lands in
        // `untrusted_app_all` / numbered higher domains where exec is
        // denied — the entire L2 plan stops working. Skipping Play Store
        // eligibility is the explicit trade.
        targetSdk = 28
        versionCode = 18
        versionName = "0.3.3"
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }

    // The loader must exist as a real file on disk: the kernel execs it
    // by path from ApplicationInfo.nativeLibraryDir. AGP 8 keeps native
    // libs uncompressed inside the APK by default (extractNativeLibs =
    // false), so opt back into extraction; this DSL is what writes the
    // manifest attribute. keepDebugSymbols keeps the strip task off the
    // hand-linked static ELF.
    packaging {
        jniLibs {
            useLegacyPackaging = true
            keepDebugSymbols += "**/libzdload.so"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // `targetSdk = 28` is intentional (see the comment in defaultConfig). It
    // pins us in the SELinux `untrusted_app_27` domain so the bundled Termux
    // runtime can `execve` $PREFIX/bin/*. AGP's `lintVitalRelease` task
    // flags this as `ExpiredTargetSdkVersion` and refuses to assemble the
    // release APK. We're not Play-Store eligible by design — disable that
    // single rule rather than bumping the SDK and breaking exec.
    lint {
        disable += "ExpiredTargetSdkVersion"
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(signingProps.getProperty("storeFile"))
                storePassword = signingProps.getProperty("storePassword")
                keyAlias = signingProps.getProperty("keyAlias")
                keyPassword = signingProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
        getByName("release") {
            isMinifyEnabled = false
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
}

dependencies {
    implementation("androidx.games:games-activity:3.0.5")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.core:core-ktx:1.13.1")
    // ActivityResultLauncher / ActivityResultContracts for SAF picker.
    implementation("androidx.activity:activity-ktx:1.9.3")
}
