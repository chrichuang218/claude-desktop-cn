use crate::{
    claude::{self, ClaudePackage},
    engine_cache::{self, download_engine},
    operation::{OperationOutcome, OperationState},
    util,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

const ENGINE_URL: &str =
    "https://codeload.github.com/javaht/claude-desktop-zh-cn/zip/refs/heads/main";
const ENGINE_DIR: &str = "patch-engine";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupFile {
    relative_path: String,
    sha256: Option<String>,
    link: Option<String>,
    #[cfg(unix)]
    mode: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    package_full_name: String,
    version: String,
    install_location: String,
    backup_set: String,
    files: Vec<BackupFile>,
    applied_mode: Option<String>,
    engine_sha256: String,
}

pub struct PatchState {
    pub applied_mode: Option<String>,
    pub backup_ready: bool,
    pub external_localization: bool,
    pub recovery_required: bool,
    /// Mode recorded for an older Claude version that an official update replaced.
    pub previous_mode: Option<String>,
}

fn record(name: &str) -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join(name))
}

fn read_manifest() -> Result<Option<BackupManifest>, String> {
    let path = record("patch-backup.json")?;
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map(Some)
        .map_err(|e| format!("补丁备份记录无效：{e}"))
}

fn save_manifest(manifest: &BackupManifest) -> Result<(), String> {
    write_json(&record("patch-backup.json")?, manifest)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    write_bytes(
        path,
        &serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("记录目录无效。")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let stage = path.with_extension(format!("{}-{stamp}.partial", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&stage).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        util::replace_file(&stage, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(stage);
    }
    result
}

fn matches_package(manifest: &BackupManifest, package: &ClaudePackage) -> bool {
    manifest.package_full_name == package.package_full_name
        && manifest.version == package.version
        && manifest.install_location == package.install_location
}

fn localized(app: &Path) -> bool {
    ["zh-CN", "zh-TW", "zh-HK"].iter().any(|lang| {
        app.join(format!("Contents/Resources/ion-dist/i18n/{lang}.json"))
            .exists()
            || app.join(format!("Contents/Resources/{lang}.json")).exists()
    })
}

fn foreign_backup(app: &Path) -> Result<bool, String> {
    let parent = app.parent().ok_or("Claude 安装目录无效。")?;
    for entry in fs::read_dir(parent).map_err(|e| e.to_string())? {
        let name = entry
            .map_err(|e| e.to_string())?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("Claude.backup-before-zh-CN-") && name.ends_with(".app") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn state(package: &ClaudePackage) -> Result<PatchState, String> {
    let pending = record("patch-pending.json")?.exists();
    let recorded = read_manifest()?;
    let previous_mode = recorded
        .as_ref()
        .filter(|m| !matches_package(m, package))
        .and_then(|m| m.applied_mode.clone());
    let manifest = recorded.filter(|m| matches_package(m, package));
    // Status polling checks backup availability. Hash the entire bundle only
    // immediately before use, otherwise each UI refresh reads gigabytes.
    let valid = manifest
        .as_ref()
        .is_some_and(|m| backup_location(m, package).is_ok());
    let has_locale = localized(&package.root());
    Ok(PatchState {
        applied_mode: if has_locale && valid && !pending {
            manifest.and_then(|m| m.applied_mode)
        } else {
            None
        },
        backup_ready: valid,
        external_localization: !valid && (has_locale || foreign_backup(&package.root())?),
        recovery_required: pending,
        previous_mode,
    })
}

fn validate_relative(relative: &str) -> Result<(), String> {
    if relative.is_empty()
        || relative.contains('\\')
        || Path::new(relative)
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err(format!("备份包含不安全路径：{relative}"));
    }
    Ok(())
}

fn inventory(root: &Path) -> Result<Vec<BackupFile>, String> {
    if fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("应用根目录不能是符号链接。".into());
    }
    let canonical_root = fs::canonicalize(root).map_err(|e| e.to_string())?;
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            let relative = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            validate_relative(&relative)?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(|e| e.to_string())?;
                if target.is_absolute()
                    || !fs::canonicalize(&path)
                        .map_err(|e| e.to_string())?
                        .starts_with(&canonical_root)
                {
                    return Err(format!("应用符号链接指向包外：{relative}"));
                }
                files.push(BackupFile {
                    relative_path: relative,
                    sha256: None,
                    link: Some(target.to_string_lossy().into_owned()),
                    #[cfg(unix)]
                    mode,
                });
            } else if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push(BackupFile {
                    relative_path: relative,
                    sha256: Some(util::sha256(&path)?),
                    link: None,
                    #[cfg(unix)]
                    mode,
                });
            } else {
                return Err("应用包包含非常规文件。".into());
            }
        }
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    if files.is_empty() {
        return Err("应用包为空。".into());
    }
    Ok(files)
}

fn backup_location(manifest: &BackupManifest, package: &ClaudePackage) -> Result<PathBuf, String> {
    if !matches_package(manifest, package) {
        return Err("备份不属于当前 Claude 版本和安装位置。".into());
    }
    let base = record("patch-backups")?;
    let backup = PathBuf::from(&manifest.backup_set);
    if backup.file_name().and_then(|s| s.to_str()) != Some("Claude.app")
        || backup.parent().and_then(Path::parent) != Some(base.as_path())
    {
        return Err("当前版本备份路径无效。".into());
    }
    let canonical_base = fs::canonicalize(&base).map_err(|e| e.to_string())?;
    let canonical = fs::canonicalize(&backup).map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canonical_base)
        || manifest.files.is_empty()
        || fs::symlink_metadata(&backup)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        || !backup.join("Contents/Info.plist").is_file()
    {
        return Err("当前版本备份路径或结构无效。".into());
    }
    Ok(backup)
}

fn validate_backup(manifest: &BackupManifest, package: &ClaudePackage) -> Result<PathBuf, String> {
    let backup = backup_location(manifest, package)?;
    let identity = claude::read_package(&backup)?;
    if identity.package_full_name != package.package_full_name
        || identity.version != package.version
    {
        return Err("备份应用内的版本与当前 Claude 版本不一致。".into());
    }
    if inventory(&backup)? != manifest.files {
        return Err("当前版本备份摘要不匹配。".into());
    }
    Ok(backup)
}

fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|e| format!("启动系统工具失败：{e}"))?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(log.trim().into())
    } else {
        Err(format!(
            "系统工具执行失败 {:?}：{log}",
            output.status.code()
        ))
    }
}

fn unique_dir(parent: &Path) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let path = parent.join(format!("run-{}-{stamp}", std::process::id()));
    fs::create_dir(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

fn python() -> Result<PathBuf, String> {
    for executable in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/Library/Frameworks/Python.framework/Versions/Current/bin/python3",
        "/usr/bin/python3",
    ] {
        if !Path::new(executable).is_file() {
            continue;
        }
        // Avoid launching the macOS developer-tools installation dialog merely
        // by probing Apple's Python placeholder.
        if executable == "/usr/bin/python3"
            && run(Command::new("/usr/bin/xcode-select").arg("-p")).is_err()
        {
            continue;
        }
        if Command::new(executable)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(PathBuf::from(executable));
        }
    }
    Err("上游 macOS 汉化引擎需要 Python 3.9 或更新版本。请安装 Python 3（python.org 或 Homebrew），然后重试。".into())
}

struct Engine {
    work: PathBuf,
    script: PathBuf,
    sha256: String,
}

/// Startup cleanup of engine leftovers from earlier versions.
pub fn clean_cache(data: &Path) -> Vec<String> {
    let engine_dir = data.join(ENGINE_DIR);
    let mut errors = engine_cache::prune_unreferenced(data, &engine_dir.join("cache"));
    match record("patch-pending.json") {
        Ok(pending) => errors.extend(engine_cache::prune_failed_runs(data, &engine_dir, &pending)),
        Err(error) => errors.push(error),
    }
    errors
}

/// Runs on every exit path after the engine was fetched, success or failure.
fn finish_engine_run(engine: &Engine, succeeded: bool, operation: &OperationState) {
    let errors = match (util::data_dir(), record("patch-pending.json")) {
        (Ok(data), Ok(pending)) => engine_cache::finish_run(
            &data,
            &data.join(ENGINE_DIR),
            &pending,
            &engine.work,
            succeeded,
        ),
        (Err(error), _) | (_, Err(error)) => vec![error],
    };
    for error in errors {
        operation.log(error);
    }
}

fn fetch_engine(python: &Path, operation: &OperationState) -> Result<Engine, String> {
    operation.step("在线检查最新汉化引擎");
    let data = util::data_dir()?;
    let engine_dir = data.join(ENGINE_DIR);
    let work = unique_dir(&engine_dir)?;
    let cache_dir = engine_dir.join("cache");
    let result = fetch_engine_into(python, &work, &cache_dir, operation);
    let errors = match &result {
        Ok(engine) => engine_cache::prune(&data, &cache_dir, &engine.sha256),
        // Nothing has run yet, so a failed download or check leaves no evidence worth keeping.
        Err(_) => util::remove_work_dir(&work).err().into_iter().collect(),
    };
    for error in errors {
        operation.log(error);
    }
    result
}

fn fetch_engine_into(
    python: &Path,
    work: &Path,
    cache_dir: &Path,
    operation: &OperationState,
) -> Result<Engine, String> {
    let adapter = work.join("assistant_adapter.py");
    fs::write(&adapter, include_str!("engine_adapter.py")).map_err(|e| e.to_string())?;
    let zip = work.join("main.zip");
    let cache = download_engine(ENGINE_URL, cache_dir, &zip, operation)?;
    let extracted = work.join("extracted");
    // Validate every ZIP member before extraction (upstream archives contain no symlinks).
    run(Command::new(python)
        .arg(&adapter)
        .arg("--extract")
        .arg(&zip)
        .arg(&extracted))?;
    let roots = fs::read_dir(&extracted)
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if roots.len() != 1 || !roots[0].is_dir() {
        return Err("最新引擎压缩包结构不兼容。".into());
    }
    let root = &roots[0];
    let script = root.join("scripts/patch_claude_zh_cn.py");
    let source = fs::read_to_string(&script).map_err(|e| format!("最新引擎缺少 Mac 脚本：{e}"))?;
    for marker in [
        "def main(",
        "def quit_claude(",
        "def set_user_locale(",
        "def verify(",
        "def backup_and_replace(",
        "--skip-asar-patch",
        "--user-home",
        "--app",
    ] {
        if !source.contains(marker) {
            return Err(format!("最新 Mac 引擎接口不兼容：缺少 {marker}"));
        }
    }
    if !root.join("resources/frontend-zh-CN.json").is_file() {
        return Err("最新引擎缺少简体中文资源。".into());
    }
    fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let stage = cache_dir.join("archive.partial");
    fs::copy(&zip, &stage).map_err(|e| e.to_string())?;
    util::replace_file(&stage, &cache_dir.join(format!("{}.zip", cache.sha256)))?;
    write_json(&cache_dir.join("metadata.json"), &cache)?;
    operation.log(format!("引擎来源：{ENGINE_URL}；SHA256：{}", cache.sha256));
    Ok(Engine {
        work: work.to_path_buf(),
        script,
        sha256: cache.sha256,
    })
}

fn run_engine(
    python: &Path,
    engine: &Engine,
    source: &Path,
    mode: &str,
    operation: &OperationState,
) -> Result<PathBuf, String> {
    let staged = engine.work.join("Claude.app");
    run(Command::new("/usr/bin/ditto").arg(source).arg(&staged))?;
    let adapter = engine.work.join("assistant_adapter.py");
    let stderr =
        fs::File::create(engine.work.join("engine-stderr.log")).map_err(|e| e.to_string())?;
    let mut child = Command::new(python)
        .arg("-u")
        .arg(&adapter)
        .arg(&engine.script)
        .arg(&staged)
        .arg(&engine.work)
        .arg(mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = (|| {
        for line in BufReader::new(child.stdout.take().ok_or("无法读取引擎输出。")?).lines()
        {
            operation.log(line.map_err(|e| e.to_string())?);
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        let errors =
            fs::read_to_string(engine.work.join("engine-stderr.log")).map_err(|e| e.to_string())?;
        if !errors.trim().is_empty() {
            operation.log(&errors);
        }
        if !status.success() {
            return Err(format!("Mac 汉化引擎失败 {:?}：{errors}", status.code()));
        }
        verify_patched(&staged)?;
        Ok(staged)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn verify_patched(app: &Path) -> Result<(), String> {
    if !app
        .join("Contents/Resources/ion-dist/i18n/zh-CN.json")
        .is_file()
    {
        return Err("脚本退出成功，但简体中文资源不存在。".into());
    }
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app))?;
    Ok(())
}

fn locale_path() -> Result<PathBuf, String> {
    Ok(util::home_dir()?.join("Library/Application Support/Claude/config.json"))
}

fn set_locale(locale: &str) -> Result<(), String> {
    let path = locale_path()?;
    let mut config: serde_json::Value = if path.exists() {
        serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("Claude 配置无效，未覆盖：{e}"))?
    } else {
        serde_json::json!({})
    };
    let object = config
        .as_object_mut()
        .ok_or("Claude 配置不是 JSON 对象，未覆盖。")?;
    object.insert("locale".into(), serde_json::Value::String(locale.into()));
    write_json(&path, &config)
}

fn restore_record(path: &Path, previous: &Option<Vec<u8>>) -> Result<(), String> {
    match previous {
        Some(bytes) => write_bytes(path, bytes),
        None if path.exists() => fs::remove_file(path).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn commit_bundle(
    source: &Path,
    package: &ClaudePackage,
    manifest: &BackupManifest,
    original: bool,
    operation: &OperationState,
) -> Result<(), String> {
    claude::close_desktop(package)?;
    let current = claude::query_package()?.ok_or("Claude 在操作期间被移除。")?;
    if current.package_full_name != package.package_full_name
        || current.install_location != package.install_location
    {
        return Err("Claude 版本或位置在操作期间发生变化，已停止。".into());
    }
    let previous_files = inventory(&package.root())?;
    let expected = inventory(source)?;
    let manifest_path = record("patch-backup.json")?;
    let previous_manifest = optional_bytes(&manifest_path)?;
    let config_path = locale_path()?;
    let previous_config = optional_bytes(&config_path)?;
    let pending = record("patch-pending.json")?;
    write_json(&pending, manifest)?;
    operation.step(if original {
        "替换并验证恢复后的应用"
    } else {
        "替换并验证汉化后的应用"
    });
    let result = claude::replace_bundle(source, &package.root(), |target| {
        if inventory(target)? != expected {
            return Err("替换后应用文件校验不一致。".into());
        }
        if original {
            claude::verify_official(target)?;
        } else {
            verify_patched(target)?;
        }
        set_locale(if original { "en-US" } else { "zh-CN" })?;
        save_manifest(manifest)
    });
    if let Ok(Some(warning)) = &result {
        operation.log(warning);
    }
    if let Err(error) = result {
        let rollback = (|| {
            if inventory(&package.root())? != previous_files {
                return Err("原应用回退校验失败".to_string());
            }
            restore_record(&manifest_path, &previous_manifest)?;
            restore_record(&config_path, &previous_config)?;
            fs::remove_file(&pending).map_err(|e| e.to_string())
        })();
        return match rollback {
            Ok(()) => Err(format!("{error}；原应用及配置已验证回退。")),
            Err(e) => Err(format!(
                "{error}；回退未能验证：{e}。已保留未完成记录，请人工核查。"
            )),
        };
    }
    fs::remove_file(pending).map_err(|e| format!("应用已完成替换，但未完成记录清理失败：{e}"))
}

pub fn apply(mode: &str, operation: &OperationState) -> Result<OperationOutcome, String> {
    if mode != "safe" {
        return Err("仅支持 Cowork 兼容汉化。".into());
    }
    if record("patch-pending.json")?.exists() {
        return Err("上次汉化未完成，请先人工核查保留的备份。".into());
    }
    let python = python()?;
    let package = claude::query_package()?.ok_or("未安装官方 Claude Desktop。")?;
    let manifest = read_manifest()?.filter(|m| matches_package(m, &package));
    if let Some(ref current) = manifest {
        validate_backup(current, &package)?;
    } else if localized(&package.root()) || foreign_backup(&package.root())? {
        return Err("检测到其他来源的中文资源或备份，助手不会接管。".into());
    }
    let engine = fetch_engine(&python, operation)?;
    let result = apply_with_engine(mode, &python, &package, manifest, &engine, operation);
    finish_engine_run(&engine, result.is_ok(), operation);
    result
}

fn apply_with_engine(
    mode: &str,
    python: &Path,
    package: &ClaudePackage,
    mut manifest: Option<BackupManifest>,
    engine: &Engine,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    if manifest.is_none() {
        operation.step("校验并备份当前版本的完整原始应用");
        claude::verify_official(&package.root())?;
        let before = inventory(&package.root())?;
        let backup = unique_dir(&record("patch-backups")?)?.join("Claude.app");
        run(Command::new("/usr/bin/ditto")
            .arg(package.root())
            .arg(&backup))?;
        if inventory(&backup)? != before || inventory(&package.root())? != before {
            return Err("备份期间应用发生变化，已停止。".into());
        }
        claude::verify_official(&backup)?;
        let backed_up = claude::read_package(&backup)?;
        let current = claude::query_package()?.ok_or("Claude 在备份期间被移除。")?;
        if backed_up.package_full_name != package.package_full_name
            || current.package_full_name != package.package_full_name
            || current.install_location != package.install_location
        {
            return Err("Claude 在备份期间发生变化，已停止。".into());
        }
        manifest = Some(BackupManifest {
            package_full_name: package.package_full_name.clone(),
            version: package.version.clone(),
            install_location: package.install_location.clone(),
            backup_set: backup.display().to_string(),
            files: before,
            applied_mode: None,
            engine_sha256: engine.sha256.clone(),
        });
        // Publish the verified backup before starting the engine so a failed
        // preparation can reuse it without creating an untracked extra copy.
        save_manifest(manifest.as_ref().ok_or("没有可用原始备份。")?)?;
    }
    let mut manifest = manifest.ok_or("没有可用原始备份。")?;
    let source = validate_backup(&manifest, package)?;
    operation.step("在独立副本应用简体中文资源");
    let staged = run_engine(python, engine, &source, mode, operation)?;
    manifest.applied_mode = Some(mode.into());
    manifest.engine_sha256 = engine.sha256.clone();
    commit_bundle(&staged, package, &manifest, false, operation)?;
    Ok(OperationOutcome::done("简体中文已应用。"))
}

pub fn restore(operation: &OperationState) -> Result<OperationOutcome, String> {
    if record("patch-pending.json")?.exists() {
        return Err("上次汉化未完成，请先人工核查保留的备份。".into());
    }
    let package = claude::query_package()?.ok_or("未安装官方 Claude Desktop。")?;
    let mut manifest = read_manifest()?.ok_or("没有本助手创建的当前版本备份，无法恢复。")?;
    let backup = validate_backup(&manifest, &package)?;
    claude::verify_official(&backup)?;
    // Keep the same online-revalidation contract as Windows even though the
    // actual restore uses our exact verified bundle, never upstream backup discovery.
    let engine = fetch_engine(&python()?, operation)?;
    manifest.applied_mode = None;
    manifest.engine_sha256 = engine.sha256.clone();
    let result = commit_bundle(&backup, &package, &manifest, true, operation)
        .map(|()| OperationOutcome::done("当前 Claude 版本的原始应用已恢复。"));
    finish_engine_run(&engine, result.is_ok(), operation);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_detects_tampering_and_refuses_escaping_links() {
        let root = unique_dir(&std::env::temp_dir().join("claude-mac-patch-test")).unwrap();
        fs::write(root.join("original"), "original").unwrap();
        let first = inventory(&root).unwrap();
        fs::write(root.join("original"), "changed").unwrap();
        assert_ne!(first, inventory(&root).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let before_permissions = inventory(&root).unwrap();
            let permissions = fs::metadata(root.join("original"))
                .unwrap()
                .permissions()
                .mode();
            fs::set_permissions(
                root.join("original"),
                fs::Permissions::from_mode(permissions ^ 0o100),
            )
            .unwrap();
            assert_ne!(before_permissions, inventory(&root).unwrap());
            std::os::unix::fs::symlink("original", root.join("valid-link")).unwrap();
            assert!(inventory(&root).is_ok());
            std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
            assert!(inventory(&root).is_err());
        }
        fs::remove_dir_all(root).unwrap();
        for path in ["", "../escape", "/etc/passwd", "a/../../b", "a\\b"] {
            assert!(validate_relative(path).is_err());
        }
    }

    #[test]
    fn failed_config_write_can_restore_existing_and_absent_records() {
        let root = unique_dir(&std::env::temp_dir().join("claude-mac-record-test")).unwrap();
        let path = root.join("config.json");
        let absent = optional_bytes(&path).unwrap();
        write_json(&path, &serde_json::json!({"locale":"zh-CN"})).unwrap();
        restore_record(&path, &absent).unwrap();
        assert!(!path.exists());
        fs::write(&path, b"original").unwrap();
        let previous = optional_bytes(&path).unwrap();
        fs::write(&path, b"changed").unwrap();
        restore_record(&path, &previous).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
        fs::remove_dir_all(root).unwrap();
    }
}
