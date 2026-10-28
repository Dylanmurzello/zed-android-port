#![cfg(target_os = "android")]
//! Zed Workspace running on Android. Boots up the full client/project/
//! workspace stack and shows the WelcomePage on first launch (no auto-
//! opened project) — matches official Zed's first-run behaviour.

mod android_input;
mod app_updater;
mod header;
mod logging;
mod menu_bar;
mod noexec_modal;
mod runtime_picker;
mod title_bar;
mod zed;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use android_activity::AndroidApp;
use anyhow::{Context as _, Result};
use client::{Client, ProxySettings, UserStore};
use collections::HashMap;
use db::AppDatabase;
use db::kvp::KeyValueStore;
use editor::Editor;
use fs::{Fs, RealFs};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, TaskExt as _, UpdateGlobal as _};
use gpui_tokio::Tokio;
use log::{error, info};
use node_runtime::NodeRuntime;
use onboarding::{FIRST_OPEN, show_onboarding_view};
use parking_lot::Mutex;
use project::Project;
use prompt_store::PromptBuilder;
use recent_projects::{RemoteSettings, open_remote_project};
use release_channel::AppVersion;
use remote::RemoteConnectionOptions;
use reqwest_client::ReqwestClient;
use session::{AppSession, Session};
use settings::{Settings as _, SettingsStore, watch_config_file};
use theme::{ActiveTheme as _, ThemeRegistry};
use theme_settings::load_user_theme;
use util::ResultExt as _;
use workspace::{
    AppState, MultiWorkspace, SerializedWorkspaceLocation, SessionWorkspace, Toast, Workspace,
    WorkspaceDb, WorkspaceSettings, WorkspaceStore, notifications::NotificationId,
    restore_multiworkspace, workspace_windows_for_location,
};
use zdroid_runtime::{RuntimeId, RuntimeProvider, adapters, config::RuntimeFile};

/// The Zed release this app is built from, reported as the app version so
/// SSH projects fetch the matching stable remote server. Read from
/// crates/zed so it follows every upstream sync.
fn zed_release_version() -> semver::Version {
    let version = include_str!("../../../../zed/Cargo.toml")
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .and_then(|version| version.parse().ok());
    version.unwrap_or_else(|| {
        log::error!("zed_android: no version in crates/zed/Cargo.toml");
        semver::Version::new(0, 1, 0)
    })
}

/// Builds before 0.4.3 kept credentials in zed_credentials_provider's
/// plaintext development file, since the Keystore wasn't wired up. Move any
/// such entries into the Keystore, deleting the file only once every write
/// succeeded.
fn migrate_development_credentials(cx: &mut App) {
    let path = paths::config_dir().join("development_credentials");
    let json = match std::fs::read(&path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!("zed_android: can't read {}: {error}", path.display());
            return;
        }
    };
    let credentials: std::collections::HashMap<String, (String, Vec<u8>)> =
        match serde_json::from_slice(&json) {
            Ok(credentials) => credentials,
            Err(error) => {
                log::warn!("zed_android: can't parse {}: {error}", path.display());
                return;
            }
        };
    let writes = credentials
        .iter()
        .map(|(url, (username, password))| cx.write_credentials(url, username, password))
        .collect::<Vec<_>>();
    cx.background_spawn(async move {
        for write in writes {
            write.await?;
        }
        std::fs::remove_file(&path)?;
        info!(
            "zed_android: moved {} credentials into the Keystore",
            credentials.len()
        );
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

// The next three are copies of the functions in Zed's crates/zed/src/main.rs.
pub(crate) async fn restore_or_create_workspace(
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let kvp = cx.update(|cx| KeyValueStore::global(cx));
    if let Some(multi_workspaces) = restorable_workspaces(cx, &app_state).await {
        let mut error_count = 0;
        for multi_workspace in multi_workspaces {
            let result = match &multi_workspace.active_workspace.location {
                SerializedWorkspaceLocation::Local => {
                    restore_multiworkspace(multi_workspace, app_state.clone(), cx)
                        .await
                        .map(|_| ())
                }
                SerializedWorkspaceLocation::Remote(connection_options) => {
                    let mut connection_options = connection_options.clone();
                    if let RemoteConnectionOptions::Ssh(options) = &mut connection_options {
                        cx.update(|cx| {
                            RemoteSettings::get_global(cx)
                                .fill_connection_options_from_settings(options)
                        });
                    }

                    let paths = multi_workspace
                        .active_workspace
                        .paths
                        .paths()
                        .iter()
                        .map(PathBuf::from)
                        .collect::<Vec<_>>();
                    let state = multi_workspace.state.clone();
                    async {
                        let window = open_remote_project(
                            connection_options,
                            paths,
                            app_state.clone(),
                            workspace::OpenOptions::default(),
                            cx,
                        )
                        .await?;
                        workspace::apply_restored_multiworkspace_state(
                            window,
                            &state,
                            app_state.fs.clone(),
                            cx,
                        )
                        .await;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await
                }
            };

            if let Err(error) = result {
                log::error!("Failed to restore workspace: {error:#}");
                error_count += 1;
            }
        }

        if error_count > 0 {
            let message = if error_count == 1 {
                "Failed to restore 1 workspace. Check logs for details.".to_string()
            } else {
                format!(
                    "Failed to restore {} workspaces. Check logs for details.",
                    error_count
                )
            };

            // Try to find an active workspace to show the toast
            let toast_shown = cx.update(|cx| {
                if let Some(window) = cx.active_window()
                    && let Some(multi_workspace) = window.downcast::<MultiWorkspace>()
                {
                    multi_workspace
                        .update(cx, |multi_workspace, _, cx| {
                            multi_workspace.workspace().update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(NotificationId::unique::<()>(), message.clone()),
                                    cx,
                                )
                            });
                        })
                        .ok();
                    return true;
                }
                false
            });

            // If we couldn't show a toast (no windows opened successfully),
            // open a fallback empty workspace and show the error there
            if !toast_shown {
                log::error!("All workspace restorations failed. Opening fallback empty workspace.");
                cx.update(|cx| {
                    workspace::open_new(
                        Default::default(),
                        app_state.clone(),
                        cx,
                        |workspace, _window, cx| {
                            workspace.show_toast(
                                Toast::new(NotificationId::unique::<()>(), message),
                                cx,
                            );
                        },
                    )
                })
                .await?;
            }
        }

        // If the user cancelled a failed remote connection at startup,
        // open_remote_project returns Ok but removes the window, so error_count
        // stays 0 and the toast fallback above does not trigger. Without this
        // check, Zed would exit silently.
        if cx.update(|cx| cx.windows().is_empty()) {
            cx.update(|cx| {
                workspace::open_new(
                    Default::default(),
                    app_state.clone(),
                    cx,
                    |workspace, window, cx| {
                        let restore_on_startup =
                            WorkspaceSettings::get_global(cx).restore_on_startup;
                        match restore_on_startup {
                            workspace::RestoreOnStartupBehavior::Launchpad => {}
                            _ => {
                                Editor::new_file(workspace, &Default::default(), window, cx);
                            }
                        }
                    },
                )
            })
            .await?;
        }
    } else if matches!(kvp.read_kvp(FIRST_OPEN), Ok(None)) {
        cx.update(|cx| show_onboarding_view(app_state, cx)).await?;
    } else {
        cx.update(|cx| {
            workspace::open_new(
                Default::default(),
                app_state,
                cx,
                |workspace, window, cx| {
                    let restore_on_startup = WorkspaceSettings::get_global(cx).restore_on_startup;
                    match restore_on_startup {
                        workspace::RestoreOnStartupBehavior::Launchpad => {}
                        _ => {
                            Editor::new_file(workspace, &Default::default(), window, cx);
                        }
                    }
                },
            )
        })
        .await?;
    }

    Ok(())
}

async fn restorable_workspaces(
    cx: &mut AsyncApp,
    app_state: &Arc<AppState>,
) -> Option<Vec<workspace::SerializedMultiWorkspace>> {
    let locations = restorable_workspace_locations(cx, app_state).await?;
    Some(cx.update(|cx| workspace::read_serialized_multi_workspaces(locations, cx)))
}

pub(crate) async fn restorable_workspace_locations(
    cx: &mut AsyncApp,
    app_state: &Arc<AppState>,
) -> Option<Vec<SessionWorkspace>> {
    let (mut restore_behavior, db) = cx.update(|cx| {
        (
            WorkspaceSettings::get(None, cx).restore_on_startup,
            workspace::WorkspaceDb::global(cx),
        )
    });

    let session_handle = app_state.session.clone();
    let (last_session_id, last_session_window_stack) = cx.update(|cx| {
        let session = session_handle.read(cx);

        (
            session.last_session_id().map(|id| id.to_string()),
            session.last_session_window_stack(),
        )
    });

    if last_session_id.is_none()
        && matches!(
            restore_behavior,
            workspace::RestoreOnStartupBehavior::LastSession
        )
    {
        restore_behavior = workspace::RestoreOnStartupBehavior::LastWorkspace;
    }

    match restore_behavior {
        workspace::RestoreOnStartupBehavior::LastWorkspace => {
            workspace::last_opened_workspace_location(&db, app_state.fs.as_ref())
                .await
                .map(|(workspace_id, location, paths)| {
                    vec![SessionWorkspace {
                        workspace_id,
                        location,
                        paths,
                        window_id: None,
                    }]
                })
        }
        workspace::RestoreOnStartupBehavior::LastSession => {
            if let Some(last_session_id) = last_session_id {
                let ordered = last_session_window_stack.is_some();

                let mut locations = workspace::last_session_workspace_locations(
                    &db,
                    &last_session_id,
                    last_session_window_stack,
                    app_state.fs.as_ref(),
                )
                .await
                .filter(|locations| !locations.is_empty());

                // Since last_session_window_order returns the windows ordered front-to-back
                // we need to open the window that was frontmost last.
                if ordered && let Some(locations) = locations.as_mut() {
                    locations.reverse();
                }

                // Android: the app has one main window (others would open as
                // extra Activities), so restore only the frontmost window,
                // with all of its workspaces. Android reports no window
                // stack, so the locations are newest first.
                if let Some(locations) = locations.as_mut() {
                    let frontmost = if ordered {
                        locations.last()
                    } else {
                        locations.first()
                    };
                    if let Some(frontmost) = frontmost {
                        let window_id = frontmost.window_id;
                        let workspace_id = frontmost.workspace_id;
                        locations.retain(|location| match window_id {
                            Some(window_id) => location.window_id == Some(window_id),
                            None => location.workspace_id == workspace_id,
                        });
                    }
                }

                locations
            } else {
                None
            }
        }
        _ => None,
    }
}

fn minimal_window_options(_: Option<uuid::Uuid>, _cx: &mut App) -> gpui::WindowOptions {
    gpui::WindowOptions::default()
}

/// Build the active adapter from `runtime.toml`, returning the boxed
/// provider so callers can ask for its `environment_root()` and
/// adapter-derived metadata. Returns None when no toml exists or the
/// adapter construction fails (defaults are filled in by the picker
/// the first time the user opens it).
fn build_active_provider(data_path: &std::path::Path) -> Option<Box<dyn RuntimeProvider>> {
    let file = RuntimeFile::load(&data_path.join("usr/etc/zd-runtime.toml"))
        .ok()
        .flatten()?;
    let resolved = file.resolve().ok()?;
    adapters::for_config(&resolved).ok()
}

/// Same as `build_active_provider` but never returns `None`. When no
/// runtime.toml exists yet (first launch before the picker is opened)
/// we fall back to a default Bootstrap adapter so the env-init path
/// always has someone to ask. The user's eventual picker selection
/// rewrites runtime.toml and the next launch picks up the chosen
/// adapter via `build_active_provider`.
fn active_provider(data_path: &std::path::Path) -> Box<dyn RuntimeProvider> {
    if let Some(provider) = build_active_provider(data_path) {
        return provider;
    }
    let file = RuntimeFile::with_defaults(RuntimeId::Bootstrap);
    let resolved = file
        .resolve()
        .expect("default Bootstrap RuntimeFile must resolve");
    adapters::for_config(&resolved).expect("default Bootstrap adapter must construct")
}

// The next four are copies of the functions in Zed's crates/zed/src/main.rs.
fn init_paths() -> HashMap<io::ErrorKind, Vec<&'static Path>> {
    [
        paths::config_dir(),
        paths::extensions_dir(),
        paths::languages_dir(),
        paths::debug_adapters_dir(),
        paths::database_dir(),
        paths::logs_dir(),
        paths::temp_dir(),
        paths::hang_traces_dir(),
    ]
    .into_iter()
    .fold(HashMap::default(), |mut errors, path| {
        if let Err(e) = std::fs::create_dir_all(path) {
            errors.entry(e.kind()).or_insert_with(Vec::new).push(path);
        }
        errors
    })
}

fn load_embedded_fonts(cx: &App) {
    let asset_source = cx.asset_source();
    let font_paths = asset_source.list("fonts").unwrap();
    let embedded_fonts = Mutex::new(Vec::new());
    let executor = cx.background_executor();

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }

            scope.spawn(async {
                let font_bytes = asset_source.load(font_path).unwrap().unwrap();
                embedded_fonts.lock().push(font_bytes);
            });
        }
    }));

    cx.text_system()
        .add_fonts(embedded_fonts.into_inner())
        .unwrap();
}

/// Spawns a background task to load the user themes from the themes directory.
fn load_user_themes_in_background(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    cx.spawn({
        let fs = fs.clone();
        async move |cx| {
            let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
            let themes_dir = paths::themes_dir().as_ref();
            match fs
                .metadata(themes_dir)
                .await
                .ok()
                .flatten()
                .map(|m| m.is_dir)
            {
                Some(is_dir) => {
                    anyhow::ensure!(is_dir, "Themes dir path {themes_dir:?} is not a directory")
                }
                None => {
                    fs.create_dir(themes_dir).await.with_context(|| {
                        format!("Failed to create themes dir at path {themes_dir:?}")
                    })?;
                }
            }

            let mut theme_paths = fs
                .read_dir(themes_dir)
                .await
                .with_context(|| format!("reading themes from {themes_dir:?}"))?;

            while let Some(theme_path) = theme_paths.next().await {
                let Some(theme_path) = theme_path.log_err() else {
                    continue;
                };
                let Some(bytes) = fs
                    .load_bytes(&theme_path)
                    .await
                    .with_context(|| format!("loading theme bytes from {theme_path:?}"))
                    .log_err()
                else {
                    continue;
                };

                load_user_theme(&theme_registry, &bytes).log_err();
            }

            cx.update(theme_settings::reload_theme);
            anyhow::Ok(())
        }
    })
    .detach_and_log_err(cx);
}

/// Spawns a background task to watch the themes directory for changes.
fn watch_themes(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    use std::time::Duration;
    cx.spawn(async move |cx| {
        let (mut events, _) = fs
            .watch(paths::themes_dir(), Duration::from_millis(100))
            .await;

        while let Some(paths) = events.next().await {
            for event in paths {
                if fs
                    .metadata(&event.path)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|m| !m.is_dir)
                {
                    let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
                    if let Some(bytes) = fs
                        .load_bytes(&event.path)
                        .await
                        .with_context(|| format!("loading theme bytes from {:?}", event.path))
                        .log_err()
                        && load_user_theme(&theme_registry, &bytes).log_err().is_some()
                    {
                        cx.update(theme_settings::reload_theme);
                    }
                }
            }
        }
    })
    .detach()
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    logging::init_logger();
    info!("zed_android: android_main entry");

    let data_path = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/data/com.zdroid/files"));
    info!("zed_android: data_path = {}", data_path.display());

    // The Zed-Rust process env is now adapter-owned. Every set_var/
    // remove_var that used to live inline here is produced by the
    // active runtime adapter's `env_for_zed_process`:
    //
    //   - Chroot adapter ships a bionic-clean env (no PREFIX, no
    //     TERMUX__*, no libtermux-exec preload). PATH front-loaded with
    //     the zd-runtime symlink farm so `Command::new("java")` finds
    //     zd-exec → zd-spawnd → chroot dispatch.
    //
    //   - Bootstrap adapter keeps the historical Termux-flavored env
    //     so dpkg patches, apt Post-Invoke hooks, and bootstrap-side
    //     tooling keep working unchanged.
    //
    //   - External Termux returns a minimal bionic env; spawns route
    //     via Intent (Phase 7) so nothing here leaks across.
    //
    // The active provider also publishes a terminal env overlay that
    // `crates/terminal/src/terminal.rs` applies when the PTY spawns,
    // since alacritty wipes inherited env on bringup.
    //
    // SAFETY: `set_var` / `remove_var` mutate libc-shared process
    // state. The invariant is "no other thread reads/writes libc env
    // via getenv/setenv at this point" — JVM service threads exist by
    // android_main but don't touch libc env. OnceLock makes activity-
    // recreation re-entry a deterministic no-op.
    //
    // DELIBERATELY NOT SET: LD_LIBRARY_PATH. Setting it globally
    // poisons every spawned subprocess (including /system/bin/
    // app_process64 trying to load /system/lib64/libsqlite.so against
    // our OpenSSL 3.x); Upstream Termux packages that need it set it
    // per-spawn in their launcher scripts. ZED_RELEASE_CHANNEL is also
    // deliberately not flipped to "nightly": channel switching
    // namespaces Zed's app data dir and would shadow the user's
    // existing settings. The dev-channel hard-bail in
    // `crates/remote/src/transport/ssh.rs` is patched directly.
    let provider = active_provider(&data_path);
    log::info!("zed_android: runtime adapter = {:?}", provider.id());

    static ENV_INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = ENV_INITIALIZED.get_or_init(|| {
        let ops = provider.env_for_zed_process(&data_path);
        unsafe {
            for (key, op) in &ops {
                match op {
                    util::env::EnvOp::Set(value) => std::env::set_var(key, value),
                    util::env::EnvOp::Remove => std::env::remove_var(key),
                }
            }
            // The app runs on the Dev channel, where zed_credentials_provider
            // otherwise writes API keys and tokens to a plaintext JSON file.
            // Use gpui's credential API instead, which gpui_android backs
            // with the Android Keystore.
            std::env::set_var("ZED_DEVELOPMENT_USE_KEYCHAIN", "1");
        }
        log::info!(
            "zed_android: PATH = {}; SHELL = {}",
            std::env::var("PATH").unwrap_or_default(),
            std::env::var("SHELL").unwrap_or_default(),
        );
    });

    // The runtime's TMPDIR may not exist yet, and temp files such as SSH's
    // askpass socket go there.
    let temp_dir = std::env::temp_dir();
    if let Err(error) = std::fs::create_dir_all(&temp_dir) {
        log::warn!("zed_android: can't create {}: {error}", temp_dir.display());
    }

    // Publish the per-adapter terminal env overlay so
    // `crates/terminal/src/terminal.rs` can apply it when the PTY
    // spawns. Idempotent: subsequent android_main re-entries (activity
    // recreation) get the same overlay; the registration is OnceLock-
    // guarded inside util::env.
    util::env::register_terminal_env_overlay(provider.env_for_terminal(&data_path));

    // Publish adapter-specific filesystem hints for editor code that
    // historically read TERMUX__HOME / TERMUX__PREFIX env vars
    // directly. Now those readers (workspace::welcome,
    // node_runtime::node_runtime, gpui_android::storage) ask the
    // active adapter via these registry slots — Bootstrap publishes
    // its Termux-flavored paths, chroot publishes the bionic-clean
    // equivalents, external Termux publishes None.
    util::env::register_workspace_root(provider.workspace_root(&data_path));
    util::env::register_npm_libtermux_exec_path(provider.npm_libtermux_exec_path(&data_path));

    // Surface the SELinux domain in logcat — this is the canary for the
    // targetSdk pin. If `untrusted_app_27` flips to `untrusted_app_all`
    // every subsequent execve into $PREFIX/bin/* will EACCES.
    gpui_android::termux_bootstrap::check_selinux_context();

    // Best-effort runtime READ/WRITE_EXTERNAL_STORAGE prompt. Replaces the
    // MANAGE_EXTERNAL_STORAGE → Settings deep-link flow we used at
    // targetSdk=35. Fire-and-forget: dialog shows on first launch, user
    // grants once, RealFs reads of /storage/emulated/0/... start working.
    gpui_android::storage::request_once(&app);

    // Materialize Android's active DNS servers into /sdcard/.zed/r so
    // hex-patched Bun-compiled CLIs (claude, codex, future) can resolve
    // hostnames without proot. Patched binaries open `/sdcard/.zed/r`
    // instead of the original `/etc/resolv.conf`. Falls back to public
    // DNS if ConnectivityManager gives nothing (no active network yet).
    gpui_android::dns_bridge::populate_resolv_conf(&app);

    // Install zd-exec into <data>/files/bin/ from the APK-bundled
    // asset. Idempotent (skipped when the installed copy matches the
    // asset byte for byte), so it runs every boot. Without this, fresh
    // installs land with no zd-exec and `terminal.shell` (which the chroot adapter sets to
    // `<data>/files/bin/zd-exec`) fails with ENOENT. Phase 4 of the
    // Termux-divestment refactor moved this off `$PREFIX/bin/`.
    if let Err(err) = gpui_android::zd_exec_install::ensure_installed(&app, &data_path) {
        log::warn!(
            "zed_android: zd-exec install failed: {err:#}; \
             chroot adapter's integrated terminal will fail to spawn"
        );
    }

    // Populate <data>/files/zd-runtime/ with symlinks for every binary
    // the active runtime adapter advertises. Each symlink points at
    // zd-exec; kernel PATH lookup intercepts Zed's
    // `Command::new("java")` and routes through the bridge to wherever
    // the binary actually lives. The adapter inspects its OWN
    // filesystem (chroot walks the rootfs's /usr/bin etc.; bootstrap
    // walks $PREFIX/bin) — no hardcoded list anywhere. apt-installing
    // a new tool inside the chroot makes it show up after the next
    // launch; switching adapters rewrites the set to match the new
    // env (stale entries get swept). Pre-Phase-4 this lived at
    // `$PREFIX/zd-runtime/`.
    if let Some(provider) = build_active_provider(&data_path) {
        let binaries = provider.list_binaries();
        log::info!(
            "zed_android: zd-runtime: {} binaries from {:?} adapter",
            binaries.len(),
            provider.id(),
        );
        if let Err(err) =
            gpui_android::zd_exec_install::ensure_runtime_symlinks(&data_path, &binaries)
        {
            log::warn!(
                "zed_android: zd-runtime symlinks failed: {err:#}; \
                 LSPs that resolve binaries via PATH will fail to spawn"
            );
        }
    } else {
        log::info!(
            "zed_android: zd-runtime: no active adapter; PATH interception \
             skipped (runtime picker will set one up after first selection)"
        );
    }

    // Wire askpass to the standalone helper. Must happen BEFORE any
    // AskPassSession is created (Open Remote, git auth prompts, etc.)
    // — the askpass crate's ASKPASS_PROGRAM OnceLock initializes on
    // first read with current_exe() (= /system/bin/app_process64 on
    // Android) and subsequent set_askpass_program calls are silently ignored.
    let askpass_path = match gpui_android::askpass_install::ensure_installed(&app, &data_path) {
        Ok(path) => path,
        Err(err) => {
            log::warn!(
                "zed_android: askpass helper install failed: {err:#}; \
                 SSH password / passphrase prompts will fall back to \
                 current_exe() (= app_process64) and SIGABRT on Android"
            );
            // Construct the expected path anyway so set_askpass_program isn't
            // skipped — if the binary materializes later (next boot
            // after the install issue is resolved) it'll be picked up.
            data_path.join("zed-askpass-helper")
        }
    };
    if askpass_path.is_file() {
        // Upstream's setter debug_panics on double-set; boot runs this
        // exactly once, before any AskPassSession exists.
        askpass::set_askpass_program(askpass_path.clone());
        log::info!(
            "zed_android: askpass program set to {}",
            askpass_path.display()
        );
    } else {
        log::warn!(
            "zed_android: askpass helper missing at {}; SSH password / \
             passphrase prompts will fall through to current_exe() and \
             abort under SELinux",
            askpass_path.display()
        );
    }

    // As in Zed's main(), which android_main stands in for. android_main runs
    // again when the activity is recreated, and both of these are once per
    // process.
    static PROCESS_INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    PROCESS_INITIALIZED.get_or_init(|| {
        util::increase_open_file_limit().log_err();
        rayon::ThreadPoolBuilder::new()
            .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().div_ceil(2)))
            .stack_size(10 * 1024 * 1024)
            .thread_name(|ix| format!("RayonWorker{}", ix))
            .build_global()
            .log_err();
    });

    gpui_android::run(app, assets::Assets, move |cx: &mut App| {
        if let Err(err) = boot(cx, &data_path) {
            error!("zed_android: boot failed: {err:#}");
        }
    });
}

fn boot(cx: &mut App, data_path: &std::path::Path) -> Result<()> {
    // android_main can run multiple times for one process when the activity
    // is recreated; paths' OnceLocks survive across invocations. The second
    // call panics with "set_custom_data_dir called after data_dir or
    // config_dir was initialized". Guard with a static flag.
    static PATHS_INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = PATHS_INITIALIZED.get_or_init(|| {
        // Each runtime adapter (chroot / bootstrap / external Termux)
        // gets a FULLY ISOLATED Zed install: its own config, keymap,
        // themes, sqlite db, language servers, extensions, history,
        // logs — everything. Switching adapters via the runtime picker
        // is a workspace switch the way booting from a different SSD
        // is a workspace switch: nothing bleeds across.
        //
        // We do this by setting `paths::set_custom_data_dir` to the
        // active adapter's `environment_root` instead of the generic
        // app data dir. Every `paths::*` function — `config_dir`,
        // `database_dir`, `logs_dir`, `languages_dir`,
        // `extensions_dir`, the lot — derives from this single root.
        // Zed never has to know about adapters.
        //
        // First launch (no runtime.toml yet) falls back to the plain
        // app data dir so the runtime picker UI itself can render and
        // the user can pick an adapter. After they pick + reboot, this
        // branch goes through `build_active_provider` and lands them
        // in the per-adapter root.
        let root = if let Some(provider) = build_active_provider(data_path) {
            let env_root = provider.environment_root();
            log::info!(
                "zed_android: data_dir for {:?} adapter -> {}",
                provider.id(),
                env_root.display(),
            );
            // Test writability before handing the path to paths::set_custom_data_dir,
            // which panics in `create_dir_all` if the path lives in another package's
            // data dir (the ExternalTermux adapter's environment_root points at
            // /data/data/com.termux/... which we can't write to under SELinux). On
            // PermissionDenied we fall back to the bare app data dir so the editor
            // can still boot; the adapter's spawn-side dispatch (env_for_zed_process,
            // zd-exec routing) stays unaffected because that's a separate seam from
            // the editor's data_dir. Without this, picking ExternalTermux in the
            // runtime picker permanently bricks the install in a boot-time panic
            // loop with no in-app recovery path.
            let env_root_usable = match std::fs::create_dir_all(&env_root) {
                Ok(()) => true,
                Err(err) => {
                    log::warn!(
                        "zed_android: {:?} adapter's environment_root {} is not \
                         writable ({err:#}); falling back to bare app data dir for \
                         editor data. Spawn-side dispatch is unaffected.",
                        provider.id(),
                        env_root.display(),
                    );
                    false
                }
            };
            // Register the env_root with util::command so absolute-path
            // spawns under it get rewritten to route through `zd-exec`.
            // Gated on `needs_command_bridge()` so adapters whose
            // binaries run natively on the host (bootstrap's bionic-
            // flavored Termux prefix, external Termux's Intent
            // dispatch) skip the rewrite. Without that gate, every
            // absolute-path spawn from a bootstrap user (PATH-resolved
            // rust-analyzer at $PREFIX/bin/rust-analyzer, downloaded
            // LSPs under <data>/languages/, etc.) would rewrite to
            // `zd-exec <abs>` and fail with ENOENT — bootstrap mode
            // doesn't put zd-exec on PATH.
            if provider.needs_command_bridge() {
                util::command::register_environment_root(env_root.clone());
            }
            if env_root_usable {
                env_root.to_string_lossy().into_owned()
            } else {
                data_path.to_string_lossy().into_owned()
            }
        } else {
            log::info!(
                "zed_android: no runtime.toml; using bare app data_dir {} \
                 (the runtime picker will set up per-adapter roots after first selection)",
                data_path.display(),
            );
            data_path.to_string_lossy().into_owned()
        };
        paths::set_custom_data_dir(&root);
    });
    info!("zed_android: paths data_dir set");

    // Zed's main() exits when it can't create these; here the editor can
    // still open, so only log.
    for (kind, paths) in init_paths() {
        error!("zed_android: couldn't create {paths:?}: {kind}");
    }
    let termux_home = data_path.join("home");
    let projects_dir = termux_home.join("projects");
    for path in [
        &termux_home,
        // Default landing dir for the project picker. Lives on app-private
        // storage where execve and dlopen work natively, so cargo / go /
        // make / native-npm / native-pip all just run. SAF-picked projects
        // get imported here; nothing executable ever runs from /sdcard.
        &projects_dir,
    ] {
        if let Err(err) = std::fs::create_dir_all(path) {
            error!("zed_android: create_dir_all({}): {err:#}", path.display());
        }
    }
    logging::init_log_file();
    // Termux-style ~/storage/* curated symlinks into shared storage. Lets
    // users browse / open / save individual files from /sdcard via
    // ~/storage/{shared,downloads,dcim,...} without ever treating those
    // paths as a workspace root (which would hit the FUSE noexec wall on
    // any compile-and-run flow). Idempotent on re-launch.
    gpui_android::storage::setup_user_symlinks(&termux_home);

    release_channel::init(zed_release_version(), cx);
    info!("zed_android: release_channel init");
    migrate_development_credentials(cx);

    info!("zed_android: settings + theme + editor init");
    settings::init(cx);
    android_input::set_default_settings(cx);
    zlog_settings::init(cx);
    // `gpui_tokio::init` registers the global Tokio runtime that wasmtime's
    // async support taps into for extension epoch interruption + WASI
    // async tasks. Production calls this at zed/src/main.rs:462. Without
    // it, extension_host's first WASM extension load panics:
    //   `RustPanic: no state of type gpui_tokio::GlobalTokio exists`
    // (caught it once on Android, then added this).
    gpui_tokio::init(cx);

    let fs: Arc<dyn Fs> = RealFs::new(None, cx.background_executor().clone());
    zed::watch_settings_files(fs.clone(), cx);
    let (user_keymap_file_rx, user_keymap_watcher) = watch_config_file(
        cx.background_executor(),
        fs.clone(),
        paths::keymap_file().clone(),
    );
    zed::handle_keymap_file_changes(user_keymap_file_rx, user_keymap_watcher, cx);

    // As in Zed's main().
    let user_agent = format!(
        "Zed/{} ({}; {})",
        AppVersion::global(cx),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let proxy_url = ProxySettings::get_global(cx).proxy_url();
    let http = {
        let _guard = Tokio::handle(cx).enter();

        ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)?
    };
    cx.set_http_client(Arc::new(http));
    <dyn Fs>::set_global(fs.clone(), cx);

    // `extension::init` creates the global ExtensionHostProxy that all
    // extension contribution registries (theme, language, debug-adapter)
    // hang off of. Has to run BEFORE any of those `*_extension::init`
    // calls. Cheap — just installs a default proxy global; the real
    // store gets created later by `extension_host::init` once fs/client
    // /node_runtime are available.
    extension::init(cx);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    editor::init(cx);
    android_input::keep_cursor_on_screen_when_window_shrinks(cx);

    let current_from_toml = RuntimeFile::load(&data_path.join("usr/etc/zd-runtime.toml"))
        .ok()
        .flatten()
        .map(|f| f.runtime.kind);
    cx.set_global(runtime_picker::ActiveRuntime {
        current: current_from_toml,
    });
    cx.set_global(onboarding::AndroidBasicsSections(vec![
        runtime_picker::render_onboarding_section,
        android_input::render_onboarding_section,
    ]));

    let app_db = AppDatabase::new();
    cx.set_global(app_db);
    info!("zed_android: AppDatabase opened + set as global");

    let session_id = uuid::Uuid::new_v4().to_string();
    let session = gpui::block_on(Session::new(session_id, KeyValueStore::global(cx)));
    info!("zed_android: Session opened");

    let client = Client::production(cx);
    cx.set_http_client(client.http_client());
    info!(
        "zed_android: Client::production constructed (id={})",
        client.id()
    );

    // Initialize auto_update so the remote_server CDN-fetch path can
    // resolve the right binary URL via `GlobalAutoUpdate`. The
    // remote/transport.rs flow at `auto_update.rs:516` reads this
    // global to know which `zed-remote-server` asset to fetch from
    // GitHub releases; without it, every Open Remote attempt hard-bails
    // with "auto-update not initialized" before any download starts.
    //
    // We deliberately set `ZED_UPDATE_EXPLANATION` ahead of `init` so
    // the polling subscription (auto_update.rs:248-262) is suppressed:
    // Zed's updater can't install an APK, so `app_updater` checks this
    // fork's releases instead. The explanation string only shows if
    // something calls `auto_update::check()` directly.
    unsafe {
        std::env::set_var(
            "ZED_UPDATE_EXPLANATION",
            "Zdroid updates itself: use Zdroid → Check for Updates.",
        );
    }

    // auto_update::Check action interception. Same gpui dispatch trap
    // as the OpenSettings deep-link: `auto_update::init` (just below)
    // calls cx.observe_new to register the upstream Check handler at
    // workspace_actions[0]. Upstream's handler calls `auto_update::check`
    // which, with ZED_UPDATE_EXPLANATION set, just shows a "Zed was
    // installed via a package manager" prompt instead of our actual
    // in-app updater. Bubble-phase action dispatch auto-stops after
    // the first listener that does not call cx.propagate, so any
    // Check handler we register later (e.g. inside the big workspace
    // observe_new further down) never runs.
    //
    // Fix: register our handler BEFORE auto_update::init so it sits
    // at workspace_actions[0] and runs first. The handler starts a
    // manual check in `app_updater`, whose progress shows in the title
    // bar, and intentionally does not propagate, so upstream's
    // package-manager prompt is bypassed.
    // Direct calls to `auto_update::check()` (e.g. from the title bar
    // collab "Please update Zed" button) still hit the prompt; that
    // is acceptable because those callsites bypass the action system
    // and our interception cannot reach them without forking the
    // function.
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(
            |_workspace: &mut Workspace, _: &auto_update::Check, _window, cx| {
                if let Some(updater) = app_updater::AppUpdater::global(cx) {
                    updater.update(cx, |updater, cx| updater.check(true, cx));
                }
            },
        );
    })
    .detach();

    auto_update::init(client.clone(), cx);
    info!("zed_android: auto_update::init complete (polling suppressed)");
    app_updater::init(cx);

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
    info!("zed_android: UserStore + WorkspaceStore constructed");

    // Real NodeRuntime, mirroring crates/zed/src/main.rs:496-518. The
    // earlier port stage stubbed this out as `NodeRuntime::unavailable()`
    // back when Termux's Node didn't run on bionic — pre L3 npm intercept
    // architecture (project_l3_npm_intercept memory). The intercept layer
    // (patched node platform string, npm wrapper, launcher-gen RUNPATH
    // fixup, libtermux-exec LD_PRELOAD) plus the bundled musl loader make
    // a Termux-installed Node usable. Wire NodeRuntime against settings
    // so PATH-resolved Node (`pkg install nodejs` → $PREFIX/bin/node)
    // works, and Zed's managed-node fallback download path is available
    // for users without termux Node. Without this, npm-based LSPs
    // (TypeScript / JavaScript / Pyright / etc.) fail at the install step
    // with `'node' settings do not allow any way to use Node.js`.
    let (mut node_options_tx, node_options_rx) =
        watch::channel::<Option<node_runtime::NodeBinaryOptions>>(None);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = &project::project_settings::ProjectSettings::get_global(cx).node;
        let options = node_runtime::NodeBinaryOptions {
            allow_path_lookup: !settings.ignore_system_version,
            allow_binary_download: true,
            use_paths: settings.path.as_ref().map(|node_path| {
                let node_path = std::path::PathBuf::from(shellexpand::tilde(node_path).as_ref());
                let npm_path = settings
                    .npm_path
                    .as_ref()
                    .map(|p| std::path::PathBuf::from(shellexpand::tilde(&p).as_ref()));
                (
                    node_path.clone(),
                    npm_path.unwrap_or_else(|| {
                        node_path
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_default()
                            .join("npm")
                    }),
                )
            }),
        };
        node_options_tx.send(Some(options)).log_err();
    })
    .detach();
    ui::on_new_scrollbars::<SettingsStore>(cx);
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);
    info!("zed_android: RealFs + NodeRuntime constructed (PATH lookup + managed download)");

    // Mirror `android_input.*` settings into gpui_android's runtime
    // atomics directly via observe_global, NOT via per-paint
    // `window.set_*` writes from `workspace::pane`. The pane path
    // only fires when a Pane renders — during onboarding (before a
    // project is open) there is no Pane, so the atomics stay at
    // their `true`/`false` defaults regardless of what the user
    // toggles in Settings. Symptom: opening Settings during
    // onboarding, switching `on_screen_keyboard` off, then focusing
    // a text input still pops the soft IME because the gate atomic
    // is stale. Observer fires once immediately + on every
    // SettingsStore change, so the gate is always fresh.
    let push_android_input_atomics = |cx: &gpui::App| {
        let android_input = android_input::AndroidInputSettings::get_global(cx);
        gpui_android::set_on_screen_keyboard_enabled(android_input.on_screen_keyboard);
        let trackpad_active = android_input.trackpad_mode && android_input.trackpad_mode_active;
        gpui_android::set_trackpad_mode_enabled(trackpad_active);
        gpui_android::set_programming_extras_row_enabled(android_input.programming_extras_row);
        gpui_android::set_invert_scroll(android_input.invert_scroll);
    };
    push_android_input_atomics(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        push_android_input_atomics(cx);
    })
    .detach();
    info!("zed_android: android_input atomics observer registered");

    load_embedded_fonts(cx);

    let mut language_registry = language::LanguageRegistry::new(cx.background_executor().clone());
    language_registry.set_language_server_download_dir(paths::languages_dir().clone());
    let language_registry = Arc::new(language_registry);

    languages::init(
        language_registry.clone(),
        fs.clone(),
        node_runtime.clone(),
        cx,
    );
    info!("zed_android: languages::init complete (load-grammars feature on)");

    language_extension::init(
        language_extension::LspAccess::ViaWorkspaces({
            let workspace_store = workspace_store.clone();
            Arc::new(move |cx: &mut App| {
                workspace_store.update(cx, |workspace_store, cx| {
                    Ok(workspace_store
                        .workspaces()
                        .filter_map(|weak| weak.upgrade())
                        .map(|workspace: gpui::Entity<workspace::Workspace>| {
                            workspace.read(cx).project().read(cx).lsp_store()
                        })
                        .collect())
                })
            })
        }),
        extension::ExtensionHostProxy::global(cx),
        language_registry.clone(),
    );

    let registry = theme::ThemeRegistry::global(cx);
    info!(
        "zed_android: theme registry has {} themes loaded",
        registry.list().len()
    );

    let app_session = cx.new(|cx| AppSession::new(session, cx));
    let app_state = Arc::new(AppState {
        languages: language_registry,
        client: client.clone(),
        user_store,
        workspace_store,
        fs: fs.clone(),
        build_window_options: minimal_window_options,
        node_runtime: node_runtime.clone(),
        session: app_session,
    });
    AppState::set_global(app_state.clone(), cx);
    info!("zed_android: AppState assembled + set as global");

    // Mirror production zed/src/main.rs:785 — without this every language's
    // tree-sitter captures parse but render with no syntax styling. Re-apply
    // on theme changes so theme toggles actually recolour text. Bracket
    // matching, indents, and outline don't depend on this and would have
    // worked already; syntax colors and rainbow brackets do.
    app_state
        .languages
        .set_theme(theme::GlobalTheme::theme(cx).clone());
    cx.observe_global::<theme::GlobalTheme>({
        let languages = app_state.languages.clone();
        move |cx| {
            languages.set_theme(theme::GlobalTheme::theme(cx).clone());
        }
    })
    .detach();

    // Adapter-aware `terminal.shell`. The picker writes runtime.toml;
    // we re-derive the spawn target every launch by asking the active
    // adapter via `RuntimeProvider::terminal_shell` so flipping the
    // adapter actually changes where the integrated terminal lands.
    //
    // alacritty's `pw_shell` lookup on Android returns /system/bin/sh
    // (the parody) so this explicit Shell::Program is what makes any
    // useful terminal work at all.
    //
    // It goes into Zed's default settings, not the user's settings.json.
    // Zed sends user settings to remote servers, and since Zed 1.25 a
    // remote terminal runs the `terminal.shell` it finds there, so an
    // Android path in settings.json broke every SSH terminal
    // (`env: '.../usr/bin/bash': No such file or directory`). Defaults
    // stay on the device and rank below settings.json, so remote
    // terminals fall back to the remote's own shell.
    let provider = active_provider(data_path);
    let shell_path = provider
        .terminal_shell(data_path)
        .unwrap_or_else(|| data_path.join("usr/bin/bash"));
    if shell_path.is_file() {
        let shell = settings::Shell::Program(shell_path.to_string_lossy().to_string());
        SettingsStore::update_global(cx, |store, cx| {
            store.update_default_settings(cx, |defaults| {
                defaults
                    .terminal
                    .get_or_insert_with(settings::TerminalSettingsContent::default)
                    .project
                    .shell = Some(shell.clone());
            });
        });
        info!(
            "zed_android: default terminal.shell -> {}",
            shell_path.display()
        );

        // Earlier versions wrote this value into settings.json, where it
        // outranks the default and still reaches remotes.
        let user_has_adapter_shell = cx
            .global::<SettingsStore>()
            .raw_user_settings()
            .and_then(|user_settings| user_settings.content.terminal.as_ref())
            .is_some_and(|terminal| terminal.project.shell.as_ref() == Some(&shell));
        if user_has_adapter_shell {
            cx.global::<SettingsStore>().update_settings_file(
                app_state.fs.clone(),
                move |content, _cx| {
                    if let Some(terminal) = content.terminal.as_mut()
                        && terminal.project.shell.as_ref() == Some(&shell)
                    {
                        terminal.project.shell = None;
                    }
                },
            );
        }
    } else {
        log::warn!(
            "zed_android: adapter-derived shell {} missing on disk; \
             leaving terminal.shell as-is",
            shell_path.display()
        );
    }

    Client::set_global(client.clone(), cx);
    zed::init(cx);
    client::init(&client, cx);
    feature_flags::FeatureFlagStore::init(cx);
    // Register the TrustedWorktrees global. git_store reads it via
    // `try_get_global` when a repo is added; without it, `is_trusted`
    // defaults to false, which routes every repo into the
    // dubious-ownership UI in git_panel.rs:4862.
    //
    // Mirror production zed/src/main.rs:450-457 — fetch the persisted
    // trust grants from `WorkspaceDb::fetch_trusted_worktrees()` so a
    // user's prior "Trust" choice survives across launches and
    // reinstalls. Previously we passed `HashMap::default()` here, which
    // wiped the in-memory trust map every boot even though the SQLite
    // db was preserving it correctly — every relaunch re-prompted the
    // restricted-mode trust dialog. Fall back to empty on fetch failure
    // (typically only happens if the db schema upgrade is mid-flight).
    let db_trusted_paths = match workspace::WorkspaceDb::global(cx).fetch_trusted_worktrees() {
        Ok(paths) => paths,
        Err(err) => {
            error!(
                "zed_android: fetch_trusted_worktrees failed at boot: \
                     {err:#} — starting with empty trust map; user will \
                     be re-prompted for any previously trusted projects"
            );
            std::collections::HashMap::default()
        }
    };
    project::trusted_worktrees::init(db_trusted_paths, cx);
    Project::init(&client, cx);

    // Extensions (Phase L3 — previously deferred). Mirrors production
    // zed/src/main.rs:623, 631–637, 741. The store creates the
    // global ExtensionStore, watches `paths::extensions_dir()` for new
    // installations, and asynchronously fetches the registry from
    // Zed's API. Theme/debug-adapter proxies register against the
    // already-installed `ExtensionHostProxy` from `extension::init`
    // earlier in boot. extensions_ui registers the workspace observer
    // that handles the `zed_actions::Extensions` action — taps from
    // the title-bar settings menu open the browse/install pane.
    //
    // Wasmtime engine init (`crates/extension_host/src/wasm_host.rs:564`)
    // currently uses Cranelift JIT. Whether that survives Android's
    // `untrusted_app_27` SELinux W^X policy is open — modern Android
    // (API 30+) typically allows anonymous executable mappings for
    // app processes, so it MAY just work; if not, the engine init
    // will panic at first extension load and we switch wasmtime's
    // strategy to Pulley interpreter (one Cargo.toml feature flip +
    // a `Config::strategy(Strategy::Pulley)` line). See
    // `deferred-render-pipeline-perf.md` philosophy: read the actual
    // failure first, don't preemptively configure for a problem that
    // may not exist.
    {
        let extension_host_proxy = extension::ExtensionHostProxy::global(cx);
        extension_host::init(
            extension_host_proxy.clone(),
            fs.clone(),
            client.clone(),
            node_runtime.clone(),
            cx,
        );
        zed::eager_load_active_theme_and_icon_theme(fs.clone(), cx);
        debug_adapter_extension::init(extension_host_proxy.clone(), cx);
        theme_extension::init(
            extension_host_proxy,
            theme::ThemeRegistry::global(cx),
            cx.background_executor().clone(),
        );
    }

    diagnostics::init(cx);
    workspace::init(app_state.clone(), cx);
    ui_prompt::init(cx);
    command_palette::init(cx);
    search::init(cx);
    lsp_locations::init(cx);
    // As in Zed's main(). The terminal panel uses this for its own buffer
    // search; editor panes get theirs from `zed::initialize_pane`.
    cx.set_global(workspace::PaneSearchBarCallbacks {
        setup_search_bar: |languages, toolbar, window, cx| {
            let search_bar = cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
            toolbar.update(cx, |toolbar, cx| {
                toolbar.add_item(search_bar, window, cx);
            });
        },
        wrap_div_with_search_actions: search::buffer_search::register_pane_search_actions,
    });
    vim::init(cx);
    // In the order of Zed's main(). Left out: audio, call and livekit
    // (collab), copilot and language_models (AI model providers: agent_ui
    // runs ACP agents only), debugger_ui and repl, and telemetry and crashes.
    go_to_line::init(cx);
    file_finder::init(cx);
    tab_switcher::init(cx);
    outline::init(cx);
    call_hierarchy::init(cx);
    project_symbols::init(cx);
    project_panel::init(cx);
    outline_panel::init(cx);
    tasks_ui::init(cx);
    snippet_provider::init(cx);
    snippets_ui::init(cx);
    image_viewer::init(cx);
    tabular_data_preview::init(cx);
    svg_preview::init(cx);
    markdown_preview::init(cx);
    encoding_selector::init(cx);
    language_selector::init(cx);
    line_ending_selector::init(cx);
    lsp_command_selector::init(cx);
    toolchain_selector::init(cx);
    theme_selector::init(cx);
    settings_profile_selector::init(cx);
    language_tools::init(cx);
    feedback::init(cx);
    // language_model::init only registers the GlobalLanguageModelRegistry —
    // no actual AI runtime spins up. git_panel.rs reads from this registry
    // for the optional commit-message generation; without it, the panel
    // panics at first paint with "no state of type
    // language_model::registry::GlobalLanguageModelRegistry exists".
    language_model::init(cx);
    // Agent Panel for external ACP agents (Claude Code, Codex, ...), in the
    // same order as crates/zed/src/main.rs. On remote projects the agent runs
    // on the remote host through the remote server, so nothing agent-related
    // is spawned on Android. language_models::init is deliberately skipped:
    // with no model providers registered, Zed's built-in agent and the inline
    // assistant have no model, and only ACP agents can run.
    let prompt_builder = PromptBuilder::load(app_state.fs.clone(), false, cx);
    // Zed counts an install as new when it creates the installation ID; the
    // app has none, and onboarding hasn't run yet on a new install.
    let is_new_install = matches!(KeyValueStore::global(cx).read_kvp(FIRST_OPEN), Ok(None));
    project::AgentRegistryStore::init_global(
        cx,
        app_state.fs.clone(),
        app_state.client.http_client(),
    );
    agent_ui::init(
        app_state.fs.clone(),
        prompt_builder,
        app_state.languages.clone(),
        is_new_install,
        false,
        cx,
    );
    zed::watch_user_agents_md(app_state.fs.clone(), cx);
    acp_tools::init(cx);
    // Also covers the git graph (commit history) view: upstream folded
    // crates/git_graph into git_ui, so its serializable item, action
    // handlers, and database domain register here.
    git_ui::init(cx);
    // Desktop creates the registry at startup; `git_hosting_providers::init`
    // reads it with `global`, which panics when it doesn't exist yet.
    git::GitHostingProviderRegistry::default_global(cx);
    git_hosting_providers::init(cx);
    // Production zed/src/main.rs:741. Registers the workspace observer
    // that handles `zed_actions::Extensions::default()` — opens the
    // browse/install/manage pane (an `ExtensionsPage` workspace item).
    // The settings-menu chevron in the Android title bar already
    // dispatches `zed_actions::Extensions` (see title_bar.rs); without
    // this init the action goes nowhere.
    extensions_ui::init(cx);
    keymap_editor::init(cx);
    inspector_ui::init(app_state.clone(), cx);
    json_schema_store::init(cx);
    recent_projects::init(cx);
    which_key::init(cx);

    // OpenSettings interception for the not-yet-configured-runtime case.
    // Terminal failures (and several other "Edit Settings" affordances
    // scattered across Zed's UI) dispatch `zed_actions::OpenSettings`,
    // which normally lands the user at the settings.json editor. Without
    // a working Android runtime that panel can't solve anything: the user
    // has to find Android Runtime > Open picker first. Skip the dead-end
    // by routing OpenSettings straight to the picker when the runtime
    // toml is missing.
    //
    // This observe_new MUST be registered BEFORE `settings_ui::init`
    // because gpui's bubble-phase action dispatch auto-stops after the
    // first listener that doesn't explicitly call `cx.propagate()`. To
    // sit at position 0 on each workspace's dispatch-node listener list
    // (so our handler runs before `settings_ui`'s) we need to register
    // our observe_new first; `register_action` appends in observe-fire
    // order. Reordering this past `settings_ui::init` silently breaks
    // the deep-link.
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(
            |_workspace: &mut Workspace, _: &zed_actions::OpenSettings, window, cx| {
                if std::path::Path::new("/data/data/com.zdroid/files/usr/etc/zd-runtime.toml")
                    .exists()
                {
                    // Runtime configured: let settings_ui's handler open
                    // the normal settings editor.
                    cx.propagate();
                    return;
                }
                log::info!(
                    "zed_android: OpenSettings routed to runtime picker (no zd-runtime.toml yet)"
                );
                runtime_picker::open_runtime_picker_window(window, cx);
            },
        );
    })
    .detach();

    settings_ui::init(cx);
    logging::init(cx);
    terminal_view::init(cx);
    // Onboarding reads `AllAgentServersSettings` from the SettingsStore;
    // `SettingsStore::get` panics if the type isn't registered, so register
    // it before any onboarding render. agent_settings doesn't expose a
    // separate init function — registering the type is the whole step.
    project::agent_server_store::AllAgentServersSettings::register(cx);
    onboarding::init(cx);
    menu_bar::register_actions(cx);
    runtime_picker::register(cx);
    info!(
        "zed_android: workspace + diagnostics + search + file_finder + outline_panel + onboarding + menu_bar init complete"
    );

    zed::initialize_workspace(app_state.clone(), cx);

    cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };

        // Custom always-on application menu bar. On Android there's no
        // native menu surface and the production `title_bar` crate pulls
        // in audio/livekit/auto_update/git_ui — too heavy for this stage.
        // The bar dispatches the same actions production's menus do, and
        // can be hidden via the chevron-up button or the
        // `ToggleAppMenuBar` action (Ctrl+Alt+M).
        let weak_workspace = cx.weak_entity();
        let menu_bar = cx.new(|inner_cx| menu_bar::MenuBar::new(weak_workspace.clone(), inner_cx));
        menu_bar::register(&menu_bar);
        // Title bar sits below the menu bar with the project name,
        // Restricted Mode trust badge, and settings chevron. The
        // chevron's tap toggles `menu_bar.hidden`; right-click /
        // two-finger tap opens the Settings/Keymap/Themes/Icon
        // Themes/Extensions dropdown.
        let title_bar = cx.new(|inner_cx| {
            title_bar::TitleBar::new(weak_workspace.clone(), menu_bar.downgrade(), inner_cx)
        });
        let header = cx.new(|_| header::Header::new(menu_bar, title_bar));
        workspace.set_titlebar_item(header.into(), window, cx);

        let soft_keyboard_button = cx.new(android_input::SoftKeyboardButton::new);
        let trackpad_mode_button = cx.new(android_input::TrackpadModeButton::new);
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(soft_keyboard_button, window, cx);
            status_bar.add_right_item(trackpad_mode_button, window, cx);
        });
    })
    .detach();

    // Check-for-Updates handler is registered per-workspace above so
    // it has Window access for prompts. The upstream
    // `auto_update::init` already ran above which set its own App-level
    // handler that defers to the Zed update server (irrelevant for
    // Android); our workspace-level handler takes precedence when the
    // menu fires the action inside a workspace context, which is the
    // only path users hit it from.

    // Production's `prompt_and_open_paths` assumes multi-window: when no
    // existing local workspace passes the `workspace_location` filter, it
    // opens a new window via `Workspace::new_local(.., None, ..)`. Single-
    // window Android rejects the second `cx.open_window`, so we replace
    // the `Open` action with the same call production makes once it's
    // already inside the right window — `MultiWorkspace::open_project`,
    // which `find_or_create_local_workspace`s and reuses the empty
    // workspace as the slot to swap. That path also dismisses any open
    // Onboarding/WelcomePage items naturally because the workspace gets
    // replaced.
    cx.on_action(|_: &workspace::Open, cx: &mut App| open_with_picker(cx));
    // Zed's default keymap binds Ctrl+O to `OpenFiles`, whose fallback handler
    // in `workspace::init` opens a new window.
    cx.on_action(|_: &workspace::OpenFiles, cx: &mut App| open_with_picker(cx));

    // ImportFromSdcard action removed in L9 — the menu entry was redundant
    // with the existing SAF picker (Open / Add Folder to Project), and the
    // copy-into-`~/projects` flow it ran is now triggered from the noexec
    // banner's confirmation dialog (see title_bar.rs::render_noexec_banner)
    // when an opened worktree turns out to live on a noexec mount.

    // From here to the end of boot(), as in Zed's main().
    cx.observe_global::<SettingsStore>({
        let http = app_state.client.http_client();
        let client = app_state.client.clone();
        move |cx| {
            for &mut window in cx.windows().iter_mut() {
                let background_appearance = cx.theme().window_background_appearance();
                window
                    .update(cx, |_, window, _| {
                        window.set_background_appearance(background_appearance)
                    })
                    .ok();
            }

            cx.set_text_rendering_mode(
                match WorkspaceSettings::get_global(cx).text_rendering_mode {
                    settings::TextRenderingMode::PlatformDefault => {
                        gpui::TextRenderingMode::PlatformDefault
                    }
                    settings::TextRenderingMode::Subpixel => gpui::TextRenderingMode::Subpixel,
                    settings::TextRenderingMode::Grayscale => gpui::TextRenderingMode::Grayscale,
                },
            );

            let new_host = &client::ClientSettings::get_global(cx).server_url;
            if &http.base_url() != new_host {
                http.set_base_url(new_host);
                if client.status().borrow().is_connected() {
                    client.reconnect(&cx.to_async());
                }
            }
        }
    })
    .detach();

    load_user_themes_in_background(fs.clone(), cx);
    watch_themes(fs.clone(), cx);

    let (current_session_id, last_session_id) = {
        let session = app_state.session.read(cx);
        (
            session.id().to_owned(),
            session.last_session_id().map(|id| id.to_owned()),
        )
    };

    let restore_task = cx.spawn({
        let app_state = app_state.clone();
        async move |cx| restore_or_create_workspace(app_state, cx).await.log_err()
    });

    cx.spawn({
        let db = WorkspaceDb::global(cx);
        let fs = app_state.fs.clone();
        async move |_cx| {
            restore_task.await;
            // The cleanup drops recent projects whose folders are missing.
            // Without the storage permission (not yet granted, denied, or
            // revoked after months unused) every folder on shared storage
            // looks missing, so wait until it's granted.
            if std::fs::read_dir("/storage/emulated/0").is_err() {
                info!("zed_android: shared storage unreadable; skipping workspace cleanup");
                return Ok(());
            }
            db.garbage_collect_workspaces(
                fs.as_ref(),
                &current_session_id,
                last_session_id.as_deref(),
            )
            .await
        }
    })
    .detach_and_log_err(cx);

    Ok(())
}

fn open_with_picker(cx: &mut App) {
    let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: true,
        multiple: false,
        prompt: None,
    });
    cx.spawn(async move |cx| {
        let picked = match paths.await {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => {
                error!("zed_android: Open picker failed: {err:#}");
                return;
            }
            Err(_) => return,
        };

        // The first-launch onboarding path (show_onboarding_view →
        // workspace::open_new) opens a plain `Workspace` window, not
        // a `MultiWorkspace`. Returning-launch path is also `Workspace`
        // until something attaches the multi-workspace shell. The Open
        // action can fire on either, so try MultiWorkspace first then
        // fall through to plain Workspace; the previous code silently
        // no-op'd ("no active MultiWorkspace for Open" log + return)
        // when invoked on the welcome screen of a fresh install.
        let active = cx.update(|cx| cx.active_window());
        let Some(active) = active else {
            error!("zed_android: no active window for Open");
            return;
        };

        if let Some(mw) = active.downcast::<MultiWorkspace>() {
            let task = mw.update(cx, |mw, window, cx| {
                mw.open_project(picked, workspace::OpenMode::Activate, window, cx)
            });
            if let Ok(task) = task {
                if let Err(err) = task.await {
                    error!("zed_android: open_project failed: {err:#}");
                }
            }
        } else if let Some(ws) = active.downcast::<workspace::Workspace>() {
            // Fall-through path: add the picked folder as a worktree
            // of the current workspace. The welcome page tab survives
            // alongside (user can close it). Not as polished as the
            // MultiWorkspace `find_or_create + replace` flow, but it
            // unblocks Open Project from the welcome screen on every
            // boot path.
            let task = ws.update(cx, |ws, window, cx| {
                ws.open_paths(picked, workspace::OpenOptions::default(), None, window, cx)
            });
            if let Ok(task) = task {
                for result in task.await.into_iter().flatten() {
                    result.log_err();
                }
            }
        } else {
            // active_window points at something that is neither a
            // Workspace nor a MultiWorkspace. The classic case on Android
            // (first-boot Open Project): the runtime picker ran in an
            // ExtraWindowActivity, the user dismissed it, and the active
            // window slot still points at that dismissed picker entity
            // when the SAF Activity-result fires. Without this fallback
            // the URI is dropped silently and the user sees "tapping Open
            // Project does nothing until I close + reopen the app".
            //
            // Fall back to the first local MultiWorkspace window. Same
            // recovery the upstream `prompt_and_open_paths` does for its
            // own scheduling.
            let fallback = cx.update(|cx| {
                workspace_windows_for_location(&SerializedWorkspaceLocation::Local, cx)
                    .into_iter()
                    .next()
            });
            if let Some(mw) = fallback {
                log::warn!(
                    "zed_android: Open action active_window was not a workspace; \
                     falling back to the first local MultiWorkspace"
                );
                let task = mw.update(cx, |mw, window, cx| {
                    mw.open_project(picked, workspace::OpenMode::Activate, window, cx)
                });
                if let Ok(task) = task {
                    if let Err(err) = task.await {
                        error!("zed_android: fallback open_project failed: {err:#}");
                    }
                }
            } else {
                error!(
                    "zed_android: no local MultiWorkspace window exists; \
                     Open action dropped"
                );
            }
        }
    })
    .detach();
}
