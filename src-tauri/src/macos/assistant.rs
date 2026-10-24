use crate::{
    operation::{OperationOutcome, OperationState},
    util::{self, run_shell, shell_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const APP_NAME: &str = "Claude 中文助手.app";
const BUNDLE_ID: &str = "com.chrichuang218.claudewindowscn";
const EXECUTABLE: &str = "claude-windows-cn";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantConfig {
    pub assistant_install_mode: String,
    pub assistant_path: String,
    pub create_assistant_shortcut: bool,
    pub daily_update_check: bool,
    #[serde(default = "default_true")]
    pub auto_reapply_patch: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultPaths {
    portable: String,
    user: String,
    system: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantStatus {
    pub installed: bool,
    pub mode: Option<String>,
    pub install_path: String,
    pub version: String,
    pub shortcut_ready: bool,
    pub default_paths: DefaultPaths,
    pub update_available: Option<bool>,
    pub latest_version: Option<String>,
    pub update_check_error: Option<String>,
    pub last_update_check: Option<String>,
    /// A background check of the assistant's own version is running.
    pub update_checking: bool,
    pub release_notes: Option<String>,
    pub update_result: Option<String>,
    pub update_result_ok: Option<bool>,
    pub uninstall_result: Option<String>,
    pub uninstall_result_ok: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallManifest {
    product: String,
    mode: String,
    install_path: String,
}

fn home() -> Result<PathBuf, String> {
    util::home_dir()
}

pub(crate) fn current_bundle() -> Result<PathBuf, String> {
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let bundle = exe
        .ancestors()
        .find(|path| path.extension().is_some_and(|value| value == "app"))
        .ok_or("当前助手不在 .app 应用包内，请运行正式的 macOS 安装包。")?;
    validate_bundle(bundle, Some(env!("CARGO_PKG_VERSION")))?;
    fs::canonicalize(bundle).map_err(|error| error.to_string())
}

fn defaults() -> Result<DefaultPaths, String> {
    // Development binaries have no bundle; settings must still open in tauri dev.
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let portable = exe
        .ancestors()
        .find(|path| path.extension().is_some_and(|value| value == "app"))
        .and_then(Path::parent)
        .or_else(|| exe.parent())
        .ok_or("无法确定助手所在目录。")?;
    Ok(DefaultPaths {
        portable: portable.display().to_string(),
        user: home()?.join("Applications").display().to_string(),
        system: "/Applications".into(),
    })
}

fn config_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("config.json"))
}

fn installed_record_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("installed.json"))
}

fn installed_record() -> Result<Option<InstallManifest>, String> {
    let path = installed_record_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let manifest: InstallManifest = serde_json::from_slice(
        &fs::read(path).map_err(|error| format!("读取助手安装记录失败：{error}"))?,
    )
    .map_err(|error| format!("助手安装记录无效：{error}"))?;
    if manifest.product != "claude-windows-cn"
        || !matches!(manifest.mode.as_str(), "portable" | "user" | "system")
    {
        return Err("助手安装记录不属于本工具或安装方式无效。".into());
    }
    validate_directory(Path::new(&manifest.install_path))?;
    Ok(Some(manifest))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("记录路径无效。")?)
        .map_err(|error| error.to_string())?;
    let stage = path.with_extension("json.tmp");
    fs::write(
        &stage,
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    util::replace_file(&stage, path)
}

pub fn load_config() -> Result<AssistantConfig, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AssistantConfig {
            assistant_install_mode: "user".into(),
            assistant_path: defaults()?.user,
            create_assistant_shortcut: true,
            daily_update_check: true,
            auto_reapply_patch: true,
        });
    }
    let config: AssistantConfig = serde_json::from_slice(
        &fs::read(path).map_err(|error| format!("读取助手设置失败：{error}"))?,
    )
    .map_err(|error| format!("助手设置格式无效：{error}"))?;
    validate_config(&config)?;
    Ok(config)
}

pub fn save_config(config: AssistantConfig) -> Result<AssistantConfig, String> {
    validate_config(&config)?;
    let was_enabled = load_config()
        .map(|previous| previous.daily_update_check)
        .unwrap_or(false);
    write_json(&config_path()?, &config)?;
    if config.daily_update_check && !was_enabled {
        crate::self_update::trigger_update_check();
    }
    Ok(config)
}

fn validate_config(config: &AssistantConfig) -> Result<PathBuf, String> {
    if !matches!(
        config.assistant_install_mode.as_str(),
        "portable" | "user" | "system"
    ) {
        return Err("助手安装方式无效。".into());
    }
    validate_directory(Path::new(config.assistant_path.trim()))
}

fn validate_directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || path.to_string_lossy().contains(['\n', '\r', '\0'])
        || path
            .ancestors()
            .any(|part| part.extension().is_some_and(|ext| ext == "app"))
    {
        return Err("请选择完整的应用安装目录，不能包含 ..、控制字符或位于其他 .app 内。".into());
    }
    // Resolve existing ancestors so /tmp, a custom symlink, or a nonexistent
    // child cannot bypass the protected-directory check.
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        if fs::symlink_metadata(existing).is_ok() {
            return Err("安装路径含有失效的符号链接。".into());
        }
        suffix.push(existing.file_name().ok_or("安装目录无效。")?);
        existing = existing.parent().ok_or("安装目录无效。")?;
    }
    if !existing.is_dir() {
        return Err("安装目录的父路径不是文件夹。".into());
    }
    let mut resolved = fs::canonicalize(existing).map_err(|error| error.to_string())?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    let user_home = home()?;
    if resolved.ancestors().any(|part| {
        part.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
    }) {
        return Err("助手不能安装到其他应用包内部。".into());
    }
    if resolved == Path::new("/")
        || resolved == user_home
        || resolved == Path::new("/Users")
        || [
            "/System",
            "/Library",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/private/etc",
        ]
        .iter()
        .any(|protected| resolved.starts_with(protected))
        || resolved.starts_with(user_home.join("Library"))
    {
        return Err("助手安装目录不能是用户根目录、系统目录或应用数据目录。".into());
    }
    Ok(resolved)
}

fn desktop_link() -> Result<PathBuf, String> {
    Ok(home()?.join("Desktop").join(APP_NAME))
}

pub fn status() -> Result<AssistantStatus, String> {
    let record = installed_record()?;
    let target = record
        .as_ref()
        .map(|value| Path::new(&value.install_path).join(APP_NAME));
    let version = target
        .as_ref()
        .filter(|path| path.exists())
        .map(|path| validate_bundle(path, None))
        .transpose()?;
    let installed = version.is_some();
    let cached = crate::self_update::cached_assistant_update()?;
    let result = crate::self_update::last_update_result()?;
    let uninstall_path = util::data_dir()?.join("installer/uninstall-result.json");
    let uninstall: Option<crate::self_update::UpdateResult> = if uninstall_path.exists() {
        Some(
            serde_json::from_slice(&fs::read(uninstall_path).map_err(|error| error.to_string())?)
                .map_err(|error| format!("卸载结果无效：{error}"))?,
        )
    } else {
        None
    };
    Ok(AssistantStatus {
        installed,
        mode: record
            .as_ref()
            .filter(|_| installed)
            .map(|value| value.mode.clone()),
        install_path: record
            .as_ref()
            .filter(|_| installed)
            .map(|value| value.install_path.clone())
            .unwrap_or_default(),
        version: version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        shortcut_ready: target.as_ref().is_some_and(|path| {
            desktop_link()
                .ok()
                .and_then(|link| fs::read_link(link).ok())
                .as_ref()
                == Some(path)
        }),
        default_paths: defaults()?,
        update_available: cached.as_ref().and_then(|value| value.update_available),
        latest_version: cached
            .as_ref()
            .and_then(|value| value.latest_version.clone()),
        update_check_error: cached.as_ref().and_then(|value| value.error.clone()),
        release_notes: cached
            .as_ref()
            .and_then(|value| value.release_notes.clone()),
        last_update_check: cached.map(|value| value.checked_at),
        update_checking: crate::self_update::assistant_update_checking(),
        update_result: result.as_ref().map(|value| value.message.clone()),
        update_result_ok: result.map(|value| value.ok),
        uninstall_result: uninstall.as_ref().map(|value| value.message.clone()),
        uninstall_result_ok: uninstall.map(|value| value.ok),
    })
}

fn plist_value(bundle: &Path, key: &str) -> Result<String, String> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Print :{key}")])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("应用包缺少有效的 {key}。"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

pub(crate) fn bundle_executable(bundle: &Path) -> PathBuf {
    bundle.join("Contents/MacOS").join(EXECUTABLE)
}

pub(crate) fn validate_bundle(
    bundle: &Path,
    expected_version: Option<&str>,
) -> Result<String, String> {
    if fs::symlink_metadata(bundle)
        .map_err(|error| error.to_string())?
        .file_type()
        .is_symlink()
        || !bundle.is_dir()
        || plist_value(bundle, "CFBundleIdentifier")? != BUNDLE_ID
        || plist_value(bundle, "CFBundleExecutable")? != EXECUTABLE
        || plist_value(bundle, "CFBundlePackageType")? != "APPL"
        || !bundle_executable(bundle).is_file()
    {
        return Err("应用包不属于 Claude 中文助手或结构不完整。".into());
    }
    let version = plist_value(bundle, "CFBundleShortVersionString")?;
    if version.is_empty()
        || !version
            .chars()
            .all(|value| value.is_ascii_digit() || value == '.')
        || expected_version.is_some_and(|expected| expected != version)
    {
        return Err(format!("助手应用包版本不符：{version}。"));
    }
    let root = fs::canonicalize(bundle).map_err(|error| error.to_string())?;
    check_bundle_links(&root, &root)?;
    Ok(version)
}

fn check_bundle_links(root: &Path, directory: &Path) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            let resolved = fs::canonicalize(entry.path()).map_err(|error| error.to_string())?;
            if !resolved.starts_with(root) {
                return Err("助手应用包包含指向外部的符号链接。".into());
            }
        } else if kind.is_dir() {
            check_bundle_links(root, &entry.path())?;
        } else if !kind.is_file() {
            return Err("助手应用包包含不支持的特殊文件。".into());
        }
    }
    Ok(())
}

pub(crate) fn needs_elevation(target: &Path) -> Result<bool, String> {
    if let Some(record) = installed_record()? {
        if target == Path::new(&record.install_path).join(APP_NAME) {
            return Ok(record.mode == "system" || target.starts_with("/Applications"));
        }
    }
    Ok(!target.starts_with(home()?) && !target.starts_with(env::temp_dir()))
}

// Staging and backup are siblings of the destination, so each rename is atomic.
// Keep the old bundle until the caller has checked the replacement and launch.
pub(crate) struct BundleSwap {
    target: PathBuf,
    backup: Option<PathBuf>,
    elevated: bool,
}

impl BundleSwap {
    pub(crate) fn rollback(&self) -> Result<(), String> {
        if fs::symlink_metadata(&self.target).is_ok_and(|value| value.file_type().is_symlink()) {
            return Err("回退目标被替换为符号链接，已停止。".into());
        }
        let script = if let Some(backup) = &self.backup {
            validate_bundle(backup, None)?;
            format!(
                "/bin/rm -rf -- {target}\n/bin/mv -- {backup} {target}",
                target = shell_quote(&self.target),
                backup = shell_quote(backup)
            )
        } else {
            format!("/bin/rm -rf -- {}", shell_quote(&self.target))
        };
        run_shell(&script, self.elevated).map(|_| ())
    }

    pub(crate) fn finish(&self) -> Result<(), String> {
        if let Some(backup) = &self.backup {
            validate_bundle(backup, None)?;
            run_shell(
                &format!("/bin/rm -rf -- {}", shell_quote(backup)),
                self.elevated,
            )?;
        }
        Ok(())
    }
}

pub(crate) fn replace_bundle(
    source: &Path,
    target: &Path,
    elevated: bool,
) -> Result<BundleSwap, String> {
    let version = validate_bundle(source, None)?;
    let parent = target.parent().ok_or("应用安装目录无效。")?;
    if validate_directory(parent)? != parent
        || target.file_name().and_then(|v| v.to_str()) != Some(APP_NAME)
    {
        return Err("目标不是规范化的助手应用路径。".into());
    }
    let exists = fs::symlink_metadata(target).is_ok();
    if exists {
        validate_bundle(target, None)?;
    }
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
    );
    let stage = parent.join(format!(".ClaudeWindowsCN-{suffix}.new.app"));
    let backup = parent.join(format!(".ClaudeWindowsCN-{suffix}.old.app"));
    let hash = util::sha256(&bundle_executable(source))?;
    let script = format!(
        r#"target={target}
stage={stage}
backup={backup}
moved=0
stage_owned=0
cleanup() {{
  code=$?
  trap - EXIT
  if [ "$code" -ne 0 ] && [ "$moved" = 1 ] && [ ! -e "$target" ]; then /bin/mv -- "$backup" "$target" || exit 71; fi
  if [ "$stage_owned" = 1 ] && [ -d "$stage" ] && [ ! -L "$stage" ]; then /bin/rm -rf -- "$stage"; fi
  exit "$code"
}}
trap cleanup EXIT
[ ! -e "$stage" ] && [ ! -L "$stage" ] && [ ! -e "$backup" ] && [ ! -L "$backup" ] || exit 65
[ ! -L "$target" ]
/bin/mkdir -p -- {parent}
/bin/mkdir -- "$stage"
stage_owned=1
/usr/bin/ditto {source} "$stage"
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$stage/Contents/Info.plist")" = {identity} ]
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$stage/Contents/Info.plist")" = {version} ]
[ "$(/usr/bin/shasum -a 256 "$stage/Contents/MacOS/claude-windows-cn" | /usr/bin/awk '{{print $1}}')" = {hash} ]
if [ {exists} = 1 ]; then
  [ -d "$target" ]
  [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$target/Contents/Info.plist")" = {identity} ]
  /bin/mv -- "$target" "$backup"
  moved=1
else
  [ ! -e "$target" ]
fi
/bin/mv -- "$stage" "$target"
"#,
        target = shell_quote(target),
        stage = shell_quote(&stage),
        backup = shell_quote(&backup),
        parent = shell_quote(parent),
        source = shell_quote(source),
        identity = shell_quote(BUNDLE_ID),
        version = shell_quote(version),
        hash = shell_quote(hash),
        exists = if exists { 1 } else { 0 }
    );
    run_shell(&script, elevated)?;
    Ok(BundleSwap {
        target: target.into(),
        backup: exists.then_some(backup),
        elevated,
    })
}

pub fn install(
    config: AssistantConfig,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    let destination = validate_config(&config)?;
    let source = current_bundle()?;
    let target = destination.join(APP_NAME);
    let in_place =
        fs::canonicalize(&target).ok().as_ref() == fs::canonicalize(&source).ok().as_ref();
    let record = installed_record()?;
    if let Some(record) = &record {
        if Path::new(&record.install_path) != destination
            && Path::new(&record.install_path).join(APP_NAME).exists()
        {
            return Err(format!(
                "助手已安装在 {}；请先卸载后再更换路径。",
                record.install_path
            ));
        }
    }
    if fs::symlink_metadata(&target).is_ok()
        && !in_place
        && !record
            .as_ref()
            .is_some_and(|value| Path::new(&value.install_path) == destination)
    {
        return Err("目标位置已有同名应用，但没有本助手的安装记录；请更换目录。".into());
    }
    let link = desktop_link()?;
    if config.create_assistant_shortcut
        && fs::symlink_metadata(&link).is_ok()
        && fs::read_link(&link).ok().as_ref() != Some(&target)
    {
        return Err("同名桌面入口不属于本助手目标。".into());
    }
    operation.step("安装 Claude 中文助手应用包");
    let swap = if in_place {
        None
    } else {
        Some(replace_bundle(
            &source,
            &target,
            config.assistant_install_mode == "system" || needs_elevation(&target)?,
        )?)
    };
    let mut created_link = false;
    let result: Result<(), String> = (|| {
        validate_bundle(&target, Some(env!("CARGO_PKG_VERSION")))?;
        if util::sha256(&bundle_executable(&target))? != util::sha256(&bundle_executable(&source))?
        {
            return Err("助手安装文件摘要不一致。".into());
        }
        if config.create_assistant_shortcut && fs::symlink_metadata(&link).is_err() {
            fs::create_dir_all(link.parent().ok_or("桌面路径无效。")?)
                .map_err(|error| error.to_string())?;
            run_shell(
                &format!(
                    "/bin/ln -s -- {} {}",
                    shell_quote(&target),
                    shell_quote(&link)
                ),
                false,
            )?;
            created_link = true;
        }
        write_json(
            &installed_record_path()?,
            &InstallManifest {
                product: "claude-windows-cn".into(),
                mode: config.assistant_install_mode.clone(),
                install_path: destination.display().to_string(),
            },
        )
    })();
    if let Err(mut error) = result {
        if created_link && fs::read_link(&link).ok().as_ref() == Some(&target) {
            if let Err(cleanup) = fs::remove_file(&link) {
                error.push_str(&format!("；清理本次桌面入口失败：{cleanup}"));
            }
        }
        if let Some(swap) = swap {
            swap.rollback()
                .map_err(|rollback| format!("{error}；回退失败：{rollback}"))?;
        }
        return Err(error);
    }
    if let Some(swap) = swap {
        if let Err(error) = swap.finish() {
            operation.log(format!("助手安装已完成，旧版备份已保留：{error}"));
        }
    }
    Ok(OperationOutcome::done(if in_place {
        format!("助手已在 {} 就绪。", target.display())
    } else {
        format!(
            "助手已安装到 {}；原始应用包已保留，今后请从安装位置或桌面入口启动。",
            target.display()
        )
    }))
}

pub fn uninstall(operation: &OperationState) -> Result<OperationOutcome, String> {
    let record = installed_record()?.ok_or("没有本助手的安装位置记录。")?;
    let directory = validate_directory(Path::new(&record.install_path))?;
    let target = directory.join(APP_NAME);
    validate_bundle(&target, None)?;
    operation.step("卸载 Claude 中文助手");
    let link = desktop_link()?;
    let data = util::data_dir()?;
    // Removing an open bundle is supported by macOS; the current process exits
    // after recording the outcome. Never remove the containing Applications folder.
    let result: Result<(), String> = (|| {
        // Every deletion runs in this one shell, after the administrator prompt
        // when one is needed, so cancelling it changes nothing. Claude itself is
        // never touched. The Claude shortcut goes first, then the app bundle,
        // then the whole data directory: rm -rf removes a link, never its target,
        // and find -H first empties a data directory that was moved behind one.
        let mut script = String::new();
        if let Some((claude_link, claude_target)) = crate::claude::owned_desktop_shortcut()? {
            script.push_str(&format!(
                "if [ -L {link} ] && [ \"$(/usr/bin/readlink {link})\" = {target} ]; then /bin/rm -f -- {link}; fi\n",
                link = shell_quote(&claude_link),
                target = shell_quote(&claude_target),
            ));
        }
        script.push_str(&format!("[ ! -L {target} ]\n[ \"$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' {plist})\" = {identity} ]\n/bin/rm -rf -- {target}\n",
            target=shell_quote(&target), plist=shell_quote(target.join("Contents/Info.plist")), identity=shell_quote(BUNDLE_ID)));
        script.push_str(&format!(
            "if [ -d {data} ]; then /usr/bin/find -H {data} -mindepth 1 -maxdepth 1 -exec /bin/rm -rf -- {{}} +; fi\n/bin/rm -rf -- {data}\n",
            data = shell_quote(&data),
        ));
        run_shell(
            &script,
            record.mode == "system" || needs_elevation(&target)?,
        )?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err("卸载操作结束后助手应用包仍存在。".into());
        }
        if fs::read_link(&link).ok().as_ref() == Some(&target) {
            fs::remove_file(link).map_err(|error| error.to_string())?;
        }
        Ok(())
    })();
    // Success leaves nothing behind; only a failure is recorded for the next launch.
    if let Err(error) = &result {
        write_json(
            &data.join("installer/uninstall-result.json"),
            &crate::self_update::UpdateResult {
                ok: false,
                message: error.clone(),
            },
        )?;
    }
    result?;
    // Once uninstalled the assistant leaves: a copy left open would write its
    // update checks back into the deleted data directory.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(0);
    });
    Ok(OperationOutcome::done(
        "助手已卸载，助手数据已清除，助手即将退出。",
    ))
}

pub fn choose_install_path() -> Result<Option<String>, String> {
    let output = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "try",
            "-e",
            "POSIX path of (choose folder with prompt \"选择助手安装目录\")",
            "-e",
            "on error number -128",
            "-e",
            "return \"\"",
            "-e",
            "end try",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_end_matches('/')
        .to_string();
    Ok(if path.is_empty() { None } else { Some(path) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_arguments_preserve_quotes_and_metacharacters() {
        let text = "space ' dollar $HOME; $(touch /tmp/should-not-exist) `id`";
        let output = run_shell(&format!("printf '%s' {}", shell_quote(text)), false).unwrap();
        assert_eq!(output, text);
    }

    #[test]
    fn refuses_protected_and_nested_bundle_paths() {
        for path in [
            "/",
            "/System/Applications",
            "/Library/Tools",
            "/usr/local",
            "/Applications/../Library",
            "/Applications/Other.app/Contents",
            "relative",
        ] {
            assert!(validate_directory(Path::new(path)).is_err(), "{path}");
        }
        assert!(validate_directory(&home().unwrap()).is_err());
        assert_eq!(
            validate_directory(Path::new("/Applications")).unwrap(),
            Path::new("/Applications")
        );
    }

    #[test]
    fn bundle_swap_rolls_back_a_damaged_replacement_and_preserves_neighbors() {
        let root = env::temp_dir().join(format!(
            "claude-swap-'$-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let source = root.join("source").join(APP_NAME);
        let target = root.join("target").join(APP_NAME);
        fn bundle(path: &Path, contents: &[u8]) {
            fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
            fs::write(bundle_executable(path), contents).unwrap();
            fs::write(
                path.join("Contents/Info.plist"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
<key>CFBundleExecutable</key><string>{EXECUTABLE}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0.0</string>
</dict></plist>"#
                ),
            )
            .unwrap();
        }
        bundle(&source, b"new executable");
        bundle(&target, b"old executable");
        let neighbor = target.parent().unwrap().join("unrelated.txt");
        fs::write(&neighbor, b"keep").unwrap();
        let swap = replace_bundle(&source, &target, false).unwrap();
        assert_eq!(
            fs::read(bundle_executable(&target)).unwrap(),
            b"new executable"
        );
        fs::remove_file(target.join("Contents/Info.plist")).unwrap();
        swap.rollback().unwrap();
        assert_eq!(
            fs::read(bundle_executable(&target)).unwrap(),
            b"old executable"
        );
        assert_eq!(fs::read(neighbor).unwrap(), b"keep");
        run_shell(
            &format!(
                "/bin/ln -s -- {} {}",
                shell_quote(&target),
                shell_quote(root.join("alias"))
            ),
            false,
        )
        .unwrap();
        assert!(validate_directory(&root.join("alias/Contents")).is_err());
        // A link escaping the app cannot enter installation or self-update.
        run_shell(
            &format!(
                "/bin/ln -s -- {} {}",
                shell_quote(&root),
                shell_quote(source.join("Contents/external"))
            ),
            false,
        )
        .unwrap();
        assert!(validate_bundle(&source, None).unwrap_err().contains("外部"));
        assert!(root.starts_with(fs::canonicalize(env::temp_dir()).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
}
