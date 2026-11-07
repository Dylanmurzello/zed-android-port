# Loader in `nativeLibraryDir` via extractNativeLibs

**Status:** Active
**Phase / Commit:** rootfs engine, chunk 0
**Files:** `android/app/build.gradle.kts` (`buildZdLoader`, `verifyZdLoader`, `stageZdLoaderJniLib`, `packaging.jniLibs`), `crates/zd_loader/verify.sh`, `MainActivity.kt::nativeLibraryDir`, `crates/gpui_android/src/runtime_links.rs::native_library_dir`

## Problem

The rootfs engine's loader (`libzdload.so`) is the one binary the kernel
execs for every guest process, and it has to be exec'd by path from a
location SELinux lets an untrusted app execute from at any targetSdk. That
location is the APK's extracted native library directory (`apk_data_file`),
not the app's files dir.

## Constraint

AGP 8 packages native libraries uncompressed inside the APK and sets
`extractNativeLibs="false"`, so by default no file exists on disk to exec.
The directory also changes on every install (`/data/app/~~<random>/...`), so
nothing persistent may embed it.

## Solution

- `packaging { jniLibs { useLegacyPackaging = true } }` makes AGP write
  `extractNativeLibs="true"` into the merged manifest; the installer then
  extracts `lib/arm64-v8a/*.so` to `ApplicationInfo.nativeLibraryDir`.
  `keepDebugSymbols += "**/libzdload.so"` keeps AGP's strip task off the
  hand-linked static ELF.
- `buildZdLoader` runs `cargo ndk` in `crates/zd_loader`, `verifyZdLoader`
  runs `verify.sh` (ET_EXEC, AArch64, no PT_INTERP, no PT_DYNAMIC, LOAD at
  `0x5400000000`, 16 KiB alignment, non-executable stack, entry and image
  inside the loader window, under 1 MiB), and `stageZdLoaderJniLib` copies
  the binary to `jniLibs/arm64-v8a/libzdload.so`. `preBuild` depends on it.
- `MainActivity.nativeLibraryDir()` exposes the directory to Rust;
  `runtime_links::loader_path` joins the loader name and boot logs the
  result.

## Why this works

The kernel checks exec permission on the target file's SELinux label.
`apk_data_file` has `rx_file_perms` for every untrusted app domain, so the
loader runs in every app domain. This is what let the targetSdk pin at 28
go (35 since 2026-10-28): every spawn goes through the loader.

## Failure mode if regressed

`libzdload.so` missing from `nativeLibraryDir` (logged at boot as
`engine loader ... exists=false`), or `verifyZdLoader` failing the build when a
flag change produces a PIE or a different image base. A `RUSTFLAGS` value
inherited from the shell replaces the loader's config rustflags (cargo
precedence) and the link dies on `duplicate symbol: _start`; the gradle
task strips the variable for that reason.

## See also

- [Rootfs engine integration](rootfs-engine-integration.md)
