//! Build-time checks for the Android app crate.

fn main() {
    // Check that the standalone askpass-helper binary has been staged
    // into APK assets. The helper lives in `askpass-helper/` and is
    // built independently via `cargo ndk -t arm64-v8a -P 26 build
    // --release` from that subdir, then copied to assets/. If a
    // contributor forgets that step, ssh password prompts will silently
    // SIGABRT (current_exe() falls back to /system/bin/app_process64,
    // which the app's SELinux domain can't run). Surface the gap loudly at build
    // time instead of waiting for the runtime symptom.
    let askpass_asset = "android/app/src/main/assets/zed-askpass-helper";
    println!("cargo:rerun-if-changed={askpass_asset}");
    if !std::path::Path::new(askpass_asset).is_file() {
        println!(
            "cargo:warning=build.rs: askpass helper missing at {askpass_asset}. \
             Build it via:\n  \
             cd askpass-helper && \\\n    \
             ANDROID_NDK_HOME=$ANDROID_NDK_HOME cargo ndk -t arm64-v8a -P 26 build --release && \\\n    \
             cp target/aarch64-linux-android/release/zed-askpass-helper ../{askpass_asset}\n  \
             Without it, SSH password / passphrase prompts will SIGABRT."
        );
    }
}
