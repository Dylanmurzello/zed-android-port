# Build for Armv8.2 CPUs

**Status:** Active
**Phase / Commit:** Zdroid 0.8.5 (built for the reference tablet's Armv9 cores), 0.15.0 (Armv8.2)
**Files:** `crates/gpui_android/examples/zed_android/.cargo/config.toml`

The file keeps its name from 0.8.5, when the build targeted the reference
hardware's CPU, because release notes since then link to it.

## Problem

The `aarch64-linux-android` target compiles for the baseline Armv8.0
instruction set: rustc enables only NEON, and the `cc` crate passes clang no
CPU flags. Every atomic read-modify-write then becomes a load-exclusive /
store-exclusive retry loop (`ldaxr` … `stlxr`), and every acquire load an
`ldar`. Zed does both constantly: `Arc` reference counts, locks, channels.
Armv8.1 added single-instruction atomics (LSE, such as `ldaddal`), and RCpc
the cheaper acquire load `ldapr`.

From 0.8.5 to 0.14.1 the build targeted the reference tablet's cores
(`target-cpu=cortex-x3` without SVE, SVE2, MTE and SSBS, and C at
`armv8.5-a+fp16+fp16fml+i8mm+bf16`). That crashed at launch on every CPU
without those Armv8.5 and Armv9 features, such as the second test device's.

## Constraint

The features each test device's kernel reports, the same on every core
(`grep Features /proc/cpuinfo`):

- Reference tablet, Android 16 with One UI 8 (2026-10-26):
  ```
  fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp sha512 asimdfhm dit uscat ilrcpc flagm sb paca pacg dcpodp flagm2 frint i8mm bf16 bti
  ```
- Galaxy S21 Ultra (Exynos 2100), Android 15 with One UI 7 (2026-11-05):
  ```
  fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp
  ```

The phone's list is Armv8.2 with FP16, RCpc (`lrcpc`) and dot product
(`asimddp`), and the tablet's contains all of it. Neither reports SSBS. The
tablet's cores also implement SVE2 and MTE, which its kernel doesn't enable:
`rustc -C target-cpu=cortex-x3` turns them on, and `clang -mcpu=cortex-x3`
turned a plain integer loop into SVE instructions, which crash with SIGILL
there.

## Solution

Rust, in `rustflags`:

```
-C target-cpu=cortex-x1 -C target-feature=-ssbs
```

rustc's `cortex-x1` enables `aes crc dotprod dpb fp16 lse rcpc rdm sha2` and
`ssbs` (`rustc --print cfg --target aarch64-linux-android -C
target-cpu=cortex-x1`), which is the phone's list plus SSBS, so SSBS is
turned back off. It also keeps `lor`, `pan`, `pmuv3`, `ras`, `spe` and `vh`,
which only cover system registers that app code never touches. SHA-3,
SHA-512 and SM4 stay off: C crypto code such as aws-lc then checks for them
at runtime instead of assuming them.

C and C++, through `CFLAGS_aarch64_linux_android` and
`CXXFLAGS_aarch64_linux_android` in `[env]`, which `cc` reads:

```
-march=armv8.2-a+fp16+rcpc+dotprod+aes+sha2 -mtune=cortex-x3
```

The same instruction set, tuned for the reference tablet's prime core. `cc`
doesn't forward rustc's `target-cpu`, so without this the tree-sitter
grammars, SQLite and the other C code stay at Armv8.0.

The config applies to everything built in the app's directory, including
the askpass helper. The engine loader builds in its own workspace,
`crates/zd_loader`, with its own flags, and stays at the baseline.

## What the Armv9 build had over this one

Compared with 0.14.1's flags, this build gives up `bf16`, `bti`, `dit`,
`dpb2`, `fcma`, `fhm`, `flagm`, `frintts`, `i8mm`, `jsconv`, `paca`, `pacg`,
`rcpc2` and `sb`, and Rust's tuning moves from the X3 to the X1. On the
reference tablet, the same session (an SSH project with the same tabs,
landscape, Battery Saver off, no thermal throttling, hottest sensor 52.5 °C
each round) measured, in this order:

| | 0.14.1 | Armv8.2 build | 0.14.1 again |
|---|---|---|---|
| Scrolling over the same lines: main loop CPU per frame | 11.3 ms (10.4–12.4) | 12.3 ms (11.6–12.9) | 12.6 ms (12.2–12.9) |
| Scrolling through new lines: main loop CPU per frame | 13.0 ms (12.6–13.3) | 12.8 ms (12.7–12.9) | 12.9 ms (12.6–13.1) |
| Scrolling over the same lines: time between frames | 15.8 ms | 16.5 ms | 17.3 ms |
| Scrolling through new lines: time between frames | 17.3 ms | 17.2 ms | 18.0 ms |
| Cold start, `android_main` entry to the event loop | 113 ms (105–117) | 117 ms (113–122) | 111 ms (104–117) |
| Cold start, `android_main` entry to the first frame | 407 ms | 406 ms | 377 ms |

Scrolling used scripted `input swipe`s over a long file, 3 runs in the first
round and 5 in the others, with CPU time from `simpleperf` samples of the
`android_main` thread and frame times from `dumpsys SurfaceFlinger
--latency` (the recipes under "Debugging on the tablet" in `FORK.md`). Cold
starts were 5 `am force-stop` and launch cycles each, timed from the app's
log. Every scrolling number of the Armv8.2 build falls between the two
0.14.1 rounds or next to them; only the cold start's setup was slower in
every run, by about 5 ms.

## Devices that can run it

Every core needs Armv8.2 with FP16, RCpc, dot product, AES and SHA-2, which
Arm's Cortex-A55, A75 and later cores have, plus Android 12 (minSdk 31).
Tested on the reference tablet and the Galaxy S21 Ultra (Exynos 2100). The
Snapdragon 8 series from the 845 on and every Tensor use such cores. CPUs
with older cores, such as Cortex-A53 or A73, crash at launch, and Samsung's
own Mongoose cores (Exynos 9810 to 990) haven't been checked. A kernel older
than Linux 5.9 also needed a loader change; see
`close-range-before-linux-5-9.md`.

## Verification

`llvm-objdump -d --no-show-raw-insn --mattr=+all` of `libzed_android.so`,
counting the instructions of each extension the phone's list lacks:

| | 0.8.4 (Armv8.0) | 0.14.1 (Armv9) | Armv8.2 build |
|---|---|---|---|
| Instructions | 22,419,287 | 24,320,141 | 24,634,237 |
| LSE atomics (`ldadd`, `cas`, `swp`, …) | 4 | 106,751 | 107,004 |
| Load/store-exclusive (`ldaxr`, `stlxr`, …) | 209,031 | 377 | 377 |
| `ldapr` | 0 | 10,340 | 10,342 |
| RCpc2 (`ldapur`, `stlur`, …) | | 4,337 | 4 |
| SHA-3, SHA-512, SM3, SM4 | | 589 | 589 |
| SVE-looking / MTE-looking | 2 / 1 | 2 / 1 | 2 / 1 |
| Other Armv8.3+ (`fjcvtzs`, `fcmla`, `frint32x`, bf16, i8mm, `fmlal`, flag manipulation, `sb`, non-hint pointer authentication) | | 0 | 0 |
| `libzed_android.so` | 140,571,520 bytes | 150,400,704 bytes | 151,801,024 bytes |

The 0.8.4 column is from 0.8.5's measurement; the other two were scanned on
2026-11-06. Of what the Armv9 flags allowed, 0.14.1 used only RCpc2's
acquire loads and release stores with an address offset (4,333 besides the
4 text hits below); without them, the address is computed by a separate
instruction. The Armv8.2 build has 314,096 more instructions (1.3%) and a
0.9% larger library. In both, 4 RCpc2 hits are text: the words
around them spell "CRYPTOGAMS", the credit OpenSSL-derived assembly keeps in
its code section. The SHA-3, SHA-512 and SM hits are in that same
OpenSSL-derived assembly, which picks those instructions only after checking
the CPU at runtime; on the phone, the editor's HTTPS requests, such as the
update check, ran without a crash. The two SVE-looking hits are the same
data bytes 0.8.4 has, and the MTE-looking hit the same `stg` loop. The
askpass helper, a static executable, has 31 MTE instructions in both builds:
they're the Scudo allocator from the NDK's static libc, which uses memory
tagging only when the kernel enables it.

## Failure mode if regressed

- Devices whose CPUs lack these features crash with SIGILL (signal 4) at
  launch, or once the code path runs. Scan the library after changing these
  flags as under Verification: beyond the hits explained there, no
  instruction may come from an extension a supported device's list lacks.
- A flag that turns SVE back on crashes the reference tablet the same way:
  nothing may use `z<n>.<size>` or `p<n>/z` operands.

## See also

- [Reference hardware](README.md#reference-hardware)
- `close-range-before-linux-5-9.md`: the loader change older kernels need
