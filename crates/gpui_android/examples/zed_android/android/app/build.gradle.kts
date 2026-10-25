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

// Bootstrap-zip distribution.
//
// Pre-Phase-6: the APK bundled bootstrap-aarch64.zip (~239 MB) as an
// asset, downloaded by a `downloadBootstrap` Gradle task from a
// GitHub release at build time, then extracted at first launch.
//
// Phase 6 moved the download to runtime, owned by
// `BootstrapAdapter::install` (see `crates/zdroid_runtime/src/
// adapters/bootstrap_install.rs`). The APK no longer bundles the
// zip; the Bootstrap adapter pulls it directly from
// `https://github.com/<release_repo>/releases/latest` when the user
// selects Bootstrap and the on-disk version sentinel doesn't match.
//
// Chroot users (Magisk + zd-spawnd module) never trigger the
// bootstrap install. Saves ~239 MB of fat for the majority case.

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

    // Don't deflate bootstrap-aarch64.zip during APK packaging — it's
    // already a deflated zip, and re-deflating it (a) wastes APK size
    // (b) forces AAssetManager to decompress at runtime, which prevents
    // the bootstrap extractor from using the mmap-able buffer path.
    androidResources {
        noCompress += listOf("zip")
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
