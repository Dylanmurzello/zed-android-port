# Zdroid fork review — 2026-10-28

## Baselines and coverage

This review starts at Zdroid `v0.9.15`, commit
`2fa3d857daa815309f4213cd6b244a1cc4af4ede`, on `origin/zdroid`.
Dylan's baseline is `1755a0d45865f985bbfa27f65350a840c7df52d2`
(`dylan/main`, 2026-08-07). The tracked Zed release is `v1.26.2-pre`,
`87353f5227f6bc0757c3a077f42841dac6a8ed8e`. This review does not merge a
newer Zed release.

The reviewed changes are based on `android-insets` at
`27bfb10a952e4546ee3b7f461094053782594225`, which already sets version
0.9.16 (57). This follow-up uses 0.9.17 (58). The second pass reviewed the
combined window/input paths and retained the base branch's animated keyboard
insets and layout resizing. Those interactions still require device checks.

The initial inventories contained 253 changed paths under `crates` against
Zed, including fork-owned files and documentation, and 77 changed paths in
`gpui_android` and `zdroid_runtime` against Dylan. Review covered those
inventories and traced startup, subprocess dispatch, input, rendering,
lifecycle, storage, credentials, updates and the shared Zed patches. It is
a source and build review, not proof that every interaction works on a device.

The owner's runtime is **Dylan's Bootstrap**, so its existing package layout
and patches remain the primary local-tool compatibility target. GitHub's latest
Bootstrap release at review time was
[`v2026.05.06-r4`](https://github.com/Dylanmurzello/zdroid-bootstrap/releases/tag/v2026.05.06-r4).
That is the distribution's version, not a measurement of the tablet's installed
prefix. No Bootstrap fork or userland update was made.

## Implemented changes

| Area | Finding and change |
|---|---|
| Native Zed AI | `agent_ui` and its dependencies were already in the build graph, but model providers were never registered. Initialize Zed's existing BYO provider registry, extension proxy and Copilot Chat. Reuse native OpenAI-compatible gateways such as LiteLLM. Android excludes the Zed-hosted provider, token refresh and cloud web-search provider; retain the empty native web-search registry for agent code. Reuse API-key onboarding without sign-in/trial prompts, and remove account actions and account-only settings. The existing agent and inline-assistant UI receives the providers; no Android provider implementation was added. Device streaming, tool calls and credential recovery remain unverified. |
| Runtime selection | The picker reconstructed defaults on every selection and discarded saved adapter settings. Centralize defaults and switch the active selection while preserving each configuration. Health checks and installation now use saved Bootstrap/chroot settings. Keep install/save errors visible. |
| Unsupported runtime | External Termux had no working spawn or install implementation. Remove its selectable card and stub adapter. Continue reading the legacy configuration so users can recover through the picker. Remove unused setup types, the duplicate configuration enum, and unused uninstall/kill/restart interface methods. |
| Spawn routing | Android's async command constructor duplicated the synchronous bridge. Convert the existing `gpui_util::new_std_command` to the async command type, retaining one implementation of environment-root and shebang routing. |
| npm | `npm_config_libc=musl` was forced for chroot as well as Bootstrap. Scope it to the Bootstrap adapter's published environment; retain Bootstrap's preload and musl behavior. |
| Project imports | Entry errors were logged and skipped, yet reported as success. Destination names were checked without reservation, allowing concurrent copies to overwrite or remove one another. Reserve with atomic directory creation, propagate errors, clean incomplete copies in the worker, preserve symlinks, reject recursive destinations, and open the result in the initiating window. The second pass fixed a nested window update when showing the toast, cancels work when the dialog is dropped, and prevents a late result from opening a project after dismissal. Remove the unused progress API. |
| Credentials | Concurrent first writes could create and replace the same Android Keystore alias. Serialize key lookup/creation while retaining Zed's credential interface. |
| Document provider | Canonical-path string prefixes admitted sibling names, and directory symlinks could loop indefinitely during search. Compare path components and track visited canonical paths. Do not advertise or accept deleting/renaming the provider's home root. |
| Cursor compatibility | The API-29 cursor implementation called API-30/33 methods without guards and passed an unsupported buffer format. Use RGBA buffers, gate frame-rate hints, and use the documented geometry API on Android 10–12. The current-device path keeps `setPosition`; no minimum SDK or CPU change. |
| Captured input | Remove unused absolute-coordinate arrays from both sides of JNI and the unused extra-window timestamp. Remove an unused tap-anchor coordinate. Enforce elapsed time and cumulative motion for captured two-finger taps. Device gesture regression checks remain necessary. |
| Build and stale files | Remove the superseded shell runtime router and unused platform ZIP dependency. Let Cargo determine helper freshness, with explicit output directories, instead of Gradle checking only direct source files. Remove stale asset-compression settings, the app's nonexistent `test-support` feature condition, and an unused splash theme/drawable/color confirmed by Lint and reference searches. Retain the active splash overlay and cursor license. Update both lockfiles and bump the app to 0.9.17 (58). |
| Documentation and CI | Correct runtime/AI/backlog claims and the fork's APK download links. Run host importer tests and Android Lint in CI. Preserve `RELEASE_CHANNEL=dev` and leave the `ours` merge driver unset. |

## Upstream reuse and retained boundaries

| Reviewed code | Decision |
|---|---|
| `gpui_android`: dispatcher, platform, windows, display, JNI, frame timing | Retain the Android NDK event loop, surfaces and lifecycle integration. At this tag `ThreadedDispatcher` is available only for tests/benchmarks, and `run_embedded` still calls `Platform::run`; neither replaces the production Android loop. |
| `gpui_android`: events, touch, captured pointer, keyboard, IME | Finger input already feeds GPUI's touch recognizer. Keep that integration. Captured trackpad/virtual trackpad behavior and Android IME bridging still require backend code. Outstanding contract gaps are listed below and in `BACKLOG.md`. |
| `gpui`, editor, terminal view, scrollbars, workspace dock/splitters | Touch claims, long press, tap modifiers and keyboard requests still extend missing behavior in the tracked release. They reuse native editing, selection, scrolling and resizing. Replacement criteria remain in [ahead-of-upstream.md](ahead-of-upstream.md). |
| `gpui_wgpu`, audio, HTTP/TLS | Retain Android font discovery, rendering changes, audio build accommodations and certificate handling. Compilation does not validate GPU behavior or audio. No speculative performance rewrite was made. |
| `zdroid_runtime`, `gpui_util`, util, terminal, node runtime | Keep the real Bootstrap/chroot boundary. Zed's native language/tool/terminal features use it; duplicating those features would increase divergence. Bootstrap package patching belongs in the userland repository. |
| fs, paths, project environment/worktree store, path equivalence | Android storage, temporary paths and chroot bind identity still need adaptations. Keep existing native Zed filesystem/project behavior around those boundaries. Shared-storage working-directory redirection needs tool-specific device checks. |
| Languages and extension host | Retain Android runtime platform mapping and the Wasmtime failure handling. These expose existing Zed extensions and language services rather than replacing them. |
| SSH, client, auto-update | Retain remote-server version/channel handling and Android SSH accommodations. The APK updater remains separate because native Zed desktop installation does not install Android packages. It reuses native update UI where available. |
| Settings, onboarding, welcome, prompts | Keep native Zed controls and action dispatch. Android runtime/input choices and shared-storage grouping still add platform-specific behavior. |
| App `lib.rs`, `zed.rs`, `zed/migrate.rs` | Zed's application crate is a binary, so these startup copies cannot simply be linked as a library. Keep the explicit comparison procedure in `FORK.md`; model registration is restored in this review. |
| App title bar, menu, Android services | Some title-bar/menu duplication remains. Runtime, APK-update and shared-storage affordances need integration points before replacing the shell with upstream UI. Keystore, DocumentsProvider, PackageInstaller and foreground-service lifecycle are Android platform responsibilities. |
| `termux-patches/dpkg` | Retain. Bootstrap's [dpkg patch documentation](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/dpkg-path-rewrite-patches.md) explicitly references these files, and its source tree does not contain replacements. They are external build inputs, despite having no editor build consumer. Move them only with a coordinated Bootstrap source/rebuild change. |
| CI, lockfiles, release configuration | Keep the separate Android Cargo workspace and locked builds. Retain CPU tuning and API-28 targeting. Signing and release publication were not exercised in this review. |

Android's [API-29 execution restriction](https://developer.android.com/about/versions/10/behavior-changes-10#execute-permission)
prevents simply raising `targetSdk` while continuing to execute downloaded
Bootstrap binaries from app-private storage. A target-SDK migration needs a
runtime design change and device validation; raising the number alone would
remove working functionality.

The importer remains a small custom implementation because the tracked
`fs::copy_recursive` follows directory symlinks and has no cancellation hook.
Its replacement criteria and behavior are documented in
[the import writeup](workarounds/noexec-banner-move.md).

The cursor API corrections follow Android's
[SurfaceControl builder](https://developer.android.com/reference/android/view/SurfaceControl.Builder)
and [transaction](https://developer.android.com/reference/android/view/SurfaceControl.Transaction)
contracts. The credential and file-provider boundaries follow the platform's
[Keystore](https://developer.android.com/privacy-and-security/keystore) and
[DocumentsProvider](https://developer.android.com/guide/topics/providers/create-document-provider)
APIs.

## Remaining work, in priority order

1. **Input correctness:** mouse-up click counts and captured mouse double-clicks;
   mouse/stylus classification in extra windows; non-US layouts, AltGr and dead
   keys; captured-pointer loss/drag-lock timers; hover exit and stylus buttons.
   The wider input inventory remains in `BACKLOG.md`.
2. **IME:** caret geometry, cursor updates, selection/composing regions,
   surrounding-text deletion and cursor-position semantics. Keyboard insets
   are implemented by the base branch. Adopt GPUI's text-input configuration
   and back-handler APIs when Zed supplies the corresponding configuration
   and handlers, as tracked in `ahead-of-upstream.md`.
3. **Native feature validation:** test the newly registered BYO providers,
   LiteLLM streaming and tool calls, credential persistence, inline assistance,
   local ACP/MCP and sandbox behavior. Copilot edit prediction, debugger UI and
   REPL startup remain absent; use their existing Zed components when their
   Android dependencies are verified. Zed account services and collaboration
   are intentionally excluded.
4. **Storage and clipboard:** `prompt_for_paths` still ignores parts of
   `PathPromptOptions`; system reveal/open hooks are empty. The clipboard is
   text-only and can drop a second write while one is in flight. Use Android
   document/clipboard APIs behind Zed's existing abstractions.
5. **Application shell:** compare copied startup on every tag merge and replace
   title-bar/menu duplication when upstream exposes sufficient hooks. Remove
   individual old workaround documents only after checking external consumers;
   the dpkg patches demonstrate why absence of Rust references is insufficient.
6. **Bootstrap maintenance:** if editor changes prove insufficient, fork
   `zdroid-bootstrap` with a reproducible patched base build, retain the
   `com.zdroid` prefix, migrate the dpkg source patches with it, and publish a
   versioned ZIP before changing the editor's configurable `release_repo`.
   Verify apt upgrade hooks, npm native binaries, loader/DNS patches and package
   reinstall behavior on the actual prefix. No unverified package update is
   included here.

## Validation and device checklist

Completed locally on the combined branch:

- Formatting checks for the root workspace, Android app and askpass helper.
- `cargo test -p zdroid_runtime -p gpui_android --locked`: 13 passing host tests
  (eight runtime/configuration/archive tests and five importer tests).
- App workspace: `cargo ndk -t arm64-v8a -P 26 check --locked`.
- Gradle: `:app:compileDebugKotlin :app:lintDebug`, including release builds of
  `zd-exec` and the static askpass helper. Lint: zero errors, 19 warnings.
- `git diff --check`; tracked askpass asset restored after Gradle.

After excluding Zed account services, the locked Android check and formatting
checks passed again. The earlier host tests and Kotlin/Lint checks cover the
unchanged runtime, platform and Android application changes. Native provider
startup/idle cost, memory use and APK size have not been measured; inference
uses the configured provider, without loading model weights in the Android app.

These checks do **not** include a fully linked editor APK or a new build running
on the tablet. The PR's **Zdroid APK** build remains the full packaging gate.
ADB confirmed the reference tablet has 0.9.15 (56) installed; the app was not
running when checked. No installation, screenshot, credential inspection or
user-data mutation was performed. The original checkout's modified `.gitignore`
and untracked `wget-log` remain untouched.

Before release, test Bootstrap first:

- Launch/restore an existing project; terminal, git, LSP startup, task execution,
  npm installation/native package selection, and package-manager hooks.
- Switch away and back with custom runtime settings; failed/offline Bootstrap
  install leaves an actionable error. Recover an old External Termux selection.
- Import a shared-storage project, cancel a copy, repeat an import, and trigger
  a read/write failure; never open a partial tree or alter an existing project.
- Fresh agent setup shows provider configuration without Zed account/trial UI.
  Configure a LiteLLM model and key, stream a response with a tool call, perform
  an inline edit, then restart and verify credential recovery. Check local and
  SSH ACP/MCP workflows and the no-provider state. Gateway requests originate
  on the tablet even for SSH projects; Zed-hosted web search is not enabled.
- Cursor capture/release in main and extra windows; rapid tap, long hold,
  two-finger tap and slow scrolling on the keyboard-cover trackpad. Verify the
  Android 10–12 cursor fallback separately if supporting those OS versions.
- SAF search through symlink cycles, ordinary shared-storage links, and attempts
  to rename/delete the provider root; background/foreground and activity recreation.
- Keyboard show/hide and the programming extras row in main and extra windows;
  content resizing, cursor limits and imports while windows change focus.
