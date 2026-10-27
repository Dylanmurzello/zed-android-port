#![cfg(unix)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Copies an Android shared-storage project to a new directory in the runtime's
/// projects folder. Cancellation and errors remove only this import's files.
pub fn import_project(
    source: &Path,
    projects_root: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<PathBuf>> {
    if !source.is_dir() {
        anyhow::bail!("{} is not a directory", source.display());
    }
    let basename = source
        .file_name()
        .context("Project has no directory name")?;
    std::fs::create_dir_all(projects_root)
        .with_context(|| format!("Create {}", projects_root.display()))?;

    // A shared-storage symlink can point back into ~/projects on Android.
    // Reject it before creating a destination that the walk would revisit.
    if projects_root
        .canonicalize()?
        .starts_with(source.canonicalize()?)
    {
        anyhow::bail!("The destination is inside the source project");
    }

    let mut destination = projects_root.join(basename);
    let mut suffix = 1;
    loop {
        // create_dir reserves the name atomically: two Android windows can
        // import the same project without sharing a cleanup destination.
        match std::fs::create_dir(&destination) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut name = basename.to_os_string();
                name.push(if suffix == 1 {
                    "-imported".to_string()
                } else {
                    format!("-imported-{suffix}")
                });
                destination = projects_root.join(name);
                suffix += 1;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Create {}", destination.display()));
            }
        }
    }

    let result = copy_tree(source, &destination, cancel);
    if matches!(result, Ok(true)) {
        return Ok(Some(destination));
    }
    std::fs::remove_dir_all(&destination).with_context(|| {
        format!(
            "Remove incomplete import {} (copy result: {result:?})",
            destination.display()
        )
    })?;
    result.map(|_| None)
}

// Zed's fs::copy_recursive follows directory symlinks and has no cancellation
// hook. Android project imports must preserve links and allow cancellation.
fn copy_tree(
    source: &Path,
    destination: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<bool> {
    use std::sync::atomic::Ordering;

    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        for entry in
            std::fs::read_dir(&source).with_context(|| format!("Read {}", source.display()))?
        {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let entry = entry.with_context(|| format!("Read entry in {}", source.display()))?;
            let source = entry.path();
            let destination = destination.join(entry.file_name());
            let file_type = entry
                .file_type()
                .with_context(|| format!("Read metadata for {}", source.display()))?;
            if file_type.is_symlink() {
                let target = std::fs::read_link(&source)
                    .with_context(|| format!("Read link {}", source.display()))?;
                std::os::unix::fs::symlink(target, &destination)
                    .with_context(|| format!("Create link {}", destination.display()))?;
            } else if file_type.is_dir() {
                std::fs::create_dir(&destination)
                    .with_context(|| format!("Create {}", destination.display()))?;
                stack.push((source, destination));
            } else if file_type.is_file() {
                std::fs::copy(&source, &destination).with_context(|| {
                    format!("Copy {} to {}", source.display(), destination.display())
                })?;
            } else {
                anyhow::bail!("Cannot copy special file {}", source.display());
            }
        }
    }
    Ok(!cancel.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::{fs::symlink, net::UnixListener};
    use std::sync::{Barrier, atomic::AtomicBool};

    #[test]
    fn copies_files_and_preserves_links_without_following_them() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("nested"))?;
        fs::write(source.join("nested/file"), "contents")?;
        symlink(".", source.join("cycle"))?;
        symlink("missing", source.join("dangling"))?;
        let destination = import_project(
            &source,
            &directory.path().join("projects"),
            &AtomicBool::new(false),
        )?
        .context("completed import")?;
        assert_eq!(
            fs::read_to_string(destination.join("nested/file"))?,
            "contents"
        );
        assert_eq!(fs::read_link(destination.join("cycle"))?, Path::new("."));
        assert_eq!(
            fs::read_link(destination.join("dangling"))?,
            Path::new("missing")
        );
        Ok(())
    }

    #[test]
    fn concurrent_imports_do_not_overwrite_existing_projects() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let projects = directory.path().join("projects");
        fs::create_dir(&source)?;
        fs::write(source.join("file"), "new")?;
        fs::create_dir_all(projects.join("source"))?;
        fs::write(projects.join("source/file"), "existing")?;
        let barrier = Barrier::new(2);
        let cancel = AtomicBool::new(false);
        let destinations = std::thread::scope(|scope| {
            let copy = || {
                barrier.wait();
                import_project(&source, &projects, &cancel)
            };
            let first = scope.spawn(copy);
            let second = copy()?;
            let first = first
                .join()
                .map_err(|_| anyhow::anyhow!("import thread panicked"))??;
            anyhow::Ok((
                first.context("first import")?,
                second.context("second import")?,
            ))
        })?;
        assert_ne!(destinations.0, destinations.1);
        assert_eq!(
            fs::read_to_string(projects.join("source/file"))?,
            "existing"
        );
        for destination in [destinations.0, destinations.1] {
            assert_eq!(fs::read_to_string(destination.join("file"))?, "new");
        }
        Ok(())
    }

    #[test]
    fn failed_import_removes_only_its_own_destination() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let projects = directory.path().join("projects");
        fs::create_dir(&source)?;
        fs::write(source.join("file"), "original")?;
        let _socket = UnixListener::bind(source.join("socket"))?;
        fs::create_dir_all(projects.join("source"))?;
        fs::write(projects.join("source/file"), "existing")?;
        assert!(import_project(&source, &projects, &AtomicBool::new(false)).is_err());
        assert!(!projects.join("source-imported").exists());
        assert_eq!(fs::read_to_string(source.join("file"))?, "original");
        assert_eq!(
            fs::read_to_string(projects.join("source/file"))?,
            "existing"
        );
        Ok(())
    }

    #[test]
    fn cancelled_import_removes_its_destination() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let projects = directory.path().join("projects");
        fs::create_dir(&source)?;
        fs::write(source.join("file"), "original")?;
        assert!(import_project(&source, &projects, &AtomicBool::new(true))?.is_none());
        assert_eq!(fs::read_dir(&projects)?.count(), 0);
        assert_eq!(fs::read_to_string(source.join("file"))?, "original");
        Ok(())
    }

    #[test]
    fn rejects_destination_within_source_including_symlink_aliases() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let alias = directory.path().join("alias");
        fs::create_dir(&source)?;
        symlink(&source, &alias)?;
        assert!(import_project(&source, &alias.join("projects"), &AtomicBool::new(false)).is_err());
        assert!(!source.join("projects/source").exists());
        Ok(())
    }
}
