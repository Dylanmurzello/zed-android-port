//! `zdroid_runtime::EngineSelfTest`: run the rootfs engine loader from
//! inside the app process and log what it reports.
//!
//! This is the only representative test vehicle: `adb shell run-as`
//! runs in a different SELinux domain and the chroot terminal runs as
//! root, so neither sees the seccomp filters and permissions a real
//! spawn from Zed gets.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, actions};
use workspace::Workspace;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};
use zdroid_runtime::Runtime;

actions!(
    zdroid_runtime,
    [
        /// Run the rootfs engine self-test and log the results.
        EngineSelfTest,
    ]
);

pub fn register(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(run);
        },
    )
    .detach();
}

fn run(
    _workspace: &mut Workspace,
    _: &EngineSelfTest,
    _window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
) {
    let data_path = zdroid_runtime::data_path();
    let log_path = paths::logs_dir().join("engine-self-test.log");
    log::info!(
        "engine_self_test: starting; report at {}",
        log_path.display()
    );
    let task = cx.background_spawn(async move { run_steps(&data_path, &log_path) });
    cx.spawn(async move |workspace, cx| {
        let report = task.await;
        let summary = report.summary();
        log::info!("engine_self_test: {summary}");
        workspace
            .update(cx, |workspace, cx| {
                struct EngineSelfTestNotification;
                workspace.show_notification(
                    NotificationId::unique::<EngineSelfTestNotification>(),
                    cx,
                    |cx| cx.new(|cx| MessageNotification::new(summary.clone(), cx)),
                );
            })
            .ok();
    })
    .detach();
}

struct Report {
    passed: usize,
    total: usize,
    lines: Vec<String>,
}

impl Report {
    fn summary(&self) -> String {
        format!(
            "Engine self-test: {}/{} steps ok. Details in logs/engine-self-test.log",
            self.passed, self.total
        )
    }

    fn record(&mut self, name: &str, ok: bool, detail: String) {
        self.total += 1;
        if ok {
            self.passed += 1;
        }
        let line = format!("{name}: {} {detail}", if ok { "ok" } else { "FAIL" });
        log::info!("engine_self_test: {line}");
        self.lines.push(line);
    }
}

fn loader_command(runtime: &Runtime, program: &str, args: &[&str]) -> Command {
    let mut command = runtime.command(program);
    command.args(args).stdin(Stdio::null());
    command
}

/// Median and 95th percentile of `runs` wall-clock timings of `command`
/// after one untimed cold run; `None` carries the first failure.
fn time_runs(mut make: impl FnMut() -> Command, runs: usize) -> Result<(f64, f64, f64), String> {
    let mut samples_ms: Vec<f64> = Vec::with_capacity(runs);
    for index in 0..=runs {
        let started = Instant::now();
        let output = make().output();
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        match output {
            Ok(output) if output.status.success() => {
                if index > 0 {
                    samples_ms.push(elapsed);
                }
            }
            Ok(output) => {
                return Err(format!(
                    "exit {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => return Err(format!("spawn: {error}")),
        }
    }
    samples_ms.sort_by(|a, b| a.total_cmp(b));
    Ok((
        samples_ms[0],
        samples_ms[samples_ms.len() / 2],
        samples_ms[samples_ms.len() * 95 / 100],
    ))
}

fn run_steps(data_path: &Path, log_path: &Path) -> Report {
    let runtime = Runtime::new(data_path);
    let mut report = Report {
        passed: 0,
        total: 0,
        lines: Vec::new(),
    };

    let loader = runtime.loader_path();
    match std::fs::read_link(&loader)
        .and_then(|target| std::fs::metadata(&target).map(|meta| (target, meta)))
    {
        Ok((target, meta)) => {
            use std::os::unix::fs::PermissionsExt;
            let executable = meta.permissions().mode() & 0o111 != 0;
            report.record(
                "loader",
                meta.is_file() && executable,
                format!(
                    "{} -> {} size={} mode={:o}",
                    loader.display(),
                    target.display(),
                    meta.len(),
                    meta.permissions().mode() & 0o777
                ),
            );
        }
        Err(error) => report.record("loader", false, format!("{}: {error}", loader.display())),
    }

    match std::fs::read_to_string(runtime.version_file()) {
        Ok(version) => report.record("rootfs", true, version.replace('\n', " ")),
        Err(error) => report.record(
            "rootfs",
            false,
            format!("{}: {error}", runtime.version_file().display()),
        ),
    }

    if report.passed == 2 {
        run_true_timing(&runtime, &mut report);
        run_loader_selftest(&runtime, &mut report);
        run_shell_builtins(&runtime, data_path, &mut report);
    }

    let mut text = String::new();
    for line in &report.lines {
        let _ = writeln!(text, "{line}");
    }
    if let Some(parent) = log_path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        log::warn!("engine_self_test: create {}: {error}", parent.display());
    }
    if let Err(error) = std::fs::write(log_path, text) {
        log::warn!("engine_self_test: write {}: {error}", log_path.display());
    }
    report
}

/// `/bin/true` through the loader against `/system/bin/true` spawned the
/// same way: Zed's `Command` forks this large process, which costs the
/// same for both, so the difference is the engine's own start-up.
fn run_true_timing(runtime: &Runtime, report: &mut Report) {
    let baseline = time_runs(
        || {
            let mut command = Command::new("/system/bin/true");
            command.stdin(Stdio::null());
            command
        },
        20,
    );
    let engine = time_runs(|| loader_command(runtime, "/bin/true", &[]), 20);
    match (baseline, engine) {
        (Ok((base_min, base_median, base_p95)), Ok((min, median, p95))) => {
            let delta = median - base_median;
            report.record(
                "true",
                delta < 10.0,
                format!(
                    "engine min={min:.2}ms median={median:.2}ms p95={p95:.2}ms; \
                     system true min={base_min:.2}ms median={base_median:.2}ms p95={base_p95:.2}ms; \
                     engine overhead median={delta:.2}ms"
                ),
            );
        }
        (Err(error), _) => {
            report.record("true", false, format!("baseline /system/bin/true: {error}"))
        }
        (_, Err(error)) => report.record("true", false, error),
    }
}

fn run_loader_selftest(runtime: &Runtime, report: &mut Report) {
    let started = Instant::now();
    let mut command = loader_command(runtime, "/bin/ls", &["-la", "/"]);
    command.env(zd_engine_core::env::SELFTEST, "1");
    let output = command.output();
    let elapsed = started.elapsed();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                log::info!("engine_self_test: {line}");
                report.lines.push(format!("  {line}"));
            }
            for line in stdout.lines() {
                report.lines.push(format!("  ls: {line}"));
            }
            let listed = ["bin", "etc", "usr", "var"]
                .iter()
                .all(|name| stdout.contains(name));
            let ok = output.status.success() && stderr.contains("result=PASS") && listed;
            report.record(
                "selftest",
                ok,
                format!(
                    "exit={:?} {}ms stdout_lines={} ",
                    output.status.code(),
                    elapsed.as_millis(),
                    stdout.lines().count()
                ),
            );
        }
        Err(error) => report.record("selftest", false, format!("spawn: {error}")),
    }
}

fn run_shell_builtins(runtime: &Runtime, data_path: &Path, report: &mut Report) {
    let cwd: PathBuf = data_path.join("home");
    let script =
        "echo HOME=$HOME; echo USER=$USER; pwd; read host < /etc/hostname; echo hostname=$host";
    let run = runtime.run_captured(
        "/bin/sh",
        &["-c", script],
        Some(&cwd),
        Duration::from_secs(10),
    );
    match run {
        Ok(run) => {
            let stdout = run.stdout_text();
            for line in stdout.lines() {
                report.lines.push(format!("  sh: {line}"));
            }
            for line in run.stderr_text().lines() {
                report.lines.push(format!("  sh stderr: {line}"));
            }
            let ok = run.succeeded()
                && stdout.contains("HOME=/root")
                && stdout.contains("hostname=zdroid");
            report.record(
                "sh",
                ok,
                format!(
                    "exit={:?} timed_out={} {}ms",
                    run.code,
                    run.timed_out,
                    run.elapsed.as_millis()
                ),
            );
        }
        Err(error) => report.record("sh", false, format!("{error:#}")),
    }
}
