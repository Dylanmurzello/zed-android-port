//! Installs the bundled Ubuntu base image into a temp dir. Needs the
//! tarball on disk (gradle's `fetchRootfsImage` downloads it to
//! `android/app/build/rootfs/`), so ignored by default:
//! `ZD_ROOTFS_IMAGE=<path> cargo test -p zdroid_runtime -- --ignored`.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zdroid_runtime::progress::{LogProgressSink, ProgressSink};
use zdroid_runtime::runtime::{Runtime, VERSION_FILE};

fn absolute_symlinks(root: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap();
            if target.is_absolute() {
                found.push(format!("{} -> {}", path.display(), target.display()));
            }
        } else if meta.is_dir() {
            absolute_symlinks(&path, found);
        }
    }
}

struct CancelAfter {
    inner: LogProgressSink,
    calls: std::cell::Cell<u32>,
    limit: u32,
}

impl ProgressSink for CancelAfter {
    fn step(&mut self, label: &str) {
        self.inner.step(label);
    }
    fn progress(&mut self, done: u64, total: u64) {
        self.inner.progress(done, total);
        self.calls.set(self.calls.get() + 1);
    }
    fn warn(&mut self, message: &str) {
        self.inner.warn(message);
    }
    fn is_cancelled(&self) -> bool {
        self.calls.get() >= self.limit
    }
}

#[test]
#[ignore]
fn installs_the_bundled_image() {
    let image_path =
        std::env::var_os("ZD_ROOTFS_IMAGE").expect("ZD_ROOTFS_IMAGE must name the tarball");
    let data = tempfile::tempdir().unwrap();
    let data_path = data.path();
    fs::create_dir_all(data_path.join("bin")).unwrap();
    fs::write(data_path.join("bin/zdload"), b"stand-in").unwrap();
    let runtime = Runtime::new(data_path);
    let open_image = || File::open(&image_path).unwrap();

    // A cancelled extraction must leave nothing behind but the bin dir.
    let mut cancelling = CancelAfter {
        inner: LogProgressSink,
        calls: std::cell::Cell::new(0),
        limit: 3,
    };
    let error = zdroid_runtime::install::install(
        &runtime,
        &mut open_image(),
        &["1.1.1.1".into()],
        "Etc/UTC",
        &mut cancelling,
    )
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<zdroid_runtime::install::Cancelled>()
            .is_some(),
        "expected Cancelled, got {error:#}"
    );
    let leftovers: Vec<_> = fs::read_dir(data_path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        leftovers,
        vec!["bin".to_string()],
        "cancel left {leftovers:?}"
    );

    let mut sink = LogProgressSink;
    zdroid_runtime::install::install(
        &runtime,
        &mut open_image(),
        &["1.1.1.1".into(), "8.8.8.8".into()],
        "America/New_York",
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_link(runtime.rootfs_dir().join("etc/localtime")).unwrap(),
        std::path::Path::new("../usr/share/zoneinfo/America/New_York")
    );
    assert_eq!(
        std::fs::read_to_string(runtime.rootfs_dir().join("etc/timezone")).unwrap(),
        "America/New_York\n"
    );

    let rootfs = runtime.rootfs_dir();
    let version = fs::read_to_string(rootfs.join(VERSION_FILE)).unwrap();
    let image = zdroid_runtime::install::image();
    assert_eq!(version, format!("{}\n{}\n", image.name, image.sha256));
    assert_eq!(
        runtime.installed_image().map(|installed| installed.sha256),
        Some(image.sha256.clone())
    );
    assert!(!rootfs.with_file_name("rootfs.staging").exists());

    let mut found = Vec::new();
    absolute_symlinks(&rootfs, &mut found);
    let allowed: Vec<_> = found
        .iter()
        .filter(|line| !line.contains("-> /dev/") && !line.contains("-> /proc/"))
        .cloned()
        .collect();
    assert!(
        allowed.is_empty(),
        "absolute symlinks outside bindings: {allowed:#?}"
    );

    assert_eq!(
        fs::read_link(rootfs.join("bin")).unwrap(),
        Path::new("usr/bin")
    );
    // ubuntu-base ships no sudo; passwd and su are its setuid binaries.
    for setuid in ["usr/bin/passwd", "usr/bin/su"] {
        let meta = fs::metadata(rootfs.join(setuid)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o4755, "{setuid} mode");
    }
    let ls = fs::metadata(rootfs.join("usr/bin/ls")).unwrap();
    assert_eq!(ls.permissions().mode() & 0o111, 0o111, "ls exec bits");
    assert!(fs::symlink_metadata(rootfs.join("lib/ld-linux-aarch64.so.1")).is_ok());
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/resolv.conf")).unwrap(),
        "nameserver 1.1.1.1\nnameserver 8.8.8.8\n"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/hostname")).unwrap(),
        "zdroid\n"
    );
    assert_eq!(
        fs::metadata(rootfs.join("tmp"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o1777
    );
    // Every alternatives link under usr/bin was absolute in the archive
    // and must now climb relatively.
    let mut alternatives = 0usize;
    for entry in fs::read_dir(rootfs.join("usr/bin")).unwrap() {
        let path = entry.unwrap().path();
        if let Ok(target) = fs::read_link(&path)
            && target.to_string_lossy().contains("etc/alternatives/")
        {
            alternatives += 1;
            assert!(
                target.starts_with("../../etc/alternatives"),
                "{} -> {}",
                path.display(),
                target.display()
            );
        }
    }
    assert!(
        alternatives > 0,
        "no alternatives links found under usr/bin"
    );
    eprintln!("alternatives links checked: {alternatives}");

    // A reset over an existing tree swaps and leaves no .old behind.
    zdroid_runtime::install::install(&runtime, &mut open_image(), &["1.1.1.1".into()], "Etc/UTC", &mut sink)
        .unwrap();
    assert!(!rootfs.with_file_name("rootfs.old").exists());
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/resolv.conf")).unwrap(),
        "nameserver 1.1.1.1\n"
    );
}
