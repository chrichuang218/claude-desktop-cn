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
    let path = installed_record_path()?;
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
        crate::self_update::trigger_daily_check();
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
    let manifest =
        read_manifest(&path)?.ok_or_else(|| "当前路径没有本助手的安装记录。".to_string())?;
    if manifest.mode != record.mode {
        return Err("助手安装位置记录与目标安装方式不一致。".into());
    }
    if !path.join(util::EXE_NAME).is_file() {
        return Err("助手安装文件不存在，已停止卸载。".into());
    }
    let work = util::data_dir()?.join("installer");
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let script_path = work.join("uninstall-assistant.ps1");
    let record_path = installed_record_path()?;
    let desktop = user_desktop()?.join("Claude Windows 中文助手.lnk");
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
$record={record}
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
if ($removeLink) {{ Remove-Item -LiteralPath $link -Force -ErrorAction Stop }}
if ($removeMenu) {{ Remove-Item -LiteralPath $menu -Force -ErrorAction Stop }}
if ($removeKey) {{ Remove-Item -LiteralPath $key -Force -ErrorAction Stop }}
Remove-Item -LiteralPath $exe -Force -ErrorAction Stop
Remove-Item -LiteralPath $manifest -Force -ErrorAction Stop
Remove-Item -LiteralPath $record -Force -ErrorAction Stop
if (@(Get-ChildItem -LiteralPath $dest -Force).Count -eq 0) {{ Remove-Item -LiteralPath $dest -Force -ErrorAction Stop }}
"#,
        dest = ps_path(&path),
        exe = ps_quote(util::EXE_NAME),
        link = ps_path(&desktop),
        record = ps_path(&record_path),
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
    util::write_script(&script_path, &script)?;
    operation.step("卸载 Claude 中文助手");
    if running_target {
        let result_path = work.join("uninstall-result.json");
        if result_path.exists() {
            fs::remove_file(&result_path).map_err(|error| error.to_string())?;
        }
        let wrapper_path = work.join("run-uninstall.ps1");
        let wrapper = format!(
            r#"$ErrorActionPreference='Stop'
            $result={result}
            try {{
                $output=& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {script} 2>&1 | Out-String
                if ($LASTEXITCODE -ne 0) {{ throw $output }}
                @{{ ok=$true; message='助手文件与入口已卸载。' }} | ConvertTo-Json -Compress | Set-Content -LiteralPath $result -Encoding UTF8
            }} catch {{
                @{{ ok=$false; message=$_.Exception.Message }} | ConvertTo-Json -Compress | Set-Content -LiteralPath $result -Encoding UTF8
            }}
        "#,
            result = ps_path(&result_path),
            script = ps_path(&script_path)
        );
        util::write_script(&wrapper_path, &wrapper)?;
        let launch = format!("Start-Process -FilePath 'powershell.exe' -ArgumentList {} {} -WindowStyle Hidden | Out-Null", ps_quote(format!("-NoProfile -ExecutionPolicy Bypass -File \"{}\"", wrapper_path.display())), if manifest.mode == "system" { "-Verb RunAs" } else { "" });
        util::powershell(&launch)?;
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            std::process::exit(0);
        });
        Ok(OperationOutcome::done(
            "卸载脚本已启动，助手退出后将删除程序文件。",
        ))
    } else {
        let log = if manifest.mode == "system" {
            run_system_script(&script_path, &work)?
        } else {
            util::run_script(&script_path)?
        };
        operation.log(log);
        if path.join(util::EXE_NAME).exists() {
            return Err("卸载脚本已退出，但助手文件仍存在。".into());
        }
        Ok(OperationOutcome::done("助手已卸载。"))
    }
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
            let data = destination.join("data");
            assert!(data.join("config.json").is_file());
            assert!(data.join("installed.json").is_file());
            let legacy =
                PathBuf::from(env::var_os("LOCALAPPDATA").unwrap()).join(util::PRODUCT_DIR);
            assert!(legacy.join("config.json").is_file());
            let entries = fs::read_dir(destination)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                entries.len(),
                3,
                "Portable root should contain only the EXE, install.json and data"
            );
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
