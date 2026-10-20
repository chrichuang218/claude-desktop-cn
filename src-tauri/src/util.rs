include!("util_shared.rs");

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const PRODUCT_DIR: &str = "ClaudeWindowsCN";
pub const EXE_NAME: &str = "claude-windows-cn.exe";
const HELPER_FLAG: &str = "--run-assistant-helper";
const PORTABLE_DATA_DIR: &str = "data";
const PORTABLE_MIGRATION_MARKER: &str = ".migrated-from-local-app-data";

pub fn data_dir() -> Result<PathBuf, String> {
    let legacy = local_app_data()?.join(PRODUCT_DIR);
    let Some(root) = portable_install_root()? else {
        return Ok(legacy);
    };
    let portable = root.join(PORTABLE_DATA_DIR);
    fs::create_dir_all(&portable).map_err(|error| format!("创建便携版数据目录失败：{error}"))?;
    migrate_legacy_data(&legacy, &portable)?;
    Ok(portable)
}

pub fn local_app_data() -> Result<PathBuf, String> {
    env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "无法读取当前用户 LocalAppData 目录。".to_string())
}

fn portable_install_root() -> Result<Option<PathBuf>, String> {
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let Some(root) = exe.parent() else {
        return Ok(None);
    };
    Ok(is_portable_install(root).then(|| root.to_path_buf()))
}

fn is_portable_install(root: &Path) -> bool {
    let manifest = root.join("install.json");
    let Ok(raw) = fs::read_to_string(manifest) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value.get("product").and_then(|item| item.as_str()) == Some("claude-windows-cn")
        && value.get("mode").and_then(|item| item.as_str()) == Some("portable")
}

fn migrate_legacy_data(legacy: &Path, portable: &Path) -> Result<(), String> {
    let marker = portable.join(PORTABLE_MIGRATION_MARKER);
    if marker.exists() || !legacy.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(legacy).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        copy_missing(&entry.path(), &portable.join(entry.file_name()))?;
    }
    fs::write(&marker, b"1\n").map_err(|error| format!("写入迁移标记失败：{error}"))
}

fn copy_missing(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if target.exists() {
        if metadata.is_dir() && target.is_dir() {
            for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                copy_missing(&entry.path(), &target.join(entry.file_name()))?;
            }
        }
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(target).map_err(|error| error.to_string())?;
        for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            copy_missing(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::copy(source, target).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn ps_quote(value: impl AsRef<str>) -> String {
    format!("'{}'", value.as_ref().replace('\'', "''"))
}

pub fn ps_path(path: &Path) -> String {
    ps_quote(path.to_string_lossy())
}

fn powershell_command() -> Command {
    let mut command = Command::new("powershell.exe");
    // PowerShell 7's module paths can make Windows PowerShell load incompatible
    // built-in modules. Let Windows PowerShell rebuild its own default paths.
    command.env_remove("PSModulePath");
    hide_window(&mut command);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
    ]);
    command
}

pub fn powershell(script: &str) -> Result<String, String> {
    let script = format!(
        // Older .NET defaults Windows PowerShell 5.1 to SSL3/TLS 1.0, which GitHub rejects.
        // Add TLS 1.2 only then; SystemDefault (0) already lets the OS negotiate.
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; if ([int][Net.ServicePointManager]::SecurityProtocol -ne 0) {{ [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 3072 }}; {script}"
    );
    let output = powershell_command()
        .args(["-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("PowerShell 启动失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        Err(format!(
            "PowerShell 退出码 {:?}：{stdout}",
            output.status.code()
        ))
    } else {
        Err(stderr)
    }
}

pub fn caller_identity() -> Result<(String, u32), String> {
    let identity = powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; (Get-Process -Id $PID).SessionId",
    )?;
    let mut lines = identity.lines();
    let sid = lines.next().ok_or("无法读取原调用用户 SID。")?.trim();
    let session: u32 = lines
        .next()
        .ok_or("无法读取原调用会话。")?
        .trim()
        .parse()
        .map_err(|_| "原调用会话无效。")?;
    if !sid.starts_with("S-1-") {
        return Err("原调用用户 SID 无效。".into());
    }
    Ok((sid.to_string(), session))
}

pub fn download(url: &str, destination: &Path) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("下载地址必须使用 HTTPS。".to_string());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建下载目录失败：{error}"))?;
    }
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri {} -OutFile {} -UseBasicParsing; if ((Get-Item -LiteralPath {}).Length -eq 0) {{ throw '下载文件为空。' }}",
        ps_quote(url), ps_path(destination), ps_path(destination),
    );
    powershell(&script).map(|_| ())
}

pub fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(0x08000000);
}

pub fn write_script(path: &Path, content: &str) -> Result<(), String> {
    let mut bytes = vec![0xef, 0xbb, 0xbf];
    bytes.extend_from_slice(content.as_bytes());
    fs::write(path, bytes).map_err(|error| format!("写入 {} 失败：{error}", path.display()))
}

pub fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("记录目录无效。")?)
        .map_err(|error| error.to_string())?;
    let stage = path.with_extension("json.tmp");
    fs::write(
        &stage,
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("写入 {} 失败：{error}", path.display()))?;
    replace_file(&stage, path)
}

pub fn run_script(script: &Path) -> Result<String, String> {
    let output = powershell_command()
        .arg("-File")
        .arg(script)
        .output()
        .map_err(|error| format!("启动脚本失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(format!("{stdout}\n{stderr}").trim().to_string())
    } else {
        Err(format!(
            "脚本退出码 {:?}\n{stdout}\n{stderr}",
            output.status.code()
        ))
    }
}

pub fn run_elevated_script(script: &Path, work_dir: &Path) -> Result<String, String> {
    run_elevated_script_with_logs(script, work_dir, |_| {})
}

pub fn run_elevated_script_with_logs(
    script: &Path,
    work_dir: &Path,
    mut on_log: impl FnMut(&str),
) -> Result<String, String> {
    let completion = work_dir.join("helper-result.json");
    let live_log = work_dir.join("helper-live.log");
    if completion.exists() {
        fs::remove_file(&completion).map_err(|error| error.to_string())?;
    }
    if live_log.exists() {
        fs::remove_file(&live_log).map_err(|error| error.to_string())?;
    }
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let args = format!(
        "{HELPER_FLAG} \"{}\" \"{}\"",
        script.display(),
        completion.display()
    );
    let launch = format!(
        "Start-Process -FilePath {} -ArgumentList {} -WorkingDirectory {} -Verb RunAs -WindowStyle Hidden | Out-Null",
        ps_path(&exe), ps_quote(args), ps_path(work_dir)
    );
    powershell(&launch).map_err(|error| format!("管理员操作未启动（可能取消了 UAC）：{error}"))?;
    let start = Instant::now();
    let mut last_log_bytes = 0usize;
    while start.elapsed() < Duration::from_secs(20 * 60) {
        if let Ok(bytes) = fs::read(&live_log) {
            if bytes.len() > last_log_bytes {
                let chunk = String::from_utf8_lossy(&bytes[last_log_bytes..]);
                on_log(&chunk);
                last_log_bytes = bytes.len();
            }
        }
        if completion.exists() {
            let raw = fs::read_to_string(&completion).map_err(|error| error.to_string())?;
            let result: HelperResult = serde_json::from_str(&raw)
                .map_err(|error| format!("管理员操作结果无效：{error}"))?;
            return if result.ok {
                Ok(result.log)
            } else {
                Err(result.log)
            };
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err("管理员操作等待超时，请检查 UAC 和操作日志。".to_string())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HelperResult {
    ok: bool,
    log: String,
}

pub fn run_helper_if_requested() -> bool {
    let mut args = env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER_FLAG)) {
        return false;
    }
    let Some(script) = args.next().map(PathBuf::from) else {
        std::process::exit(2);
    };
    let Some(result_path) = args.next().map(PathBuf::from) else {
        std::process::exit(2);
    };
    let live_log = result_path.with_file_name("helper-live.log");
    let result = match run_script_to_log(&script, &live_log) {
        Ok(log) => HelperResult { ok: true, log },
        Err(log) => HelperResult { ok: false, log },
    };
    let code = if result.ok { 0 } else { 1 };
    if let Ok(bytes) = serde_json::to_vec(&result) {
        let _ = fs::write(result_path, bytes);
    }
    std::process::exit(code);
}

fn run_script_to_log(script: &Path, log_path: &Path) -> Result<String, String> {
    let file = fs::File::create(log_path).map_err(|error| error.to_string())?;
    let stderr = file.try_clone().map_err(|error| error.to_string())?;
    let status = powershell_command()
        .arg("-File")
        .arg(script)
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr))
        .status()
        .map_err(|error| format!("启动管理员脚本失败：{error}"))?;
    let log = String::from_utf8_lossy(&fs::read(log_path).unwrap_or_default()).to_string();
    if status.success() {
        Ok(log)
    } else {
        Err(format!("管理员脚本退出码 {:?}\n{log}", status.code()))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn portable_manifest_selects_root_data_storage() {
        let root = env::temp_dir().join(format!(
            "claude-windows-cn-portable-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let manifest = root.join("install.json");
        fs::write(
            &manifest,
            r#"{"product":"claude-windows-cn","mode":"portable"}"#,
        )
        .unwrap();
        assert!(is_portable_install(&root));
        fs::write(
            &manifest,
            r#"{"product":"claude-windows-cn","mode":"user"}"#,
        )
        .unwrap();
        assert!(!is_portable_install(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn portable_data_migration_copies_missing_legacy_data_once() {
        let root = env::temp_dir().join(format!(
            "claude-windows-cn-portable-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = root.join("legacy");
        let portable = root.join("data");
        fs::create_dir_all(legacy.join("patch-engine/cache")).unwrap();
        fs::create_dir_all(&portable).unwrap();
        fs::write(legacy.join("config.json"), "legacy-config").unwrap();
        fs::write(
            legacy.join("patch-engine/cache/engine.zip"),
            "cached-engine",
        )
        .unwrap();
        fs::write(portable.join("config.json"), "portable-config").unwrap();

        migrate_legacy_data(&legacy, &portable).unwrap();
        assert_eq!(
            fs::read_to_string(portable.join("config.json")).unwrap(),
            "portable-config"
        );
        assert_eq!(
            fs::read_to_string(portable.join("patch-engine/cache/engine.zip")).unwrap(),
            "cached-engine"
        );
        assert!(portable.join(PORTABLE_MIGRATION_MARKER).is_file());
        assert!(legacy.join("config.json").is_file());

        fs::write(legacy.join("config.json"), "new-legacy-config").unwrap();
        migrate_legacy_data(&legacy, &portable).unwrap();
        assert_eq!(
            fs::read_to_string(portable.join("config.json")).unwrap(),
            "portable-config"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_powershell_launchers_load_native_security_module() {
        let root = env::temp_dir().join(format!(
            "claude-windows-cn-powershell-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let script = "$ErrorActionPreference='Stop'; (Get-AuthenticodeSignature -FilePath (Join-Path $PSHOME 'powershell.exe')).Status.ToString()";
        let script_path = root.join("security.ps1");
        write_script(&script_path, script).unwrap();
        let results = [
            powershell(script),
            run_script(&script_path),
            run_script_to_log(&script_path, &root.join("security.log")),
        ];
        let resolved = fs::canonicalize(&root).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(env::temp_dir()).unwrap()));
        fs::remove_dir_all(resolved).unwrap();
        for result in results {
            assert_eq!(result.unwrap().trim(), "Valid");
        }
    }
}
