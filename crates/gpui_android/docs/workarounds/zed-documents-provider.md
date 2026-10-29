# `ZedDocumentsProvider` exposes `~` as a SAF root

**Status:** Active
**Phase / Commit:** L8
**Files:**
- `crates/.../kotlin/com/zdroid/ZedDocumentsProvider.kt` (new)
- `crates/.../kotlin/com/zdroid/ZedApplication.kt` (mkdir `~`)
- `crates/.../AndroidManifest.xml` (`<provider>` declaration)

## Problem

Termux exposes its home dir to other apps via Android's Storage Access
Framework — open any system file picker and "Termux" appears in the
sidebar with full read-write browse. We didn't have an equivalent: our
`/data/data/com.zdroid/files/home` was effectively invisible to
the rest of the OS. Couldn't share files out of `~/projects` via the
share sheet, couldn't drop into a stock document picker and pick "from
Zed", couldn't have other apps (NetHunter, Files-app, IDEs, etc.)
discover our project tree.

## Constraint

`/data/data/<pkg>/...` is app-private — by design only our process can
read/write it. The system-level mechanism for "expose private files to
the user via the system picker" is `android.provider.DocumentsProvider`.
You subclass it, register in the manifest with the
`DOCUMENTS_PROVIDER` intent filter and `MANAGE_DOCUMENTS` permission,
and the system DocumentsUI surfaces your provider's roots in the
sidebar.

API floor is 19; we're at minSdk=26, fine.

## Solution

Port Termux's `TermuxDocumentsProvider` shape with our paths and three
deliberate variances. ~280 lines of Kotlin in
`ZedDocumentsProvider.kt`.

Manifest:

```xml
<provider
    android:name=".ZedDocumentsProvider"
    android:authorities="com.zdroid.documents"
    android:exported="true"
    android:grantUriPermissions="true"
    android:permission="android.permission.MANAGE_DOCUMENTS">
    <intent-filter>
        <action android:name="android.content.action.DOCUMENTS_PROVIDER"/>
    </intent-filter>
</provider>
```

`MANAGE_DOCUMENTS` is system-only — only the OS DocumentsUI talks to us
directly, ensuring sandboxed UX. App devs reach our root by selecting
it in the picker.

### Provider shape (matches Termux)

- Single root = `~` (`Context.getFilesDir().resolve("home")`).
  Resolves correctly across `/data/data/<pkg>` vs `/data/user/0/<pkg>`
  vs work-profile namespaces.
- Document ID = absolute filesystem path. `getFileForDocId` is
  `new File(docId)` with existence check.
- Root flags: `FLAG_SUPPORTS_CREATE | FLAG_SUPPORTS_SEARCH | FLAG_SUPPORTS_IS_CHILD`.
- Per-document flags computed from `File.canWrite()` and parent
  writability: `FLAG_SUPPORTS_WRITE`, `FLAG_DIR_SUPPORTS_CREATE`,
  `FLAG_SUPPORTS_DELETE`, `FLAG_SUPPORTS_RENAME`,
  `FLAG_SUPPORTS_THUMBNAIL` (image MIMEs only).
- Search via subtree walk capped at 50 results, with canonical-path
  symlink-escape protection (`~/storage/shared` would otherwise leak
  all of `/sdcard` into our search results).
- Conflict-rename on create: appends ` (2)`, ` (3)`, etc. before the
  extension.

### Three variances from Termux

1. **Custom MIME map for dev extensions.** `MimeTypeMap.getSingleton()`
   returns `application/octet-stream` for `.rs`, `.toml`, `.md`, `.go`,
   `.py`, `.ts`, `.tsx`, etc. Receiving apps that filter on `text/*`
   (share sheet, "open with") then don't see them. We override with
   `text/rust`, `application/toml`, `text/markdown`, `text/x-go`,
   `text/x-python`, `text/typescript`, etc. before falling through to
   the system MIME map.
2. **Search skip list.** `~` will commonly contain `.git/`,
   `node_modules/`, `target/`, `__pycache__/`, `.venv/`, `.cargo/`,
   `.rustup/`, `build/`, `dist/`, `.gradle/`, `.idea/`, `.next/` —
   directories with thousands of files that match no useful query and
   exhaust the 50-result cap on a top-down walk before reaching real
   source. Skipped only during recursive search (root-anchored walk);
   users can still navigate into them via `queryChildDocuments`.
3. **Missing-home guard.** Termux assumes `$HOME` always exists (its
   first-launch setup runs from an Activity, and queries land after
   Activity startup). Ours doesn't:
   ContentProviders attach earlier than Activities (see below). We
   defend with `if (!baseDir.isDirectory) return emptyCursor`, plus a
   `mkdirs()` in `ZedApplication.onCreate` so `~` always exists pre-
   provider-call. Defense in depth.

### Lifecycle gotcha (the load-bearing one)

When another app opens DocumentsUI and Zed isn't running:

1. System forks our process
2. `Application.attachBaseContext` → `ContentProvider.attachInfo`
   (no actual queries yet)
3. `Application.onCreate` runs
4. `ContentProvider.onCreate` runs
5. System services queries

`MainActivity` is **never instantiated** in this path. `android_main`
never runs. The runtime setup lives there and so do our env-setup
`std::fs::create_dir_all(&home)` calls. So a cold provider query finds no
`~` dir.

If `queryRoots` returns empty here, the user sees "Zdroid" in the SAF
sidebar with a blank inside on tap — and on some Android versions the
system caches the empty root and stops surfacing the provider until
next reboot. Bad UX.

The fix is one line in `ZedApplication.onCreate`:

```kotlin
override fun onCreate() {
    super.onCreate()
    System.loadLibrary("zed_android")
    File(filesDir, "home").mkdirs()
}
```

`Application.onCreate` runs before any query is served (step 3 above
precedes step 5). Costs ~0ms when the dir already exists. The
`isDirectory()` check in `queryRoots` becomes defense-in-depth rather
than the primary safety net.

## Why this works

`getAppTasks`-style "self-only API" parallel: SAF's contract is that
the system DocumentsUI is the trusted intermediary; our provider is
declared as needing `MANAGE_DOCUMENTS` so only it can call us
directly, but it's `exported=true` so the system can see us at all.
Standard pattern.

Verified end-to-end: Files-app's "Open from" picker shows "Zdroid" in the
sidebar with subtitle "Home folder". Tapping it lists `~`
contents (visible: projects, storage, plus any non-hidden files). Standard SAF actions
(navigate, "Use this folder", create folder) all wired.

## Failure mode if regressed

- **Without `mkdirs()` in Application.onCreate:** First cold query
  from another app sees an empty root → user gets confused or the
  system silently drops our root from the sidebar.
- **Without `MANAGE_DOCUMENTS` permission gate:** Random apps could
  skip the picker UX and call our provider directly, bypassing the
  Documents UI sandboxing. (Wouldn't actually leak data — they still
  go through our path-prefix and `canonicalPath` checks — but it
  breaks the SAF contract assumption.)
- **Without canonical-path check in search:** `~/storage/shared`
  symlink would expose all of `/sdcard` matching any search query.
- **Without skip list:** searching from root with `node_modules/` in
  the tree fills the 50-result cap from useless `.d.ts` and `.js.map`
  files before finding a real `Cargo.toml`.

## See also

- [SAF picker integration](saf-picker-integration.md) — the *client*
  side (Zed's Open Project picker invoking DocumentsUI). This file
  is the *provider* side.
- Termux source we ported from:
  `termux/termux-app/app/src/main/java/com/termux/filepicker/TermuxDocumentsProvider.java`
- [`ZedApplication` for native lib load](zedapplication-loadlibrary.md)
  — the same Application also does the `loadLibrary`, gated by the
  same provider-attaches-pre-Activity ordering.
