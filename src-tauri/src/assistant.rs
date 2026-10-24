use crate::{
    operation::{OperationOutcome, OperationState},
    util::{self, ps_path, ps_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

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

#[derive(Clone, Debug, Deserialize)]
struct UninstallResult {
    ok: bool,
    message: String,
}

fn last_uninstall_result() -> Result<Option<UninstallResult>, String> {
    let path = util::data_dir()?
        .join("installer")
        .join("uninstall-result.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map(Some)
        .map_err(|error| format!("读取卸载结果失败：{error}"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallManifest {
    product: String,
    mode: String,
    install_path: String,
    version: String,
}

fn defaults() -> Result<DefaultPaths, String> {
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let portable = exe.parent().ok_or("无法确定助手所在目录。")?.to_path_buf();
    let user = util::local_app_data()?
        .join("Programs")
        .join("ClaudeWindowsCN");
    let system = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("ClaudeWindowsCN");
    Ok(DefaultPaths {
        portable: portable.display().to_string(),
        user: user.display().to_string(),
        system: system.display().to_string(),
    })
}

fn config_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("config.json"))
}

fn installed_record_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("installed.json"))
}

fn installed_record() -> Result<Option<InstallManifest>, String> {
    read_installed_record(&installed_record_path()?)
}

fn read_installed_record(path: &Path) -> Result<Option<InstallManifest>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let manifest: InstallManifest =
        serde_json::from_str(&raw).map_err(|error| format!("助手安装位置记录无效：{error}"))?;
    if manifest.product != "claude-windows-cn" {
        return Err("助手安装位置记录不属于本工具。".into());
    }
    Ok(Some(manifest))
}

fn save_installed_record(manifest: &InstallManifest) -> Result<(), String> {
    util::write_json(&installed_record_path()?, manifest)
}

/// A portable copy takes over the data an earlier version kept in the per-user
/// directory, unless that data is recorded for another installation. Settings,
/// records and the engine's run logs are copied; caches are not and download
/// again. The Claude shortcut follows its icon before the old directory goes.
/// Any failure leaves the old directory in place.
pub fn adopt_legacy_data() -> Result<(), String> {
    let data = util::data_dir()?;
    let legacy = util::legacy_data_dir()?;
    // Not portable, or nothing to adopt.
    if data == legacy || !legacy.is_dir() {
        return Ok(());
    }
    // Links, case and short names let different paths name one place, so
    // overlap and ownership are judged on the paths they resolve to.
    let real = |path: &Path| {
        fs::canonicalize(path).map_err(|error| format!("无法解析 {}：{error}", path.display()))
    };
    let root = real(data.parent().ok_or("助手数据目录无效。")?)?;
    let (data_real, legacy_real) = (real(&data)?, real(&legacy)?);
    let overlaps = |path: &Path| path.starts_with(&legacy_real) || legacy_real.starts_with(path);
    // The old directory is this data, holds this copy, or lies inside it.
    if overlaps(&data_real) || overlaps(&root) {
        return Ok(());
    }
    match read_installed_record(&legacy.join("installed.json")) {
        // Written by this copy before it was installed in place.
        Ok(None) => {}
        Ok(Some(record)) if real(Path::new(&record.install_path)).ok() == Some(root.clone()) => {}
        // Another installation's, or unreadable: not this copy's to take.
        _ => return Ok(()),
    }
    let engine = Path::new(crate::patch::ENGINE_DIR);
    let mut entries = [
        "config.json",
        "installed.json",
        "patch-backup.json",
        "patch-pending.json",
        "icons/claude-desktop-app.ico",
    ]
    .map(PathBuf::from)
    .to_vec();
    // Run directories hold the logs an unfinished localization is checked against.
    match fs::read_dir(legacy.join(engine)) {
        Ok(runs) => {
            for run in runs {
                let name = run.map_err(|error| error.to_string())?.file_name();
                if name.to_string_lossy().starts_with("run-") {
                    entries.push(engine.join(name));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "读取 {} 失败：{error}",
                legacy.join(engine).display()
            ))
        }
    }
    for entry in entries {
        adopt_entry(&legacy.join(&entry), &data.join(&entry))?;
    }
    util::powershell(&crate::claude::move_desktop_shortcut_icon_script(
        &user_desktop()?,
        &legacy.join("icons"),
        &data.join("icons"),
    ))?;
    // remove_dir_all removes a link itself and never follows one.
    fs::remove_dir_all(&legacy)
        .map_err(|error| format!("删除旧数据目录 {} 失败：{error}", legacy.display()))
}

// Copies `from` to `to` through a staging sibling, so an interrupted copy is
// never taken for a finished one. A `to` already there wins, and links are
// neither copied nor followed.
fn adopt_entry(from: &Path, to: &Path) -> Result<(), String> {
    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let (kind, target) = (entry.file_type()?, to.join(entry.file_name()));
            if kind.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else if kind.is_file() {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let failed = |error: std::io::Error| format!("迁移 {} 失败：{error}", from.display());
    let kind = match fs::symlink_metadata(from) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed(error)),
    };
    if kind.is_symlink() || to.exists() {
        return Ok(());
    }
    let mut stage = to.as_os_str().to_owned();
    stage.push(".adopting");
    let stage = PathBuf::from(stage);
    // Left by an interrupted attempt.
    if fs::symlink_metadata(&stage).is_ok() {
        util::remove_entry(&stage)?;
    }
    fs::create_dir_all(to.parent().ok_or("迁移目标无效。")?).map_err(failed)?;
    let copied = if kind.is_dir() {
        copy_tree(from, &stage)
    } else {
        fs::copy(from, &stage).map(drop)
    };
    copied.and_then(|()| fs::rename(&stage, to)).map_err(failed)
}

/// A portable folder can be moved or copied as a whole. Its records travel
/// with it but still name the old place, so they are pointed at where this
/// copy runs now; the settings follow when they named the old place too.
pub fn follow_moved_portable_folder() -> Result<(), String> {
    let data = util::data_dir()?;
    if data == util::legacy_data_dir()? {
        return Ok(());
    }
    let root = data.parent().ok_or("助手数据目录无效。")?;
    let Some(mut record) = installed_record()? else {
        return Ok(());
    };
    let old = PathBuf::from(&record.install_path);
    if record.mode != "portable" || old == root {
        return Ok(());
    }
    let manifest_path = root.join("install.json");
    let raw =
        fs::read_to_string(&manifest_path).map_err(|error| format!("读取安装记录失败：{error}"))?;
    let mut manifest: InstallManifest = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("安装记录格式无效：{error}"))?;
    manifest.install_path = root.display().to_string();
    util::write_json(&manifest_path, &manifest)?;
    let mut config = load_config()?;
    if config.assistant_install_mode == "portable" && Path::new(&config.assistant_path) == old {
        config.assistant_path = manifest.install_path.clone();
        util::write_json(&config_path()?, &config)?;
    }
    // Last: until installed.json names this place, the next start redoes
    // every step, so a failure partway is finished later.
    record.install_path = manifest.install_path;
    save_installed_record(&record)
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
    let raw = fs::read_to_string(&path).map_err(|error| format!("读取助手设置失败：{error}"))?;
    let config =
        serde_json::from_str(&raw).map_err(|error| format!("助手设置格式无效：{error}"))?;
    validate_config(&config)?;
    Ok(config)
}

pub fn save_config(config: AssistantConfig) -> Result<AssistantConfig, String> {
    validate_config(&config)?;
    let was_enabled = load_config()
        .map(|previous| previous.daily_update_check)
        .unwrap_or(false);
    util::write_json(&config_path()?, &config)
        .map_err(|error| format!("保存助手设置失败：{error}"))?;
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
    let path = PathBuf::from(config.assistant_path.trim());
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("助手安装路径必须是完整路径，且不能包含 ..。".into());
    }
    let cleaned = path
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_string();
    let cleaned = PathBuf::from(cleaned);
    if cleaned.parent().is_none()
        || cleaned
            .parent()
            .is_some_and(|parent| parent.parent().is_none())
    {
        return Err("助手不能安装在磁盘根目录或系统根目录。".into());
    }
    for protected in [
        env::var_os("USERPROFILE"),
        env::var_os("WINDIR"),
        env::var_os("ProgramFiles"),
        env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    {
        if cleaned == protected {
            return Err("助手安装路径不能是用户、系统或应用数据根目录。".into());
        }
    }
    Ok(cleaned)
}

pub fn status() -> Result<AssistantStatus, String> {
    let config = load_config()?;
    let path = installed_record()?
        .map(|value| PathBuf::from(value.install_path))
        .unwrap_or(validate_config(&config)?);
    let mut manifest = read_manifest(&path)?;
    let installed = manifest.is_some() && path.join(util::EXE_NAME).is_file();
    if installed && running_installed_exe(&path) {
        sync_manifest_version(&path, manifest.as_mut());
    }
    let mode = manifest.as_ref().map(|value| value.mode.clone());
    let shortcut = user_desktop()?.join("Claude Windows 中文助手.lnk");
    let cached = crate::self_update::cached_assistant_update()?;
    let update_result = crate::self_update::last_update_result()?;
    let uninstall_result = last_uninstall_result()?;
    Ok(AssistantStatus {
        installed,
        mode,
        install_path: if installed {
            path.display().to_string()
        } else {
            String::new()
        },
        version: if installed {
            manifest.map_or_else(String::new, |value| value.version)
        } else {
            env!("CARGO_PKG_VERSION").into()
        },
        shortcut_ready: shortcut.is_file(),
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
        update_result: update_result.as_ref().map(|value| value.message.clone()),
        update_result_ok: update_result.map(|value| value.ok),
        uninstall_result: uninstall_result.as_ref().map(|value| value.message.clone()),
        uninstall_result_ok: uninstall_result.map(|value| value.ok),
    })
}

fn running_installed_exe(path: &Path) -> bool {
    let canonical = |value: &Path| fs::canonicalize(value).ok();
    env::current_exe()
        .ok()
        .and_then(|exe| canonical(&exe))
        .is_some_and(|exe| Some(exe) == canonical(&path.join(util::EXE_NAME)))
}

// Older self-updates replaced only the exe and left install.json at the
// previous version. The running installed exe is the source of truth.
fn sync_manifest_version(path: &Path, manifest: Option<&mut InstallManifest>) {
    let Some(manifest) = manifest else { return };
    if manifest.version == env!("CARGO_PKG_VERSION") {
        return;
    }
    manifest.version = env!("CARGO_PKG_VERSION").into();
    // Best effort: a system install may not be writable without elevation.
    if let Ok(bytes) = serde_json::to_vec_pretty(&*manifest) {
        let _ = fs::write(path.join("install.json"), bytes);
    }
}

fn read_manifest(path: &Path) -> Result<Option<InstallManifest>, String> {
    let file = path.join("install.json");
    if !file.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&file).map_err(|error| format!("读取安装记录失败：{error}"))?;
    let manifest: InstallManifest =
        serde_json::from_str(&raw).map_err(|error| format!("安装记录格式无效：{error}"))?;
    if manifest.product != "claude-windows-cn" || Path::new(&manifest.install_path) != path {
        return Err("安装记录不属于本助手或路径不一致。".into());
    }
    Ok(Some(manifest))
}

fn user_desktop() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = env::var_os("CLAUDE_WINDOWS_CN_TEST_DESKTOP") {
        return Ok(PathBuf::from(path));
    }
    let raw = util::powershell("[Environment]::GetFolderPath('Desktop')")?;
    if raw.is_empty() {
        Err("无法确定当前用户桌面目录。".into())
    } else {
        Ok(PathBuf::from(raw))
    }
}

fn run_system_script(script: &Path, work: &Path) -> Result<String, String> {
    // Stands in for a cancelled UAC prompt, which a test cannot click.
    #[cfg(test)]
    if env::var_os("CLAUDE_WINDOWS_CN_TEST_CANCEL_ELEVATION").is_some() {
        return Err("管理员操作未启动（可能取消了 UAC）：测试模拟取消。".into());
    }
    // The test binary has no application helper entry point. The opt-in test
    // runs the same generated script from an already elevated shell.
    #[cfg(test)]
    if env::var_os("CLAUDE_WINDOWS_CN_INSTALL_TEST_CHILD").is_some() {
        util::powershell("if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'System acceptance test requires elevation.' }")?;
        return util::run_script(script);
    }
    util::run_elevated_script(script, work)
}

pub fn install(
    config: AssistantConfig,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    let destination = validate_config(&config)?;
    if let Some(record) = installed_record()? {
        let previous = PathBuf::from(record.install_path);
        if previous != destination && previous.join(util::EXE_NAME).exists() {
            return Err(format!(
                "助手已安装在 {}；请先卸载后再更换路径。",
                previous.display()
            ));
        }
    }
    let source = env::current_exe().map_err(|error| error.to_string())?;
    let target = destination.join(util::EXE_NAME);
    let in_place = target.is_file()
        && fs::canonicalize(&target).map_err(|error| error.to_string())?
            == fs::canonicalize(&source).map_err(|error| error.to_string())?;
    let existing_manifest = read_manifest(&destination)?;
    if target.exists() && !in_place && existing_manifest.is_none() {
        return Err("目标目录已有同名程序，但没有本助手的安装记录；请换一个目录。".into());
    }
    let manifest = InstallManifest {
        product: "claude-windows-cn".into(),
        mode: config.assistant_install_mode.clone(),
        install_path: destination.display().to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let work = util::data_dir()?.join("installer");
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let manifest_stage = work.join("install.json");
    fs::write(
        &manifest_stage,
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let desktop = user_desktop()?;
    let script_path = work.join("install-assistant.ps1");
    let scope = config.assistant_install_mode.as_str();
    let registry = if scope == "system" {
        "HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ClaudeWindowsCN"
    } else {
        "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ClaudeWindowsCN"
    };
    let script = format!(
        r#"$ErrorActionPreference='Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$dest={dest}
$source={source}
$target={target}
$manifest={manifest}
$desktop={desktop}
$shell=New-Object -ComObject WScript.Shell
$link=Join-Path $desktop 'Claude Windows 中文助手.lnk'
if ({shortcut} -and (Test-Path -LiteralPath $link)) {{
    if ($shell.CreateShortcut($link).TargetPath -ne $target) {{ throw '同名桌面快捷方式不属于本助手目标。' }}
}}
if ({registered}) {{
    $programs=if ({system}) {{ [Environment]::GetFolderPath('CommonPrograms') }} else {{ [Environment]::GetFolderPath('Programs') }}
    if (-not $programs) {{ throw '无法确定开始菜单目录。' }}
    $menu=Join-Path $programs 'Claude Windows 中文助手.lnk'
    if (Test-Path -LiteralPath $menu) {{
        if ($shell.CreateShortcut($menu).TargetPath -ne $target) {{ throw '同名开始菜单快捷方式不属于本助手目标。' }}
    }}
    $key={registry}
    if (Test-Path -LiteralPath $key) {{
        $entry=Get-ItemProperty -Path $key
        if ($entry.InstallLocation -ne $dest -or $entry.DisplayIcon -ne $target) {{ throw '同名卸载项不属于本助手目标。' }}
    }}
}}
New-Item -ItemType Directory -Path $dest -Force | Out-Null
if ({copy}) {{ Copy-Item -LiteralPath $source -Destination $target -Force }}
Copy-Item -LiteralPath $manifest -Destination (Join-Path $dest 'install.json') -Force
if ({shortcut}) {{
    $s=$shell.CreateShortcut($link); $s.TargetPath=$target; $s.WorkingDirectory=$dest; $s.Description='Claude Windows 中文助手'; $s.Save()
}}
if ({registered}) {{
    $s=$shell.CreateShortcut($menu); $s.TargetPath=$target; $s.WorkingDirectory=$dest; $s.Description='Claude Windows 中文助手'; $s.Save()
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name DisplayName -Value 'Claude Windows 中文助手'
    Set-ItemProperty -Path $key -Name DisplayVersion -Value {version}
    Set-ItemProperty -Path $key -Name InstallLocation -Value $dest
    Set-ItemProperty -Path $key -Name DisplayIcon -Value $target
}}
if (-not (Test-Path -LiteralPath $target)) {{ throw '助手程序复制失败。' }}
"#,
        dest = ps_path(&destination),
        source = ps_path(&source),
        target = ps_path(&target),
        copy = if in_place { "$false" } else { "$true" },
        manifest = ps_path(&manifest_stage),
        desktop = ps_path(&desktop),
        shortcut = if config.create_assistant_shortcut {
            "$true"
        } else {
            "$false"
        },
        registered = if scope == "portable" {
            "$false"
        } else {
            "$true"
        },
        system = if scope == "system" { "$true" } else { "$false" },
        registry = ps_quote(registry),
        version = ps_quote(env!("CARGO_PKG_VERSION")),
    );
    let script = if scope == "portable" {
        script
    } else {
        format!(
            "{script}\nSet-ItemProperty -Path {} -Name UninstallString -Value {}\n",
            ps_quote(registry),
            ps_quote(format!("\"{}\" --uninstall-assistant", target.display())),
        )
    };
    util::write_script(&script_path, &script)?;
    operation.step("安装 Claude 中文助手");
    let log = if scope == "system" {
        run_system_script(&script_path, &work)?
    } else {
        util::run_script(&script_path)?
    };
    operation.log(log);
    if !target.is_file() || read_manifest(&destination)?.is_none() {
        return Err("助手安装脚本已退出，但目标程序或安装记录缺失。".into());
    }
    if util::sha256(&target)? != util::sha256(&source)? {
        return Err("助手安装文件摘要与当前程序不一致。".into());
    }
    save_installed_record(&manifest)?;
    // Installed in place as portable: data now lives beside the program.
    if let Err(error) = adopt_legacy_data() {
        operation.log(error);
    }
    let message = if in_place {
        format!("助手已在 {} 就绪，未复制程序。", destination.display())
    } else {
        format!(
            "助手已安装到 {}。原始文件 {} 已保留；今后请使用安装目录中的程序或快捷方式，退出后可删除原始文件。",
            destination.display(), source.display()
        )
    };
    operation.log(&message);
    Ok(OperationOutcome::done(message))
}

pub fn uninstall(operation: &OperationState) -> Result<OperationOutcome, String> {
    let record = installed_record()?.ok_or("没有本助手的安装位置记录。")?;
    let path = PathBuf::from(record.install_path);
    let data = util::data_dir()?;
    // Data kept beside the program belongs to that copy alone, so a portable
    // copy only ever uninstalls itself, even while its records name elsewhere.
    if data != util::legacy_data_dir()? && Some(path.as_path()) != data.parent() {
        return Err(format!(
            "安装记录指向 {}，不是当前便携副本所在位置；请重新启动助手后再试。",
            path.display()
        ));
    }
    let manifest =
        read_manifest(&path)?.ok_or_else(|| "当前路径没有本助手的安装记录。".to_string())?;
    if manifest.mode != record.mode {
        return Err("助手安装位置记录与目标安装方式不一致。".into());
    }
    if !path.join(util::EXE_NAME).is_file() {
        return Err("助手安装文件不存在，已停止卸载。".into());
    }
    // The scripts live outside the data directory, which they remove last.
    let work = env::temp_dir().join(format!(
        "claude-windows-cn-uninstall-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
    ));
    let script_path = work.join("uninstall-assistant.ps1");
    let desktop_dir = user_desktop()?;
    let desktop = desktop_dir.join("Claude Windows 中文助手.lnk");
    let registry = if manifest.mode == "system" {
        "HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ClaudeWindowsCN"
    } else {
        "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ClaudeWindowsCN"
    };
    let current_exe = env::current_exe().map_err(|error| error.to_string())?;
    let running_target = current_exe == path.join(util::EXE_NAME);
    let script = format!(
        r#"$ErrorActionPreference='Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$dest={dest}
$exe=Join-Path $dest {exe}
$manifest=Join-Path $dest 'install.json'
$link={link}
$shell=New-Object -ComObject WScript.Shell
$removeLink=(Test-Path -LiteralPath $link) -and ($shell.CreateShortcut($link).TargetPath -eq $exe)
$removeMenu=$false
$removeKey=$false
if ({registered}) {{
    $programs=if ({system}) {{ [Environment]::GetFolderPath('CommonPrograms') }} else {{ [Environment]::GetFolderPath('Programs') }}
    if (-not $programs) {{ throw '无法确定开始菜单目录。' }}
    $menu=Join-Path $programs 'Claude Windows 中文助手.lnk'
    if (Test-Path -LiteralPath $menu) {{
        if ($shell.CreateShortcut($menu).TargetPath -ne $exe) {{ throw '同名开始菜单快捷方式不属于本助手目标。' }}
        $removeMenu=$true
    }}
    $key={registry}
    if (Test-Path -LiteralPath $key) {{
        $entry=Get-ItemProperty -Path $key
        if ($entry.InstallLocation -ne $dest -or $entry.DisplayIcon -ne $exe) {{ throw '同名卸载项不属于本助手目标。' }}
        $removeKey=$true
    }}
}}
if ({wait}) {{ Wait-Process -Id {pid} -Timeout 60 -ErrorAction SilentlyContinue }}
{remove_entry}
# Every deletion happens here, after any administrator prompt. Claude itself is
# never touched; a localization left in place stays until Claude updates.
# 1. The Claude shortcut this assistant made; its icon lives in the data.
{claude_shortcut}
# 2. The program and its entry points.
if ($removeLink) {{ Remove-Item -LiteralPath $link -Force -ErrorAction Stop }}
if ($removeMenu) {{ Remove-Item -LiteralPath $menu -Force -ErrorAction Stop }}
if ($removeKey) {{ Remove-Item -LiteralPath $key -Force -ErrorAction Stop }}
Remove-Item -LiteralPath $exe -Force -ErrorAction Stop
Remove-Item -LiteralPath $manifest -Force -ErrorAction Stop
# 3. The whole data directory. A moved one is emptied and only its link goes.
$data={data}
if (Test-Path -LiteralPath $data) {{
    try {{
        foreach ($entry in [IO.Directory]::GetFileSystemEntries($data)) {{ Remove-AssistantEntry $entry }}
        Remove-AssistantEntry $data
    }} catch {{ throw ('助手程序已卸载，但助手数据未能完全删除，可手动删除：' + $data + '。' + $_.Exception.Message) }}
}}
# 4. The program directory once empty; a portable copy kept its data inside.
if (@(Get-ChildItem -LiteralPath $dest -Force).Count -eq 0) {{ Remove-Item -LiteralPath $dest -Force -ErrorAction Stop }}
"#,
        dest = ps_path(&path),
        exe = ps_quote(util::EXE_NAME),
        link = ps_path(&desktop),
        remove_entry = REMOVE_ENTRY_FUNCTION,
        claude_shortcut =
            crate::claude::remove_desktop_shortcut_script(&desktop_dir, &data.join("icons")),
        data = ps_path(&data),
        registered = if manifest.mode == "portable" {
            "$false"
        } else {
            "$true"
        },
        system = if manifest.mode == "system" {
            "$true"
        } else {
            "$false"
        },
        registry = ps_quote(registry),
        wait = if running_target { "$true" } else { "$false" },
        pid = std::process::id(),
    );
    operation.step("卸载 Claude 中文助手");
    if running_target {
        let result_path = data.join("installer").join("uninstall-result.json");
        let wrapper_path = work.join("run-uninstall.ps1");
        let started = fs::create_dir_all(&work)
            .map_err(|error| error.to_string())
            .and_then(|()| util::write_script(&script_path, &script))
            .and_then(|()| {
                util::write_script(
                    &wrapper_path,
                    &uninstall_wrapper(&script_path, &result_path, &work),
                )
            })
            .and_then(|()| start_after_exit(&wrapper_path, manifest.mode == "system"));
        if let Err(error) = started {
            // No wrapper runs to remove the scripts, so they go here. The
            // original error, such as a cancelled UAC prompt, is what matters.
            if let Err(cleanup) = util::remove_work_dir(&work) {
                operation.log(cleanup);
            }
            return Err(error);
        }
        exit_soon();
        Ok(OperationOutcome::done(
            "卸载脚本已启动，助手退出后将删除程序文件和助手数据。",
        ))
    } else {
        let written = fs::create_dir_all(&work)
            .map_err(|error| error.to_string())
            .and_then(|()| util::write_script(&script_path, &script));
        let log = written.and_then(|()| {
            if manifest.mode == "system" {
                run_system_script(&script_path, &work)
            } else {
                util::run_script(&script_path)
            }
        });
        if let Err(error) = util::remove_work_dir(&work) {
            operation.log(error);
        }
        operation.log(log?);
        if path.join(util::EXE_NAME).exists() {
            return Err("卸载脚本已退出，但助手文件仍存在。".into());
        }
        exit_soon();
        Ok(OperationOutcome::done(
            "助手已卸载，助手数据已清除，助手即将退出。",
        ))
    }
}

/// Starts the wrapper that finishes the uninstall once this process exits.
fn start_after_exit(wrapper: &Path, elevated: bool) -> Result<(), String> {
    // Stands in for a cancelled UAC prompt, which a test cannot click.
    #[cfg(test)]
    if env::var_os("CLAUDE_WINDOWS_CN_TEST_CANCEL_ELEVATION").is_some() {
        return Err("管理员操作未启动（可能取消了 UAC）：测试模拟取消。".into());
    }
    let launch = format!(
        "Start-Process -FilePath 'powershell.exe' -ArgumentList {} {} -WindowStyle Hidden | Out-Null",
        ps_quote(format!(
            "-NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            wrapper.display()
        )),
        if elevated { "-Verb RunAs" } else { "" }
    );
    util::powershell(&launch).map(|_| ())
}

/// Once uninstalled the assistant leaves: a copy left open would write its
/// update checks back into the deleted data directory.
fn exit_soon() {
    // Tests run uninstall in their own process and must outlive it.
    #[cfg(not(test))]
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(0);
    });
}

// Deletes a file or directory tree for uninstall. A junction or symlink goes as
// a link, never followed, and read-only attributes are cleared first. Only .NET
// calls receive the path, so no command interpreter ever parses it again.
const REMOVE_ENTRY_FUNCTION: &str = r#"function Remove-AssistantEntry([string]$Path) {
    $attributes=[IO.File]::GetAttributes($Path)
    $directory=($attributes -band [IO.FileAttributes]::Directory) -ne 0
    if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        if ($directory) { [IO.Directory]::Delete($Path, $false) } else { [IO.File]::Delete($Path) }
    } elseif ($directory) {
        foreach ($child in [IO.Directory]::GetFileSystemEntries($Path)) { Remove-AssistantEntry $child }
        [IO.File]::SetAttributes($Path, [IO.FileAttributes]::Directory)
        [IO.Directory]::Delete($Path, $false)
    } else {
        [IO.File]::SetAttributes($Path, [IO.FileAttributes]::Normal)
        [IO.File]::Delete($Path)
    }
}"#;

// Runs after the assistant exits. Success leaves nothing behind: the script has
// removed the data directory and this temporary folder removes itself. Only a
// failure is recorded, where the next launch of any copy can show it.
fn uninstall_wrapper(script: &Path, result: &Path, work: &Path) -> String {
    format!(
        r#"$ErrorActionPreference='Stop'
$result={result}
try {{
    $output=& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {script} 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {{ throw $output }}
}} catch {{
    New-Item -ItemType Directory -Path (Split-Path -Parent $result) -Force | Out-Null
    @{{ ok=$false; message=$_.Exception.Message }} | ConvertTo-Json -Compress | Set-Content -LiteralPath $result -Encoding UTF8
}}
Remove-Item -LiteralPath {work} -Recurse -Force -ErrorAction SilentlyContinue
"#,
        result = ps_path(result),
        script = ps_path(script),
        work = ps_path(work),
    )
}

pub fn choose_install_path() -> Result<Option<String>, String> {
    let script = "Add-Type -AssemblyName System.Windows.Forms; $dialog=New-Object System.Windows.Forms.FolderBrowserDialog; $dialog.Description='选择助手安装目录'; if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $dialog.SelectedPath }";
    let selected = util::powershell(script)?;
    Ok(if selected.is_empty() {
        None
    } else {
        Some(selected)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::{process::Command, time::SystemTime};

    #[cfg(windows)]
    fn scratch(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(windows)]
    #[test]
    fn uninstall_wrapper_leaves_nothing_on_success_and_records_failure() {
        let root = scratch("claude-uninstall-wrapper");
        let result = root.join("data/installer/uninstall-result.json");
        for (inner, succeeds) in [("Write-Output 'done'", true), ("throw 'boom'", false)] {
            let work = root.join(format!("work-{succeeds}"));
            fs::create_dir_all(&work).unwrap();
            let script = work.join("uninstall-assistant.ps1");
            util::write_script(&script, inner).unwrap();
            let wrapper = work.join("run-uninstall.ps1");
            util::write_script(&wrapper, &uninstall_wrapper(&script, &result, &work)).unwrap();
            util::run_script(&wrapper).unwrap();
            // The temporary folder goes, including the wrapper that is running.
            assert!(!work.exists());
            if succeeds {
                assert!(!result.exists());
            } else {
                let recorded: UninstallResult = serde_json::from_str(
                    fs::read_to_string(&result)
                        .unwrap()
                        .trim_start_matches('\u{feff}'),
                )
                .unwrap();
                assert!(!recorded.ok && recorded.message.contains("boom"));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn clean_uninstall_removes_program_entries_and_all_assistant_data() {
        const CHILD: &str = "CLAUDE_WINDOWS_CN_CLEAN_UNINSTALL_TEST_CHILD";
        // Characters a second command interpreter would split or expand.
        const LOCAL: &str = "Local A&B %PATH% ^x";
        if env::var_os(CHILD).is_some() {
            let scratch = util::local_app_data().unwrap();
            // What a split path would have pointed at, and a neighbour of the data.
            let prefix = scratch.parent().unwrap().join("Local A");
            fs::create_dir_all(&prefix).unwrap();
            fs::write(prefix.join("keep.txt"), "x").unwrap();
            let neighbour = scratch.join("other-app");
            fs::create_dir_all(&neighbour).unwrap();
            fs::write(neighbour.join("keep.txt"), "x").unwrap();
            let data = util::data_dir().unwrap();
            let desktop = user_desktop().unwrap();
            fs::create_dir_all(&desktop).unwrap();
            // Shortcut targets expand %VAR%, so the program lives beside the odd path;
            // only the data directory needs those characters.
            let destination = scratch
                .parent()
                .unwrap()
                .join("Portable")
                .join(util::PRODUCT_DIR);
            // WScript.Shell passes shortcut names through the ANSI code page, so the
            // Chinese-named link only exists where that code page has those characters
            // (not on English CI runners).
            let shortcut = util::powershell(
                "$n='中文助手'; $e=[Text.Encoding]::Default; $e.GetString($e.GetBytes($n)) -eq $n",
            )
            .unwrap()
                == "True";
            let config = AssistantConfig {
                assistant_install_mode: "portable".into(),
                assistant_path: destination.display().to_string(),
                create_assistant_shortcut: shortcut,
                daily_update_check: false,
                auto_reapply_patch: true,
            };
            save_config(config.clone()).unwrap();
            let operation = OperationState::new();
            install(config, &operation).unwrap();
            let target = destination.join(util::EXE_NAME);
            let assistant_link = desktop.join("Claude Windows 中文助手.lnk");
            assert!(target.is_file() && assistant_link.is_file() == shortcut);

            // Leftovers the uninstall must remove, beside a junction it must not follow.
            fs::create_dir_all(data.join("official-msix")).unwrap();
            fs::write(data.join("official-msix/Claude-1.0.msix"), "x").unwrap();
            let read_only = data.join("logs").join("read-only.log");
            fs::create_dir_all(read_only.parent().unwrap()).unwrap();
            fs::write(&read_only, "x").unwrap();
            let mut permissions = fs::metadata(&read_only).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&read_only, permissions).unwrap();
            let outside = scratch.join("outside");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("keep.txt"), "x").unwrap();
            util::powershell(&format!(
                "New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
                ps_path(&data.join("linked")),
                ps_path(&outside)
            ))
            .unwrap();
            let claude_link = desktop.join("Claude Desktop.lnk");
            util::powershell(&format!(
                "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath=(Join-Path $env:WINDIR 'explorer.exe'); $s.Arguments={}; $s.IconLocation={}; $s.Save()",
                ps_path(&claude_link),
                ps_quote(format!(r"shell:AppsFolder\{}", crate::claude::APP_ID)),
                ps_quote(format!("{},0", data.join("icons/claude-desktop-app.ico").display())),
            ))
            .unwrap();
            // Records a gated uninstall used to stop for go with everything else.
            fs::write(data.join("patch-backup.json"), "{}").unwrap();
            fs::write(data.join("patch-pending.json"), "{}").unwrap();

            uninstall(&operation).unwrap();
            assert!(!target.exists());
            assert!(!assistant_link.exists());
            assert!(!claude_link.exists());
            assert!(!data.exists());
            assert!(outside.join("keep.txt").is_file());
            assert!(prefix.join("keep.txt").is_file());
            assert!(neighbour.join("keep.txt").is_file());
            return;
        }
        let root = scratch("claude-clean-uninstall");
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "assistant::tests::clean_uninstall_removes_program_entries_and_all_assistant_data",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("LOCALAPPDATA", root.join(LOCAL))
            .env("CLAUDE_WINDOWS_CN_TEST_DESKTOP", root.join("Desktop"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_elevation_leaves_everything_in_place() {
        const CHILD: &str = "CLAUDE_WINDOWS_CN_CANCELLED_ELEVATION_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let scratch = util::local_app_data().unwrap();
            let data = util::data_dir().unwrap();
            let desktop = user_desktop().unwrap();
            fs::create_dir_all(&desktop).unwrap();
            // A system install on record; the test cannot really create one.
            let destination = scratch.join("Programs").join(util::PRODUCT_DIR);
            fs::create_dir_all(&destination).unwrap();
            let target = destination.join(util::EXE_NAME);
            fs::write(&target, "exe").unwrap();
            let manifest = InstallManifest {
                product: "claude-windows-cn".into(),
                mode: "system".into(),
                install_path: destination.display().to_string(),
                version: env!("CARGO_PKG_VERSION").into(),
            };
            fs::write(
                destination.join("install.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            save_installed_record(&manifest).unwrap();
            let config = data.join("config.json");
            fs::write(&config, "{}").unwrap();
            let claude_link = desktop.join("Claude Desktop.lnk");
            util::powershell(&format!(
                "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath=(Join-Path $env:WINDIR 'explorer.exe'); $s.Arguments={}; $s.IconLocation={}; $s.Save()",
                ps_path(&claude_link),
                ps_quote(format!(r"shell:AppsFolder\{}", crate::claude::APP_ID)),
                ps_quote(format!("{},0", data.join("icons/claude-desktop-app.ico").display())),
            ))
            .unwrap();

            let error = uninstall(&OperationState::new()).err().unwrap();
            assert!(error.contains("UAC"), "{error}");
            // Every deletion waits for the approved script, so nothing changed.
            assert!(target.is_file() && destination.join("install.json").is_file());
            assert!(installed_record().unwrap().is_some());
            assert!(config.is_file() && claude_link.is_file());
            return;
        }
        let root = scratch("claude-cancelled-elevation");
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "assistant::tests::cancelled_elevation_leaves_everything_in_place",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CLAUDE_WINDOWS_CN_TEST_CANCEL_ELEVATION", "1")
            .env("LOCALAPPDATA", &root)
            .env("CLAUDE_WINDOWS_CN_TEST_DESKTOP", root.join("Desktop"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_start_from_the_running_copy_leaves_nothing_behind() {
        const CHILD: &str = "CLAUDE_WINDOWS_CN_CANCELLED_START_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            // This process is the installed copy, so uninstall must hand over to
            // a script that runs after it exits; starting that script fails.
            let source = env::current_exe().unwrap();
            let destination = source.parent().unwrap();
            let config = AssistantConfig {
                assistant_install_mode: "portable".into(),
                assistant_path: destination.display().to_string(),
                create_assistant_shortcut: false,
                daily_update_check: false,
                auto_reapply_patch: true,
            };
            save_config(config.clone()).unwrap();
            let operation = OperationState::new();
            install(config, &operation).unwrap();
            let data = util::data_dir().unwrap();
            let desktop = user_desktop().unwrap();
            fs::create_dir_all(&desktop).unwrap();
            let claude_link = desktop.join("Claude Desktop.lnk");
            util::powershell(&format!(
                "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath=(Join-Path $env:WINDIR 'explorer.exe'); $s.Arguments={}; $s.IconLocation={}; $s.Save()",
                ps_path(&claude_link),
                ps_quote(format!(r"shell:AppsFolder\{}", crate::claude::APP_ID)),
                ps_quote(format!("{},0", data.join("icons/claude-desktop-app.ico").display())),
            ))
            .unwrap();

            let error = uninstall(&operation).err().unwrap();
            assert!(error.contains("UAC"), "{error}");
            assert!(source.is_file() && destination.join("install.json").is_file());
            assert!(installed_record().unwrap().is_some());
            assert!(data.join("config.json").is_file() && claude_link.is_file());
            // No wrapper started, so this process removed its own scripts.
            let prefix = format!("claude-windows-cn-uninstall-{}-", std::process::id());
            let left = fs::read_dir(env::temp_dir())
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                .count();
            assert_eq!(left, 0);
            return;
        }
        let scratch = scratch("claude-cancelled-start");
        let portable = scratch.join("Portable");
        fs::create_dir_all(&portable).unwrap();
        let child_exe = portable.join(util::EXE_NAME);
        fs::copy(env::current_exe().unwrap(), &child_exe).unwrap();
        let output = Command::new(&child_exe)
            .args([
                "--exact",
                "assistant::tests::cancelled_start_from_the_running_copy_leaves_nothing_behind",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CLAUDE_WINDOWS_CN_TEST_CANCEL_ELEVATION", "1")
            .env("LOCALAPPDATA", scratch.join("Data"))
            .env("CLAUDE_WINDOWS_CN_TEST_DESKTOP", scratch.join("Desktop"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(scratch).unwrap();
    }

    // Runs `name` again in a copy of this test binary placed in `portable` with
    // its own install.json, `local` as LOCALAPPDATA and a desktop beside it.
    #[cfg(windows)]
    fn run_as_portable_copy(name: &str, child: &str, portable: &Path, local: &Path) {
        fs::create_dir_all(portable).unwrap();
        let child_exe = portable.join(util::EXE_NAME);
        fs::copy(env::current_exe().unwrap(), &child_exe).unwrap();
        let manifest = InstallManifest {
            product: "claude-windows-cn".into(),
            mode: "portable".into(),
            install_path: portable.display().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        fs::write(
            portable.join("install.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let output = Command::new(&child_exe)
            .args([
                &format!("assistant::tests::{name}"),
                "--exact",
                "--nocapture",
            ])
            .env(child, "1")
            .env("LOCALAPPDATA", local)
            .env(
                "CLAUDE_WINDOWS_CN_TEST_DESKTOP",
                local.parent().unwrap().join("Desktop"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // A program that just exited, or a scanner looking at it, can hold its file
    // for a moment.
    #[cfg(windows)]
    fn remove_scratch(path: &Path) {
        for _ in 0..50 {
            if fs::remove_dir_all(path).is_ok() || !path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        util::powershell(&format!(
            "New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
            ps_path(link),
            ps_path(target)
        ))
        .unwrap();
    }

    #[cfg(windows)]
    fn claude_shortcut_with_icon(link: &Path, icon: &Path) {
        util::powershell(&format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath=(Join-Path $env:WINDIR 'explorer.exe'); $s.Arguments={}; $s.IconLocation={}; $s.Save()",
            ps_path(link),
            ps_quote(format!(r"shell:AppsFolder\{}", crate::claude::APP_ID)),
            ps_quote(format!("{},0", icon.display())),
        ))
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn portable_copy_adopts_only_its_own_per_user_data() {
        const NAME: &str = "portable_copy_adopts_only_its_own_per_user_data";
        const CHILD: &str = "CLAUDE_WINDOWS_CN_ADOPT_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let root = env::current_exe().unwrap().parent().unwrap().to_path_buf();
            let data = util::data_dir().unwrap();
            assert_eq!(data, root.join("data"));
            let legacy = util::legacy_data_dir().unwrap();
            let record = |install_path: &Path| {
                serde_json::to_vec(&InstallManifest {
                    product: "claude-windows-cn".into(),
                    mode: "portable".into(),
                    install_path: install_path.display().to_string(),
                    version: "0.2.5".into(),
                })
                .unwrap()
            };
            let fill = |install_path: &Path| {
                for dir in [
                    "official-msix",
                    "icons",
                    "patch-engine/cache",
                    "patch-engine/run-1-2",
                ] {
                    fs::create_dir_all(legacy.join(dir)).unwrap();
                }
                fs::write(legacy.join("installed.json"), record(install_path)).unwrap();
                for (name, content) in [
                    ("config.json", "{}"),
                    ("patch-backup.json", "{}"),
                    ("patch-pending.json", "{}"),
                    ("patch-engine/run-1-2/engine.log", "log"),
                    ("patch-engine/cache/engine.zip", "zip"),
                    ("official-msix/Claude-1.0.msix", "msix"),
                    ("icons/claude-desktop-app.ico", "ico"),
                ] {
                    fs::write(legacy.join(name), content).unwrap();
                }
            };

            // Recorded for another installation: neither copied nor removed.
            fill(&root.parent().unwrap().join("Elsewhere"));
            adopt_legacy_data().unwrap();
            assert!(legacy.join("config.json").is_file());
            assert!(!data.join("config.json").exists());
            fs::remove_dir_all(&legacy).unwrap();

            // An earlier version of this copy, recorded in another case: records
            // and run logs move, caches do not, the Claude shortcut follows its
            // icon, and the old directory goes.
            fill(&PathBuf::from(root.display().to_string().to_uppercase()));
            // A copy cut short last time is redone; a record already here stays.
            fs::write(data.join("config.json.adopting"), "{").unwrap();
            fs::write(data.join("patch-backup.json"), "kept").unwrap();
            let desktop = user_desktop().unwrap();
            fs::create_dir_all(&desktop).unwrap();
            let claude_link = desktop.join("Claude Desktop.lnk");
            claude_shortcut_with_icon(&claude_link, &legacy.join("icons/claude-desktop-app.ico"));
            adopt_legacy_data().unwrap();
            for name in [
                "config.json",
                "installed.json",
                "patch-pending.json",
                "patch-engine/run-1-2/engine.log",
                "icons/claude-desktop-app.ico",
            ] {
                assert!(data.join(name).is_file(), "{name}");
            }
            assert_eq!(fs::read_to_string(data.join("config.json")).unwrap(), "{}");
            assert!(!data.join("config.json.adopting").exists());
            assert_eq!(
                fs::read_to_string(data.join("patch-backup.json")).unwrap(),
                "kept"
            );
            assert!(!data.join("official-msix").exists());
            assert!(!data.join("patch-engine/cache").exists());
            assert!(!legacy.exists());
            let icon = util::powershell(&format!(
                "(New-Object -ComObject WScript.Shell).CreateShortcut({}).IconLocation",
                ps_path(&claude_link)
            ))
            .unwrap();
            assert_eq!(
                icon.replace('/', "\\"),
                format!(
                    "{},0",
                    data.join("icons").join("claude-desktop-app.ico").display()
                )
            );
            // Nothing left to adopt is fine.
            adopt_legacy_data().unwrap();
            return;
        }
        let scratch = scratch(NAME);
        run_as_portable_copy(
            NAME,
            CHILD,
            &scratch.join("Portable"),
            &scratch.join("Local"),
        );
        remove_scratch(&scratch);
    }

    #[cfg(windows)]
    #[test]
    fn adoption_never_deletes_what_another_path_leads_back_to() {
        const NAME: &str = "adoption_never_deletes_what_another_path_leads_back_to";
        const CHILD: &str = "CLAUDE_WINDOWS_CN_ADOPT_ALIAS_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            // Nothing is recorded for another installation, so only the overlap
            // check stands between this copy and the old directory.
            let legacy = util::legacy_data_dir().unwrap();
            adopt_legacy_data().unwrap();
            assert!(legacy.join("config.json").is_file());
            return;
        }
        for case in 0..3 {
            let scratch = scratch(NAME);
            let local = scratch.join("Local");
            let legacy = local.join(util::PRODUCT_DIR);
            fs::create_dir_all(&legacy).unwrap();
            fs::write(legacy.join("config.json"), "{}").unwrap();
            let (portable, local) = match case {
                // This copy's data is a junction to the old directory.
                0 => {
                    let portable = scratch.join("Portable");
                    fs::create_dir_all(&portable).unwrap();
                    junction(&portable.join("data"), &legacy);
                    (portable, local)
                }
                // This copy lives in the old directory, reached through a junction.
                1 => {
                    junction(&scratch.join("Alias"), &legacy);
                    (scratch.join("Alias").join("Portable"), local)
                }
                // This copy lives in the old directory, named in another case.
                _ => (legacy.join("Portable"), scratch.join("LOCAL")),
            };
            run_as_portable_copy(NAME, CHILD, &portable, &local);
            assert!(legacy.join("config.json").is_file(), "case {case}");
            remove_scratch(&scratch);
        }
    }

    #[cfg(windows)]
    #[test]
    fn portable_copy_falls_back_when_its_data_refuses_new_files() {
        const NAME: &str = "portable_copy_falls_back_when_its_data_refuses_new_files";
        const CHILD: &str = "CLAUDE_WINDOWS_CN_PORTABLE_FALLBACK_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let root = env::current_exe().unwrap().parent().unwrap().to_path_buf();
            let legacy = util::legacy_data_dir().unwrap();
            assert_eq!(util::data_dir().unwrap(), legacy);
            // Settings and adoption keep to that one place.
            let mut config = load_config().unwrap();
            config.assistant_install_mode = "portable".into();
            config.assistant_path = root.display().to_string();
            config.daily_update_check = false;
            save_config(config).unwrap();
            adopt_legacy_data().unwrap();
            assert!(legacy.join("config.json").is_file());
            assert!(!root.join("data/config.json").exists());
            return;
        }
        let scratch = scratch(NAME);
        let portable = scratch.join("Portable");
        let data = portable.join("data");
        fs::create_dir_all(&data).unwrap();
        // Refuses new files and folders, which the ReadOnly attribute would not.
        let (sid, _) = util::caller_identity().unwrap();
        let icacls = |args: [&str; 2]| {
            let output = Command::new("icacls")
                .arg(&data)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        };
        icacls(["/deny", &format!("*{sid}:(WD,AD)")]);
        let outcome = std::panic::catch_unwind(|| {
            run_as_portable_copy(NAME, CHILD, &portable, &scratch.join("Local"))
        });
        icacls(["/remove:d", &format!("*{sid}")]);
        outcome.unwrap();
        remove_scratch(&scratch);
    }

    #[cfg(windows)]
    #[test]
    fn moved_portable_copy_installs_and_uninstalls_in_its_new_folder() {
        const NAME: &str = "moved_portable_copy_installs_and_uninstalls_in_its_new_folder";
        const CHILD: &str = "CLAUDE_WINDOWS_CN_PORTABLE_UNINSTALL_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let root = env::current_exe().unwrap().parent().unwrap().to_path_buf();
            // The records and settings the folder carried from before the move.
            let old = root.with_file_name("Old");
            let record = InstallManifest {
                product: "claude-windows-cn".into(),
                mode: "portable".into(),
                install_path: old.display().to_string(),
                version: env!("CARGO_PKG_VERSION").into(),
            };
            fs::write(
                root.join("install.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            save_installed_record(&record).unwrap();
            let carried = AssistantConfig {
                assistant_install_mode: "portable".into(),
                assistant_path: old.display().to_string(),
                create_assistant_shortcut: false,
                daily_update_check: false,
                auto_reapply_patch: true,
            };
            util::write_json(&config_path().unwrap(), &carried).unwrap();
            let recorded = || {
                read_installed_record(&installed_record_path().unwrap())
                    .unwrap()
                    .unwrap()
                    .install_path
            };

            // The settings cannot be read the first time, as when another
            // program holds them open.
            use std::os::windows::fs::OpenOptionsExt;
            let lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(config_path().unwrap())
                .unwrap();
            let error = follow_moved_portable_folder().unwrap_err();
            assert!(error.contains("读取助手设置失败"), "{error}");
            // Not finished, so the next start redoes it; meanwhile nothing acts
            // on the copy still in the old folder.
            assert_eq!(recorded(), old.display().to_string());
            let refused = uninstall(&OperationState::new()).err().unwrap();
            assert!(refused.contains("不是当前便携副本"), "{refused}");
            drop(lock);

            follow_moved_portable_folder().unwrap();
            assert_eq!(recorded(), root.display().to_string());
            assert!(read_manifest(&root).unwrap().is_some());
            let current = status().unwrap();
            assert!(current.installed);
            assert_eq!(current.install_path, root.display().to_string());
            let config = load_config().unwrap();
            assert_eq!(config.assistant_path, root.display().to_string());
            install(config, &OperationState::new()).unwrap();
            fs::create_dir_all(root.join("data/official-msix")).unwrap();
            fs::write(root.join("data/official-msix/Claude-1.0.msix"), "msix").unwrap();
            // This process is the installed copy: a script finishes after it exits.
            let outcome = uninstall(&OperationState::new());
            assert!(outcome.is_ok(), "{:?}", outcome.err());
            return;
        }
        let scratch = scratch(NAME);
        // A copy left in the old folder, as when the folder was copied rather
        // than moved, is not this one's to remove.
        let old = scratch.join("Old");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(util::EXE_NAME), "another copy").unwrap();
        let old_manifest = serde_json::to_vec(&InstallManifest {
            product: "claude-windows-cn".into(),
            mode: "portable".into(),
            install_path: old.display().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
        })
        .unwrap();
        fs::write(old.join("install.json"), &old_manifest).unwrap();
        let portable = scratch.join("Portable");
        run_as_portable_copy(NAME, CHILD, &portable, &scratch.join("Local"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while portable.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // Program, install.json and the data inside, then the emptied folder.
        assert!(
            !portable.exists(),
            "{:?}",
            fs::read_dir(&portable).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
        );
        assert_eq!(fs::read(old.join(util::EXE_NAME)).unwrap(), b"another copy");
        assert_eq!(fs::read(old.join("install.json")).unwrap(), old_manifest);
        remove_scratch(&scratch);
    }

    #[cfg(windows)]
    #[test]
    fn portable_in_place_does_not_copy_executable() {
        const CHILD: &str = "CLAUDE_WINDOWS_CN_PORTABLE_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let source = env::current_exe().unwrap();
            let destination = source.parent().unwrap();
            assert_eq!(Path::new(&defaults().unwrap().portable), destination);
            let original_hash = util::sha256(&source).unwrap();
            let config = AssistantConfig {
                assistant_install_mode: "portable".into(),
                assistant_path: destination.display().to_string(),
                create_assistant_shortcut: false,
                daily_update_check: false,
                auto_reapply_patch: true,
            };
            save_config(config.clone()).unwrap();
            let operation = OperationState::new();
            install(config.clone(), &operation).unwrap();
            install(config, &operation).unwrap();
            assert_eq!(util::sha256(&source).unwrap(), original_hash);
            assert_eq!(
                read_manifest(destination).unwrap().unwrap().mode,
                "portable"
            );
            assert_eq!(
                status().unwrap().install_path,
                destination.display().to_string()
            );
            let mut entries = fs::read_dir(destination)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            entries.sort();
            // The original EXE, install.json, and the data kept beside them.
            assert_eq!(entries, [util::EXE_NAME, "data", "install.json"]);
            // Settings saved before installing in place moved into that data,
            // and the per-user directory used until then is gone.
            assert_eq!(util::data_dir().unwrap(), destination.join("data"));
            assert!(destination.join("data/config.json").is_file());
            assert!(destination.join("data/installed.json").is_file());
            assert!(!util::legacy_data_dir().unwrap().exists());
            return;
        }
        let scratch = env::temp_dir().join(format!(
            "claude-in-place-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let portable = scratch.join("Portable");
        fs::create_dir_all(&portable).unwrap();
        let child_exe = portable.join(util::EXE_NAME);
        fs::copy(env::current_exe().unwrap(), &child_exe).unwrap();
        let output = Command::new(&child_exe)
            .args([
                "--exact",
                "assistant::tests::portable_in_place_does_not_copy_executable",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("LOCALAPPDATA", scratch.join("Data"))
            .env("CLAUDE_WINDOWS_CN_TEST_DESKTOP", scratch.join("Desktop"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let resolved = fs::canonicalize(&scratch).unwrap();
        let temporary_root = fs::canonicalize(env::temp_dir()).unwrap();
        assert!(resolved.starts_with(&temporary_root) && resolved != temporary_root);
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn rejects_drive_root_and_relative_paths() {
        let mut config = AssistantConfig {
            assistant_install_mode: "user".into(),
            assistant_path: r"C:\".into(),
            create_assistant_shortcut: true,
            daily_update_check: true,
            auto_reapply_patch: true,
        };
        assert!(validate_config(&config).is_err());
        config.assistant_path = r"temp\assistant".into();
        assert!(validate_config(&config).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "会短暂创建用户/系统开始菜单和卸载项；须管理员权限且目标为空，单独运行"]
    fn isolated_assistant_install_round_trip() {
        const CHILD: &str = "CLAUDE_WINDOWS_CN_INSTALL_TEST_CHILD";
        const TEST_NAME: &str = "assistant::tests::isolated_assistant_install_round_trip";
        let system = env::var_os("CLAUDE_WINDOWS_CN_SYSTEM_INSTALL_TEST").is_some();
        let registry = r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ClaudeWindowsCN";
        let registry = if system {
            registry.replace("HKCU:", "HKLM:")
        } else {
            registry.into()
        };
        let registry = registry.as_str();
        let menu_script = "[IO.Path]::Combine([Environment]::GetFolderPath('Programs'), 'Claude Windows 中文助手.lnk')";
        let menu = util::powershell(&if system {
            menu_script.replace("Programs", "CommonPrograms")
        } else {
            menu_script.into()
        })
        .unwrap();
        let menu = PathBuf::from(menu);
        let preflight = format!(
            "if (Test-Path -LiteralPath {}) {{ throw '已有同名开始菜单入口。' }}; if (Test-Path -LiteralPath {}) {{ throw '已有助手卸载项。' }}",
            ps_path(&menu),
            ps_quote(registry)
        );

        if env::var_os(CHILD).is_some() {
            util::powershell(&preflight).unwrap();
            let scratch = util::local_app_data().unwrap();
            let state = OperationState::new();
            let desktop = user_desktop().unwrap();
            fs::create_dir_all(&desktop).unwrap();
            let desktop_link = desktop.join("Claude Windows 中文助手.lnk");
            for (mode, destination) in [
                ("portable", scratch.join("Portable").join(util::PRODUCT_DIR)),
                (
                    if system { "system" } else { "user" },
                    scratch.join("Programs").join(util::PRODUCT_DIR),
                ),
            ] {
                let config = AssistantConfig {
                    assistant_install_mode: mode.into(),
                    assistant_path: destination.display().to_string(),
                    create_assistant_shortcut: true,
                    daily_update_check: false,
                    auto_reapply_patch: true,
                };
                save_config(config).unwrap();
                let loaded = load_config().unwrap();
                assert_eq!(loaded.assistant_install_mode, mode);
                assert_eq!(loaded.assistant_path, destination.display().to_string());
                assert!(loaded.create_assistant_shortcut);
                assert!(!loaded.daily_update_check);
                let persisted: AssistantConfig =
                    serde_json::from_slice(&fs::read(config_path().unwrap()).unwrap()).unwrap();
                assert_eq!(persisted.assistant_path, loaded.assistant_path);

                install(loaded.clone(), &state).unwrap();
                let target = destination.join(util::EXE_NAME);
                assert!(target.is_file());
                assert_eq!(
                    util::sha256(&target).unwrap(),
                    util::sha256(&env::current_exe().unwrap()).unwrap()
                );
                assert_eq!(read_manifest(&destination).unwrap().unwrap().mode, mode);
                let record = installed_record().unwrap().unwrap();
                assert_eq!(record.install_path, destination.display().to_string());
                assert_eq!(record.mode, mode);
                assert_eq!(status().unwrap().mode.as_deref(), Some(mode));

                let verify_desktop = format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); if ($s.TargetPath -ne {}) {{ throw '桌面目标不匹配。' }}", ps_path(&desktop_link), ps_path(&target));
                util::powershell(&verify_desktop).unwrap();
                let foreign = scratch.join("foreign.exe");
                util::powershell(&format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath={}; $s.Save()", ps_path(&desktop_link), ps_path(&foreign))).unwrap();
                let previous_hash = util::sha256(&target).unwrap();
                let error = install(loaded.clone(), &state).err().unwrap();
                assert!(error.contains("同名桌面快捷方式"), "{error}");
                assert_eq!(previous_hash, util::sha256(&target).unwrap());
                util::powershell(&format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath={}; $s.Save()", ps_path(&desktop_link), ps_path(&target))).unwrap();
                if mode != "portable" {
                    let verify = format!(
                        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); if ($s.TargetPath -ne {}) {{ throw '开始菜单目标不匹配。' }}; $r=Get-ItemProperty -Path {}; if ($r.InstallLocation -ne {}) {{ throw '卸载项安装目录不匹配。' }}",
                        ps_path(&menu), ps_path(&target), ps_quote(registry), ps_path(&destination)
                    );
                    util::powershell(&verify).unwrap();

                    let foreign = scratch.join("foreign.exe");
                    fs::write(&target, b"ownership-preflight-sentinel").unwrap();
                    util::powershell(&format!(
                        "Set-ItemProperty -Path {} -Name DisplayIcon -Value {}",
                        ps_quote(registry),
                        ps_path(&foreign)
                    ))
                    .unwrap();
                    let error = install(loaded, &state).err().unwrap();
                    assert!(error.contains("同名卸载项"), "{error}");
                    assert_eq!(fs::read(&target).unwrap(), b"ownership-preflight-sentinel");
                    fs::copy(env::current_exe().unwrap(), &target).unwrap();
                    util::powershell(&format!(
                        "Set-ItemProperty -Path {} -Name DisplayIcon -Value {}",
                        ps_quote(registry),
                        ps_path(&target)
                    ))
                    .unwrap();

                    util::powershell(&format!(
                        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath={}; $s.Save()",
                        ps_path(&menu), ps_path(&foreign)
                    )).unwrap();
                    let error = uninstall(&state).err().unwrap();
                    assert!(error.contains("同名开始菜单"), "{error}");
                    assert!(target.is_file());
                    assert!(installed_record_path().unwrap().exists());
                    util::powershell(&format!(
                        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath={}; $s.Save()",
                        ps_path(&menu), ps_path(&target)
                    )).unwrap();
                } else {
                    util::powershell(&preflight).unwrap();
                }

                let sentinel = destination.join("user-data.txt");
                fs::write(&sentinel, b"preserve-user-data").unwrap();
                uninstall(&state).unwrap();
                assert_eq!(fs::read(&sentinel).unwrap(), b"preserve-user-data");
                assert!(!desktop_link.exists());
                assert!(!target.exists());
                assert!(!installed_record_path().unwrap().exists());
                assert!(!status().unwrap().installed);
                util::powershell(&preflight).unwrap();
            }
            return;
        }

        util::powershell(&preflight).unwrap();
        let real_record = fs::read(installed_record_path().unwrap()).ok();
        let real_config = fs::read(config_path().unwrap()).ok();
        let desktop_shortcut = user_desktop().unwrap().join("Claude Windows 中文助手.lnk");
        let real_desktop_shortcut = fs::read(&desktop_shortcut).ok();
        let scratch = env::temp_dir().join(format!(
            "claude-windows-cn-install-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&scratch).unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                TEST_NAME,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("LOCALAPPDATA", &scratch)
            .env("CLAUDE_WINDOWS_CN_TEST_DESKTOP", scratch.join("Desktop"))
            .output()
            .unwrap();
        let destination = scratch.join("Programs").join(util::PRODUCT_DIR);
        let target = destination.join(util::EXE_NAME);
        let foreign = scratch.join("foreign.exe");
        let cleanup_registration = format!(
            "$menu={}; $target={}; $foreign={}; $key={}; $dest={}; if (Test-Path -LiteralPath $menu) {{ $s=(New-Object -ComObject WScript.Shell).CreateShortcut($menu); if ($s.TargetPath -ne $target -and $s.TargetPath -ne $foreign) {{ throw '同名开始菜单入口目标已变化，未清理。' }}; Remove-Item -LiteralPath $menu -Force }}; if (Test-Path -LiteralPath $key) {{ $r=Get-ItemProperty -Path $key; if ($r.InstallLocation -ne $dest) {{ throw '同名卸载项目标已变化，未清理。' }}; Remove-Item -LiteralPath $key -Force }}",
            ps_path(&menu), ps_path(&target), ps_path(&foreign), ps_quote(registry), ps_path(&destination)
        );
        util::powershell(&cleanup_registration).unwrap();
        assert_eq!(fs::read(installed_record_path().unwrap()).ok(), real_record);
        assert_eq!(fs::read(config_path().unwrap()).ok(), real_config);
        assert_eq!(fs::read(&desktop_shortcut).ok(), real_desktop_shortcut);
        assert!(
            output.status.success(),
            "隔离安装测试失败，保留现场 {}\nstdout: {}\nstderr: {}",
            scratch.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        util::powershell(&preflight).unwrap();
        assert!(!scratch
            .join(util::PRODUCT_DIR)
            .join("installed.json")
            .exists());

        let temporary_root = fs::canonicalize(env::temp_dir()).unwrap();
        let resolved_scratch = fs::canonicalize(&scratch).unwrap();
        assert!(
            resolved_scratch.starts_with(&temporary_root) && resolved_scratch != temporary_root
        );
        fs::remove_dir_all(&resolved_scratch).unwrap();
        println!(
            "portable 和选定 registered 模式安装/卸载、快捷方式、冲突保护及配置持久化通过；临时目录已清理：{}",
            scratch.display()
        );
    }
}
