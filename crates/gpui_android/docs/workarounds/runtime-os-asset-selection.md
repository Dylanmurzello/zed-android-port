# Asset selection by runtime OS

**Status:** Active
**Phase / Commit:** rootfs engine, chunk 2
**Files:** `crates/util/src/util.rs::runtime_os`, `crates/dap_adapters/src/{codelldb,go}.rs`, `crates/languages/src/{c,json,python,rust}.rs`, `crates/copilot/src/copilot.rs`, `crates/extension/src/extension_builder.rs`, `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs`, `crates/node_runtime/src/node_runtime.rs`, `crates/prompt_store/src/prompts.rs`, `crates/project/src/{agent_registry_store,agent_server_store}.rs`

## Problem

Zed downloads prebuilt tools (language servers, debug adapters, Node, the
Copilot server, wasi-sdk) and picks the asset by `std::env::consts::OS`,
a compile-time constant from the target triple. The Android build reports
`android`, which no release table knows, so every such site bailed with
"unsupported operating system android" or, worse, a compile-time platform
block silently had no arm for Android.

## Constraint

The editor process really is an Android binary (bionic, GPU, IME), so the
target triple cannot lie. But nothing Zed downloads runs in that process:
every spawned tool is exec'd through the rootfs engine's loader onto the
Ubuntu userland, as a plain Linux process. The right asset is therefore
the `linux` build of the same architecture, and the decision has to be
made at runtime, per site, without touching the triple.

## Solution

`util::runtime_os()` answers the question "which OS do my subprocesses run
on": `linux` on Android, `consts::OS` everywhere else. Every asset-selection
site asks it instead of `consts::OS`: the asset name, the archive type
(`tar.gz` vs `zip`) and the extension host's `current_platform` (so
extensions pick their `linux-aarch64` release from their own tables).
Compile-time platform blocks that cannot call a function (`RustLspAdapter`,
`TyLspAdapter`, `RuffLspAdapter`, `PyreflyLspAdapter`, `WASI_SDK_PLATFORM`)
and the `cfg!` chains that pick an ACP agent's binary build (the registry's
platform key and the binary agent servers) carry
`any(target_os = "linux", target_os = "android")` instead of a duplicated
Android block. The Rust adapter's libc probe (`ldd --version`) runs in the
rootfs too and reports glibc, which selects the `-gnu` asset as on any
glibc Linux.

The assistant prompt context uses the same helper for its `os` field, since
the commands the model writes run in that userland.

## Why not the obvious fix

Patching each site with an `"android" => "linux"` arm was the first pass.
It drifts: upstream adds a new adapter every few weeks, each a fresh
"unsupported os" at runtime that only shows up when someone opens that
language on the tablet. One helper with one name is greppable, and the
upstream merge checklist greps for `consts::OS` after every merge.

## Failure mode

A new upstream site that keeps `consts::OS` fails only on Android, with
"unsupported os: android" in the language server or debug adapter log.
Route it through the helper. A clangd download on arm64 fails on any
Linux (no prebuilt arm64 asset upstream); `apt install clangd` in the
rootfs puts it on the farm and `check_if_user_installed` wins.

## See also

- `UPSTREAM_MERGE.md`, "Asset-selection sites (runtime OS)"
- `rootfs-guest-env.md` for why the spawned tools see a Linux environment
