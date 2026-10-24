use sha2::{Digest, Sha256};
use std::{cmp::Ordering, io::Read};

pub fn sha256(path: &Path) -> Result<String, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("读取 {} 失败：{error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let parse = |version: &str| {
        version
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let left = parse(left);
    let right = parse(right);
    (0..left.len().max(right.len()))
        .map(|index| {
            left.get(index)
                .unwrap_or(&0)
                .cmp(right.get(index).unwrap_or(&0))
        })
        .find(|order| *order != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

pub fn replace_file(stage: &Path, target: &Path) -> Result<(), String> {
    fs::rename(stage, target).map_err(|error| format!("替换 {} 失败：{error}", target.display()))
}

/// Failed operation directories kept for diagnosis; successful ones remove themselves.
pub const KEPT_FAILED_RUNS: usize = 2;

// Leftovers older than this cannot belong to a download or operation that is still running.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub fn is_stale(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= STALE_AFTER)
}

// Cache cleanup is best effort: callers log what could not be removed and never
// fail an operation over it. A link entry is skipped, never followed, and
// `read_cleanup_dir` refuses to list a directory reached through a link.
pub fn remove_entry(path: &Path) -> Result<(), String> {
    fs::symlink_metadata(path)
        .and_then(|metadata| {
            if metadata.file_type().is_symlink() {
                Ok(())
            } else if metadata.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        })
        .map_err(|error| format!("清理 {} 失败：{error}", path.display()))
}

/// Removes a finished operation's directory. An elevated helper may still be
/// exiting with this directory as its working directory, so retry briefly.
pub fn remove_work_dir(path: &Path) -> Result<(), String> {
    let mut result = remove_entry(path);
    for _ in 0..3 {
        if result.is_ok() || !path.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        result = remove_entry(path);
    }
    result
}

/// Lists `dir` for cleanup only when every path from the trusted data `root`
/// down to `dir` is a real directory. A junction or symlink anywhere below the
/// root could otherwise lead cleanup into unrelated files. A missing directory
/// has nothing to clean; any other problem is reported in `errors`.
fn read_cleanup_dir(root: &Path, dir: &Path, errors: &mut Vec<String>) -> Vec<fs::DirEntry> {
    let Ok(relative) = dir.strip_prefix(root) else {
        errors.push(format!("跳过清理 {}：不在助手数据目录内", dir.display()));
        return Vec::new();
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            errors.push(format!("跳过清理 {}：路径无效", dir.display()));
            return Vec::new();
        };
        current.push(name);
        let problem = match fs::symlink_metadata(&current) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => format!("读取 {} 失败：{error}", current.display()),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                format!("{} 是链接", current.display())
            }
            Ok(metadata) if !metadata.is_dir() => format!("{} 不是目录", current.display()),
            Ok(_) => continue,
        };
        errors.push(format!("跳过清理 {}：{problem}", dir.display()));
        return Vec::new();
    }
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| {
                entry
                    .map_err(|error| errors.push(format!("读取 {} 失败：{error}", dir.display())))
                    .ok()
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            errors.push(format!("读取 {} 失败：{error}", dir.display()));
            Vec::new()
        }
    }
}

fn entry_name(entry: &fs::DirEntry) -> String {
    entry.file_name().to_string_lossy().into_owned()
}

/// Removes the entries directly under `dir` whose names match.
pub fn remove_entries(root: &Path, dir: &Path, matches: impl Fn(&str) -> bool) -> Vec<String> {
    let mut errors = Vec::new();
    for entry in read_cleanup_dir(root, dir, &mut errors) {
        if matches(&entry_name(&entry)) {
            errors.extend(remove_entry(&entry.path()).err());
        }
    }
    errors
}

/// Keeps the `keep` most recently modified matching entries and removes the rest.
pub fn keep_newest_entries(
    root: &Path,
    dir: &Path,
    matches: impl Fn(&str) -> bool,
    keep: usize,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut found = read_cleanup_dir(root, dir, &mut errors)
        .into_iter()
        .filter(|entry| matches(&entry_name(entry)))
        .map(|entry| {
            let modified = fs::symlink_metadata(entry.path())
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, entry.path())
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in found.into_iter().skip(keep) {
        errors.extend(remove_entry(&path).err());
    }
    errors
}

/// Ends a work directory under `runs` once its outcome is known. A successful
/// run removes its own; retention runs afterwards so this run's failure counts.
pub fn finish_work_dir(root: &Path, runs: &Path, work: &Path, succeeded: bool) -> Vec<String> {
    let mut errors = Vec::new();
    if succeeded {
        errors.extend(remove_work_dir(work).err());
    }
    errors.extend(keep_newest_entries(root, runs, |_| true, KEPT_FAILED_RUNS));
    errors
}

/// Removes cached `{prefix}<version>{suffix}` downloads except `keep`, or the
/// newest cached version when `keep` is not given. Partial downloads are
/// removed once stale; a failed download already removes its own.
pub fn prune_versioned_downloads(
    root: &Path,
    dir: &Path,
    prefix: &str,
    suffix: &str,
    keep: Option<&str>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let entries = read_cleanup_dir(root, dir, &mut errors);
    let version = |name: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .map(str::to_owned)
    };
    let keep = keep.map(str::to_owned).or_else(|| {
        entries
            .iter()
            .filter_map(|entry| version(&entry_name(entry)))
            .max_by(|left, right| compare_versions(left, right))
    });
    let partial = format!("{suffix}.partial");
    for entry in entries {
        let name = entry_name(&entry);
        let remove = match version(&name) {
            Some(found) => keep.as_deref() != Some(found.as_str()),
            None => name.starts_with(prefix) && name.ends_with(&partial) && is_stale(&entry.path()),
        };
        if remove {
            errors.extend(remove_entry(&entry.path()).err());
        }
    }
    errors
}

/// Startup cleanup has no operation log, so its latest problems are kept next
/// to the data they concern; a clean startup removes the old record.
pub fn record_cleanup_errors(root: &Path, errors: &[String]) {
    let log = root.join("cleanup-errors.log");
    let _ = if errors.is_empty() {
        fs::remove_file(log)
    } else {
        fs::write(log, errors.join("\n"))
    };
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn replacement_overwrites_existing_file_and_failure_preserves_it() {
        let root = std::env::temp_dir().join(format!(
            "claude-file-replace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let stage = root.join("stage");
        let target = root.join("target");
        fs::write(&target, b"old").unwrap();
        fs::write(&stage, b"new").unwrap();
        replace_file(&stage, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!stage.exists());
        assert!(replace_file(&stage, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new");
        fs::remove_dir_all(root).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    // A Windows junction needs no privilege, unlike a directory symlink.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn versioned_downloads_keep_one_version_and_only_fresh_partials() {
        let root = scratch("claude-download-prune");
        let dir = root.join("downloads");
        fs::create_dir(&dir).unwrap();
        for name in [
            "Claude-1.0.msix",
            "Claude-2.9.1.msix",
            "Claude-2.10.0.msix",
            "Claude-3.0.msix.partial",
            "Claude-0.9.msix.partial",
            "notes.txt",
        ] {
            fs::write(dir.join(name), name).unwrap();
        }
        fs::File::options()
            .write(true)
            .open(dir.join("Claude-0.9.msix.partial"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - STALE_AFTER * 2)
            .unwrap();
        // Versions compare numerically, and a fresh partial may still be downloading.
        assert!(prune_versioned_downloads(&root, &dir, "Claude-", ".msix", None).is_empty());
        assert_eq!(
            names(&dir),
            ["Claude-2.10.0.msix", "Claude-3.0.msix.partial", "notes.txt"]
        );
        assert!(
            prune_versioned_downloads(&root, &dir, "Claude-", ".msix", Some("2.9.1")).is_empty()
        );
        assert_eq!(names(&dir), ["Claude-3.0.msix.partial", "notes.txt"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_never_follows_links_below_the_data_root() {
        let root = scratch("claude-cleanup-links");
        let data = root.join("data");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("sub")).unwrap();
        for file in ["Claude-1.0.msix", "unrelated.zip", "sub/unrelated.zip"] {
            fs::write(outside.join(file), file).unwrap();
        }
        fs::create_dir_all(data.join("real")).unwrap();
        fs::write(data.join("real/old.zip"), "ours").unwrap();
        link_dir(&outside, &data.join("linked"));
        link_dir(&outside, &data.join("parent"));

        // The cleanup directory itself is a link.
        let errors = remove_entries(&data, &data.join("linked"), |_| true);
        assert!(errors.len() == 1 && errors[0].contains("链接"), "{errors:?}");
        let errors = keep_newest_entries(&data, &data.join("linked"), |_| true, 0);
        assert!(errors.len() == 1 && errors[0].contains("链接"), "{errors:?}");
        let errors = prune_versioned_downloads(&data, &data.join("linked"), "Claude-", ".msix", None);
        assert!(errors.len() == 1 && errors[0].contains("链接"), "{errors:?}");
        // An intermediate path below the data root is a link.
        let errors = remove_entries(&data, &data.join("parent").join("sub"), |_| true);
        assert!(errors.len() == 1 && errors[0].contains("链接"), "{errors:?}");
        assert_eq!(names(&outside), ["Claude-1.0.msix", "sub", "unrelated.zip"]);
        assert_eq!(names(&outside.join("sub")), ["unrelated.zip"]);

        // An ordinary cache directory is still cleaned, even beside a link.
        assert!(remove_entries(&data, &data.join("real"), |name| name.ends_with(".zip")).is_empty());
        assert!(names(&data.join("real")).is_empty());
        // Link entries are skipped: neither followed nor removed.
        assert!(remove_entries(&data, &data, |name| name != "real").is_empty());
        assert_eq!(names(&data), ["linked", "parent", "real"]);
        assert_eq!(names(&outside), ["Claude-1.0.msix", "sub", "unrelated.zip"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finished_work_dirs_keep_two_failures_and_a_success_removes_only_itself() {
        let root = scratch("claude-finish-work");
        let runs = root.join("runs");
        let mut index = 0;
        let mut run = |succeeded: bool| {
            index += 1;
            std::thread::sleep(std::time::Duration::from_millis(20));
            let work = runs.join(index.to_string());
            fs::create_dir_all(&work).unwrap();
            fs::write(work.join("close-desktop.ps1"), "x").unwrap();
            finish_work_dir(&root, &runs, &work, succeeded)
        };
        for _ in 0..3 {
            assert!(run(false).is_empty());
        }
        assert_eq!(names(&runs), ["2", "3"]);
        assert!(run(true).is_empty());
        assert_eq!(names(&runs), ["2", "3"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_reports_unreadable_directories_but_not_missing_ones() {
        let root = scratch("claude-cleanup-errors");
        assert!(remove_entries(&root, &root.join("missing"), |_| true).is_empty());
        assert!(remove_entries(&root, &root.join("missing/deeper"), |_| true).is_empty());
        fs::write(root.join("file"), "x").unwrap();
        let errors = remove_entries(&root, &root.join("file"), |_| true);
        assert!(
            errors.len() == 1 && errors[0].contains("不是目录") && errors[0].contains("file"),
            "{errors:?}"
        );
        let errors = remove_entries(&root, &std::env::temp_dir(), |_| true);
        assert!(
            errors.len() == 1 && errors[0].contains("不在助手数据目录内"),
            "{errors:?}"
        );
        assert!(root.join("file").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
