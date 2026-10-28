# Contributing to Zdroid

Zdroid is the Android port of [Zed](https://zed.dev). Contributions are welcome; the rules are short.

## Scope

This repo's diff against upstream Zed is the **Android-specific** delta:

- `crates/gpui_android/` (whole crate, our platform backend)
- `crates/gpui_platform/` (cfg-gated arm)
- `crates/zdroid_runtime/` (runtime-adapter trait + chroot / bootstrap impls)
- `crates/onboarding/` (tweaks for the Android runtime picker)
- `crates/zed_android/` (example binary + Android Gradle project)
- `crates/zed/Cargo.toml` (workspace `android` feature flag disabling desktop-only crates)
- Small env-aware-paths additions in `crates/paths/`, `crates/util/`

Anything outside that scope (a bug in `editor`, `project_panel`, `terminal`, the LSP stack, the agent panel, etc.) **reproduces on desktop Zed and should be filed [upstream](https://github.com/zed-industries/zed/issues)**, not here. We won't fix desktop-side bugs here because our patches get periodically rebased on top of upstream and we don't want to carry conflict surface.

## Filing issues

Use the issue templates: **Report a bug (Zdroid)** or **Report a crash (Zdroid)**. Include APK version, runtime adapter, device + Android version, and logcat. Blank issues are also fine if your concern doesn't fit a template.

## PRs

- Build on top of `origin/main`; rebase rather than merge when possible.
- One logical change per commit. Commit messages: short imperative subject + a body explaining *why* the change is necessary (the "what" is in the diff).
- For changes that touch upstream Zed files (the 54 or so we patch): add a workaround writeup under `crates/gpui_android/docs/workarounds/` explaining what the patch is and what constraint rules out the simpler fix. Future-you and future-Claude will thank you.

## Code style

See [`AGENTS.md`](AGENTS.md) for Rust conventions used by this codebase (most of it inherited from upstream Zed: no `unwrap()` on fallible paths, no organizational comments, prefer editing existing files over creating new small ones, etc.).

## Building

The complete recipe lives in [`crates/gpui_android/examples/zed_android/README.md`](crates/gpui_android/examples/zed_android/README.md). The short version:

```sh
cd crates/gpui_android/examples/zed_android
export ANDROID_NDK_HOME=...
export RUSTFLAGS="-C target-feature=+fp16"
cargo ndk --platform 26 -t arm64-v8a -o android/app/src/main/jniLibs build --release
cd android && gradle assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

## Upstream merges

See [`UPSTREAM_MERGE.md`](UPSTREAM_MERGE.md) for the policy. tl;dr: identity files (README, .github/ISSUE_TEMPLATE/, etc.) are pinned to ours via `.gitattributes merge=ours`; everything else uses default 3-way merge.

## License

Same as upstream Zed (see `LICENSE-*` files). Android-side additions inherit the upstream license; nothing in this repo is more permissive than the parent.
