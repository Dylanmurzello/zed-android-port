# Build for the reference hardware's CPU

**Status:** Active
**Phase / Commit:** Zdroid 0.8.5
**Files:** `crates/gpui_android/examples/zed_android/.cargo/config.toml`

## Problem

The `aarch64-linux-android` target compiles for the baseline Armv8.0
instruction set: rustc enables only NEON, and the `cc` crate passes clang no
CPU flags. Every atomic read-modify-write then becomes a load-exclusive /
store-exclusive retry loop (`ldaxr` … `stlxr`), and every acquire load an
`ldar`. Zed does both constantly: `Arc` reference counts, locks, channels.
The reference hardware's CPU has single-instruction atomics (LSE, such as
`ldaddal`) and the cheaper acquire load `ldapr`, and the compiler can tune
instruction scheduling for its cores.

## Constraint

The features the reference hardware's kernel reports
(`grep -m1 Features /proc/cpuinfo`, Android 16 with One UI 8, 2026-10-26):

```
fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp sha512 asimdfhm dit uscat ilrcpc flagm sb paca pacg dcpodp flagm2 frint i8mm bf16 bti
```

Its Snapdragon 8 Gen 2 cores (Cortex-X3, A715, A710, A510) implement SVE2,
MTE and SSBS, but the kernel doesn't enable them, so one SVE instruction
kills the app with SIGILL. The obvious flags turn them on:

- `rustc -C target-cpu=cortex-x3` enables `sve`, `sve2`, `mte` and `ssbs`.
- `clang -mcpu=cortex-x3` turned a plain integer loop into 10 SVE
  instructions. Adding `+nosve+nosve2` stops that, but NDK r27's clang still
  defines `__ARM_FEATURE_SVE2`, and it rejects `+nossbs`.

## Solution

Rust, in `rustflags`:

```
-C target-cpu=cortex-x3 -C target-feature=-sve,-sve2,-mte,-ssbs,+aes,+sha2
```

That's the core's tuning and the features the kernel reports, except SHA-3
and SM4. Other Armv9 chips may lack those, Zed doesn't use them, and C
crypto code such as aws-lc can skip its runtime checks for features that
are enabled at compile time.

rustc's list also keeps `lor`, `pan`, `pmuv3`, `ras`, `spe` and `vh`, which
only cover system registers that app code never touches.

C and C++, through `CFLAGS_aarch64_linux_android` and
`CXXFLAGS_aarch64_linux_android` in `[env]`, which `cc` reads:

```
-march=armv8.5-a+fp16+fp16fml+i8mm+bf16+aes+sha2 -mtune=cortex-x3
```

The same instruction set without SVE or MTE, tuned for the core. `cc`
doesn't forward rustc's `target-cpu`, so without this the tree-sitter
grammars, SQLite and the other C code stay at Armv8.0.

The config applies to everything built in the app's directory, including
the askpass helper. The engine loader builds in its own workspace,
`crates/zd_loader`, with its own flags, and stays at the baseline.

## Devices that can run it

The flags need Armv8.5 plus fp16, fp16fml, i8mm, bf16, AES and SHA-2, which
the Armv9 chips in phones and tablets have. So these should run release
builds, though only the reference hardware is tested: Snapdragon 8 Gen 1,
7+ Gen 2 and later, Dimensity 9000 and later, Exynos 2200 and later, and
Tensor G3 and later. Older CPUs can't, including some high-end devices
from 2022–2023 with Tensor G2 (Pixel 7, Pixel Fold, Pixel Tablet). Neither
can mid-range chips on Cortex-A78 and A55 cores.

## Verification

A full local `cargo ndk -t arm64-v8a -P 26 build --release` in the CI image,
compared with the library in the published 0.8.4 APK through
`llvm-objdump -d --no-show-raw-insn`:

| | 0.8.4 | This build |
|---|---|---|
| Instructions | 22,419,287 | 22,342,181 |
| LSE atomics (`ldadd`, `cas`, `swp`, …) | 4 | 99,755 |
| Load/store-exclusive (`ldaxr`, `stlxr`, …) | 209,031 | 377 |
| `ldapr` / `ldar` | 0 / 11,721 | 9,809 / 1,671 |
| SVE-looking / MTE-looking | 2 / 1 | 2 / 1 |
| `libzed_android.so` | 140,571,520 bytes | 140,893,712 bytes |

The two SVE-looking hits are the same bytes in both libraries, right after a
`ret` and beside undecodable words: data, not code. The MTE-looking hit is
the same `stg` loop in both, which 0.8.4 already contains and runs fine with.
The build took 15m36s and 11.5 GB of peak memory on 16 cores, as before.
This was measured with SHA-3 and SM4 still enabled; turning features off
can only remove instructions.

## Failure mode if regressed

- A flag that turns SVE back on crashes the app with SIGILL (signal 4) on
  the reference hardware, possibly only once the vectorized code path runs.
  Scan the library after changing these flags: in `llvm-objdump -d
  --no-show-raw-insn libzed_android.so`, nothing beyond the data bytes and
  the `stg` loop described under Verification may use `z<n>.<size>` or
  `p<n>/z` operands or MTE instructions.
- Devices whose CPUs lack these features crash with SIGILL at launch (see
  "Devices that can run it"). To support them again, remove the
  `target-cpu` and `target-feature` lines and the `[env]` section, and
  put back `-C target-feature=+fp16`.

## See also

- [Reference hardware](README.md#reference-hardware)
