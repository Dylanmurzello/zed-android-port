import java.net.URL
import java.security.MessageDigest
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

// Ubuntu base image bundling.
//
// The rootfs the engine runs is extracted on first launch from an APK
// asset, so the app is complete without a network. `fetchRootfsImage`
// downloads the tarball named in crates/zdroid_runtime/rootfs-image.properties
// into build/rootfs/ and verifies its SHA-256 (the Rust side compiles the
// same file in and writes name and digest to the rootfs sentinel);
// `stageRootfsAsset` copies it to assets/rootfs/image.tgz, which is
// gitignored. The asset is stored uncompressed in the APK (`noCompress
// tgz` below): it is gzip already, and AAssetManager serves a stored
// asset by mmap.
val workspaceRoot = file("../../../../../..").canonicalFile
val rootfsImageProps = Properties().apply {
    file("${workspaceRoot}/crates/zdroid_runtime/rootfs-image.properties").inputStream().use { load(it) }
}
val rootfsImageName: String = rootfsImageProps.getProperty("name")
val rootfsImageUrl: String = rootfsImageProps.getProperty("url")
val rootfsImageSha256: String = rootfsImageProps.getProperty("sha256").lowercase()
val rootfsImageDownload = file("build/rootfs/${rootfsImageName}")
// Staged under a fixed name without a `.gz` suffix: aapt gunzips `.gz`
// assets at packaging time and drops the suffix, which would turn the
// 33 MB image into a 120 MB tar.
val rootfsImageAsset = file("src/main/assets/rootfs/image.tgz")

fun sha256Of(file: File): String {
    val digest = MessageDigest.getInstance("SHA-256")
    file.inputStream().use { input ->
        val buffer = ByteArray(1 shl 16)
        while (true) {
            val count = input.read(buffer)
            if (count < 0) break
            digest.update(buffer, 0, count)
        }
    }
    return digest.digest().joinToString("") { "%02x".format(it) }
}

tasks.register("fetchRootfsImage") {
    description = "Download the Ubuntu base image named in rootfs-image.properties and verify its SHA-256."
    group = "build setup"

    inputs.property("sha256", rootfsImageSha256)
    inputs.property("url", rootfsImageUrl)
    outputs.file(rootfsImageDownload)
    doLast {
        if (rootfsImageDownload.isFile && sha256Of(rootfsImageDownload) == rootfsImageSha256) {
            return@doLast
        }
        rootfsImageDownload.parentFile.mkdirs()
        val partial = File(rootfsImageDownload.path + ".part")
        logger.lifecycle("fetchRootfsImage: downloading $rootfsImageUrl")
        URL(rootfsImageUrl).openStream().use { input ->
            partial.outputStream().use { output -> input.copyTo(output) }
        }
        val actual = sha256Of(partial)
        if (actual != rootfsImageSha256) {
            partial.delete()
            throw GradleException(
                "fetchRootfsImage: SHA-256 mismatch for $rootfsImageName: expected $rootfsImageSha256, got $actual"
            )
        }
        if (!partial.renameTo(rootfsImageDownload)) {
            throw GradleException("fetchRootfsImage: cannot move ${partial.path} into place")
        }
    }
}

tasks.register<Copy>("stageRootfsAsset") {
    description = "Copy the verified Ubuntu base image into assets/rootfs/ for APK packaging."
    group = "build setup"

    dependsOn("fetchRootfsImage")
    from(rootfsImageDownload)
    into(rootfsImageAsset.parentFile)
    rename { rootfsImageAsset.name }
    inputs.file(rootfsImageDownload)
    outputs.file(rootfsImageAsset)
}

tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn("stageRootfsAsset")
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
    // The app build exports RUSTFLAGS (+fp16 for gemm-f16). Cargo lets an
    // environment RUSTFLAGS replace the target rustflags in a crate's
    // .cargo/config.toml, which for the loader means losing the static,
    // fixed-base, no-crt link: crt1.o then defines a second `_start` and
    // lld fails with a duplicate symbol. Strip the inherited flags so the
    // loader's own config governs.
    environment.remove("RUSTFLAGS")
    environment.remove("CARGO_ENCODED_RUSTFLAGS")

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
        // targetSdk 35. The old pin at 28 kept the app in a SELinux domain
        // that may exec app-private files; the rootfs engine made that
        // moot (the kernel only execs the loader in the native library
        // dir, and the Ubuntu binaries are mapped, not exec'd). What the
        // level buys: no "built for an older version" install warning, a
        // real notification permission prompt (33+), and the foreground
        // service type declared here is enforced (34+). What it costs:
        // raw /sdcard paths need All files access (MANAGE_EXTERNAL_STORAGE)
        // on Android 11+; see AndroidManifest.xml.
        targetSdk = 35
        versionCode = 20
        versionName = "0.4.0"
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

    // The bundled rootfs image is gzip already; storing it lets the
    // installer stream it straight out of the APK.
    androidResources {
        noCompress += listOf("tgz")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
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
