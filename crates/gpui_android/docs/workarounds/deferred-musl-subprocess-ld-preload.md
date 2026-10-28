# LD_PRELOAD strip propagates to musl-static binary's children

**Status:** Deferred. Workaround for users today is "invoke `node` (or the target interpreter) directly, bypassing the shebang." Long-term fix is wrapper-shell-with-LD_PRELOAD-restoration; not in v0.1.x scope.

## What breaks

The `$PREFIX/bin` wrapper the bootstrap's launcher generator (`etc/apt/zed-launcher-gen.sh`) writes for musl binaries (Claude Code, anything Bun-compiled) looks like:

```sh
#!/data/data/com.zdroid/files/usr/bin/sh
exec env -u LD_PRELOAD "/data/data/com.zdroid/.../claude" "$@"
```

The `env -u LD_PRELOAD` is non-negotiable for the musl process itself: musl `ld-musl-aarch64.so.1` will try to `dlopen` whatever `LD_PRELOAD` names. Our `libtermux-exec.so` is bionic-linked (depends on `__system_property_get`, `__register_atfork`, FORTIFY `_chk` symbols musl doesn't provide). Loading it into a musl process aborts before claude's first instruction. Strip mandatory.

But that strip propagates: when claude spawns children via `execve(...)`, it passes through its own (LD_PRELOAD-less) environment. Children include bash, which is bionic-linked and would have happily honored libtermux-exec for shebang path-rewriting, but never sees it.

Failure mode in practice: claude does

```javascript
spawn('npx', ['create-next-app', ...])
```

which becomes `bash -c "npx create-next-app ..."`. bash forks, exec's `npx`, kernel reads `npx`'s `#!/usr/bin/env node` shebang, tries `execve("/usr/bin/env")`. Android has no `/usr/bin/env`. Without libtermux-exec to rewrite the shebang path to `$PREFIX/bin/env`, ENOENT is final. claude reports "command failed".

## Workaround for users today

Bypass the shebang by invoking the interpreter directly:

```bash
# Instead of:
npx create-next-app my-app
# Run:
node $PREFIX/lib/node_modules/create-next-app/dist/index.js my-app
```

Or for Python tools:

```bash
# Instead of:
my-script.py
# Run:
$PREFIX/bin/python $PREFIX/bin/my-script.py
```

The interpreter itself is found via `PATH` (which is set correctly), so this works. It's the kernel-level shebang resolution that breaks, not anything inside the spawned process.

claude's "tool use" subprocesses (bash blocks, npm calls) hit this whenever the npm package's bin entries use `#!/usr/bin/env <interpreter>` shebangs. Most do.

## Why the strip is "too coarse"

It's an all-or-nothing decision applied at musl-binary boundary. Three logical cases the wrapper conflates:

1. **The musl process itself** — must have LD_PRELOAD stripped (musl ld would abort).
2. **Children that are bionic-static or musl-static** — don't need LD_PRELOAD restored (they don't honor it anyway).
3. **Children that are bionic-dynamic** (bash, node, npm, sh) — *would* benefit from libtermux-exec being LD_PRELOAD'd.

We can't distinguish (2) from (3) at the wrapper level — claude exec's whatever, we don't know what's at the other end.

## Long-term fix options

### A. Per-shell wrapper that re-exports LD_PRELOAD

Wrap `bash`, `sh`, `dash`, `ash` (anything that runs scripts with shebangs) at `$PREFIX/bin/`. Each wrapper:

```sh
#!$PREFIX/bin/<shell>-real
[ -n "$ZED_LD_PRELOAD" ] && export LD_PRELOAD="$ZED_LD_PRELOAD"
exec $PREFIX/bin/<shell>-real "$@"
```

The musl wrapper would then carry the original LD_PRELOAD value in `ZED_LD_PRELOAD` instead of stripping it:

```sh
exec env -u LD_PRELOAD "ZED_LD_PRELOAD=$LD_PRELOAD" "$bin" "$@"
```

Children that go through our wrapped shells re-discover LD_PRELOAD; direct execves of non-shell binaries don't. Costs: extra fork/exec per shell invocation, several small wrapper scripts, and the wrappers need to coexist with apt-installed `pkg install bash` which would clobber them (handle with a dpkg `path-exclude`).

### B. Compile a musl-flavored libtermux-exec

A second `.so` linked against musl that exports the same shebang-rewrite hook. The musl process can load it without symbol failures. Children inherit the musl-ld'd version, which honors it.

But bionic-linked children (bash, etc.) need the bionic-flavored version. So we'd need either:
- Two LD_PRELOAD entries chained, each ld picks the one it can load — fragile in practice.
- Different LD_PRELOADs per child — same identification problem as the wrapper approach.

### C. Patch shebangs at install time

When npm installs a package, our launcher-gen could rewrite every shipped script's shebang from `#!/usr/bin/env node` to `#!$PREFIX/bin/node`. Avoids the kernel-level `/usr/bin/env` lookup entirely.

Pro: simple, no runtime overhead, no LD_PRELOAD juggling.

Con: every script in `node_modules/` needs the rewrite, including ones in nested deps. Performance hit at install time (~100 files × few ms = seconds). Doesn't help dynamic invocations like `node -e "require('child_process').exec('some-script')"` — the called process still sees its original shebang. But practically, most npm-bin invocations go through the static install layout, so this catches ~90% of cases.

### D. Symlink `/usr/bin/env` to `$PREFIX/bin/env`

Would solve `#!/usr/bin/env node` directly. But `/usr/bin/` is on the system partition (read-only). Even with root we'd be modifying system, which voids warranty / breaks SafetyNet. Not viable.

## Recommendation

For v0.1.x: **document and accept**. Users hitting this are advanced enough to bypass via direct interpreter invocation. We've now told them.

For v0.2 / v1: option C (install-time shebang rewrite) is the cheapest with the highest catch rate. Option A is the most thorough but adds wrapper-stack complexity that's already non-trivial in our boot path.

## Triggers to revisit

- Multiple user reports of "command not found" / ENOENT during `npm`/`node` workflows that worked on a regular Linux dev box.
- A CLI tool whose primary use case is spawning subprocess pipelines (claude, gemini-cli, codex, aider) ships and the subprocess hostility becomes the dominant pain.
- Bigger surface (Python CLIs, Ruby CLIs, etc.) start landing — Python's shebang patterns are similar (`#!/usr/bin/env python3`).
