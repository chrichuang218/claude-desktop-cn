use crate::{
    claude,
    operation::{OperationOutcome, OperationState},
    util::{self, ps_path, ps_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ASSET: &str = "claude-windows-cn.exe";
const CHECKSUM: &str = "claude-windows-cn.exe.sha256";
const UPDATE_DIR: &str = "self-update";
const STAGED_PREFIX: &str = "claude-windows-cn-";
const BACKUP: &str = "claude-windows-cn.exe.bak";
static CACHE_LOCK: Mutex<()> = Mutex::new(());

include!("self_update_shared.rs");

/// Startup cleanup: staged downloads no newer than this build are spent, and the
/// rollback copy is no longer needed once the update reported success.
pub fn clean_cache(data: &Path) -> Vec<String> {
    let updated = last_update_result()
        .ok()
        .flatten()
        .is_some_and(|result| result.ok);
    util::remove_entries(data, &data.join(UPDATE_DIR), |name| {
        (updated && name == BACKUP)
            || name
                .strip_prefix(STAGED_PREFIX)
                .and_then(|rest| rest.strip_suffix(".exe"))
                .is_some_and(|version| {
                    claude::compare_versions(version, env!("CARGO_PKG_VERSION"))
                        != std::cmp::Ordering::Greater
                })
    })
}

fn release_api(url: &str, allow_missing: bool) -> Result<Option<Release>, String> {
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; try {{ $response=Invoke-WebRequest -Uri {} -Headers @{{'User-Agent'='claude-windows-cn'; 'Accept'='application/vnd.github+json'}} -UseBasicParsing -TimeoutSec 30; @{{status=[int]$response.StatusCode;content=[string]$response.Content}} | ConvertTo-Json -Compress }} catch {{ $response=$_.Exception.Response; if (-not $response) {{ throw }}; @{{status=[int]$response.StatusCode;content=''}} | ConvertTo-Json -Compress }}",
        ps_quote(url)
    );
    let json = util::powershell(&script)?;
    parse_release_response(&json, allow_missing)
}

fn latest_release() -> Result<Option<ReleaseInfo>, String> {
    let latest = format!("https://github.com/{OWNER_REPO}/releases/latest");
    let redirect_script = format!(
        "$request=[System.Net.HttpWebRequest]::Create({}); $request.AllowAutoRedirect=$false; $request.UserAgent='claude-windows-cn'; try {{ $response=$request.GetResponse() }} catch [System.Net.WebException] {{ $response=$_.Exception.Response; if (-not $response) {{ throw }} }}; try {{ if ([int]$response.StatusCode -eq 404) {{ '' }} else {{ $response.Headers['Location'] }} }} finally {{ $response.Close() }}",
        ps_quote(&latest)
    );
    let tag = util::powershell(&redirect_script)
        .ok()
        .and_then(|redirect| {
            redirect.split_once("/releases/tag/").map(|(_, value)| {
                value
                    .split(['?', '#', '/'])
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
        })
        .filter(|value| !value.is_empty());
    let release = match tag {
        Some(tag) => release_api(
            &format!("https://api.github.com/repos/{OWNER_REPO}/releases/tags/{tag}"),
            false,
        ),
        None => release_api(
            &format!("https://api.github.com/repos/{OWNER_REPO}/releases/latest"),
            true,
        ),
    }
    .map_err(|error| {
        if error.contains("HTTP 403") {
            "GitHub API 限流或拒绝访问。".to_string()
        } else {
            format!("查询助手 Release 失败：{error}")
        }
    })?;
    release.map(validate_release_assets).transpose()
}

pub fn install(operation: &OperationState) -> Result<OperationOutcome, String> {
    operation.step("查询助手 Release");
    let release = latest_release()?.ok_or("暂无可安装的助手发行版本。")?;
    if claude::compare_versions(&release.version, env!("CARGO_PKG_VERSION"))
        != std::cmp::Ordering::Greater
    {
        return Ok(OperationOutcome::done(format!(
            "助手已是最新版本 {}。",
            env!("CARGO_PKG_VERSION")
        )));
    }
    let data = util::data_dir()?;
    let work = data.join(UPDATE_DIR);
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    for error in
        util::prune_versioned_downloads(&data, &work, STAGED_PREFIX, ".exe", Some(&release.version))
    {
        operation.log(error);
    }
    let executable = work.join(format!("{STAGED_PREFIX}{}.exe", release.version));
    let checksum = work.join(CHECKSUM);
    operation.step("下载助手程序及 SHA256 摘要");
    util::download(&release.asset_url, &executable)?;
    util::download(&release.checksum_url, &checksum)?;
    let actual = verify_checksum(&executable, &checksum)?;
    operation.log(format!("下载地址：{}；SHA256：{actual}", release.asset_url));
    operation.step("运行新版助手无副作用自检");
    run_self_test(&executable)?;
    let target = env::current_exe().map_err(|error| error.to_string())?;
    if target.file_name().and_then(|name| name.to_str()) != Some(ASSET) {
        return Err("当前程序名称不是正式助手 exe，不能原位自更新。".into());
    }
    operation.step("准备替换助手程序");
    let script = work.join("apply-update.ps1");
    let backup = work.join(BACKUP);
    let result = work.join("apply-result.json");
    if result.exists() {
        fs::remove_file(&result).map_err(|error| error.to_string())?;
    }
    let content = updater_script(
        std::process::id(),
        &target,
        &executable,
        &backup,
        &result,
        &release.version,
    );
    util::write_script(&script, &content)?;
    let elevated = target.starts_with(
        env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_default(),
    );
    let args = format!(
        "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        script.display()
    );
    let launch = format!(
        "Start-Process -FilePath 'powershell.exe' -ArgumentList {} {} -WindowStyle Hidden | Out-Null",
        ps_quote(args), if elevated { "-Verb RunAs" } else { "" }
    );
    util::powershell(&launch).map_err(|error| format!("更新替换进程未启动：{error}"))?;
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(2));
        std::process::exit(0);
    });
    Ok(OperationOutcome::done(
        "新版助手已通过校验和自检，替换进程已启动；程序退出后执行更新。",
    ))
}

fn updater_script(
    pid: u32,
    target: &Path,
    staged: &Path,
    backup: &Path,
    result: &Path,
    version: &str,
) -> String {
    format!(
        r#"$ErrorActionPreference='Stop'
        $target={target}; $staged={staged}; $backup={backup}; $result={result}
        try {{
            Wait-Process -Id {pid} -Timeout 60 -ErrorAction SilentlyContinue
            if (-not (Test-Path -LiteralPath $staged)) {{ throw '新版程序不存在。' }}
            Copy-Item -LiteralPath $target -Destination $backup -Force -ErrorAction Stop
            Copy-Item -LiteralPath $staged -Destination $target -Force -ErrorAction Stop
            $process=Start-Process -FilePath $target -PassThru -ErrorAction Stop
            if (-not $process) {{ throw '新版助手未启动。' }}
            # The staged copy is now installed; only the rollback copy is still useful.
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            # Version records are informational; never roll back a working exe over them.
            try {{
                $manifest=Join-Path (Split-Path -Parent $target) 'install.json'
                if (Test-Path -LiteralPath $manifest) {{
                    $record=Get-Content -LiteralPath $manifest -Raw -Encoding UTF8 | ConvertFrom-Json
                    $record.version={version}
                    [IO.File]::WriteAllText($manifest, ($record | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))
                }}
                foreach ($key in 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ClaudeWindowsCN','HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ClaudeWindowsCN') {{
                    if ((Test-Path -LiteralPath $key) -and (Get-ItemProperty -Path $key).DisplayIcon -eq $target) {{
                        Set-ItemProperty -Path $key -Name DisplayVersion -Value {version} -ErrorAction SilentlyContinue
                    }}
                }}
            }} catch {{ }}
            @{{ ok=$true; message='更新成功。' }} | ConvertTo-Json -Compress | Set-Content -LiteralPath $result -Encoding UTF8
        }} catch {{
            $reason=$_.Exception.Message
            if (Test-Path -LiteralPath $backup) {{
                try {{ Copy-Item -LiteralPath $backup -Destination $target -Force -ErrorAction Stop }}
                catch {{ $reason += '; 回退失败：' + $_.Exception.Message }}
            }}
            @{{ ok=$false; message=$reason }} | ConvertTo-Json -Compress | Set-Content -LiteralPath $result -Encoding UTF8
        }}
    "#,
        pid = pid,
        target = ps_path(target),
        staged = ps_path(staged),
        backup = ps_path(backup),
        result = ps_path(result),
        version = ps_quote(version),
    )
}

pub fn self_test() -> bool {
    env!("CARGO_PKG_NAME") == "claude-windows-cn"
        && env::current_exe().ok().is_some_and(|path| path.is_file())
        && std::panic::catch_unwind(|| {
            let _ = crate::app_context();
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::repository_tests::fake_release;
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = env::temp_dir().join(format!(
                "claude-cn-self-update-test-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for name in [
                "new.exe",
                "claude-windows-cn.exe",
                "checksum.sha256",
                "old.bak",
                "result.json",
                "update.ps1",
                "install.json",
            ] {
                let _ = fs::remove_file(self.path(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn release_requires_exe_and_checksum_assets() {
        assert!(validate_release_assets(fake_release(&[ASSET, CHECKSUM])).is_ok());
        assert!(validate_release_assets(fake_release(&[CHECKSUM]))
            .unwrap_err()
            .contains(ASSET));
        assert!(validate_release_assets(fake_release(&[ASSET]))
            .unwrap_err()
            .contains(CHECKSUM));
    }

    #[test]
    fn checksum_requires_own_exe_name() {
        let hash = "a".repeat(64);
        assert!(parse_checksum(&format!("{hash}  {ASSET}")).is_ok());
        assert!(parse_checksum(&format!("{hash}  other.exe")).is_err());
        assert!(parse_checksum("broken").is_err());
    }

    #[test]
    fn checksum_mismatch_and_invalid_exe_fail_before_replacement() {
        let fixture = Fixture::new();
        let exe = fixture.path("new.exe");
        let checksum = fixture.path("checksum.sha256");
        fs::write(&exe, b"not a Windows executable").unwrap();
        fs::write(&checksum, format!("{}  {ASSET}", "a".repeat(64))).unwrap();
        assert!(verify_checksum(&exe, &checksum)
            .unwrap_err()
            .contains("摘要不匹配"));
        assert!(run_self_test(&exe).is_err());
    }

    #[test]
    fn rollback_script_restores_even_when_target_exists() {
        let script = updater_script(
            42,
            Path::new("C:\\tool.exe"),
            Path::new("C:\\next.exe"),
            Path::new("C:\\old.exe"),
            Path::new("C:\\result.json"),
            "9.9.9",
        );
        assert!(script.contains("Copy-Item -LiteralPath $backup -Destination $target -Force"));
    }

    #[cfg(windows)]
    #[test]
    fn failed_local_replacement_restores_old_exe() {
        let fixture = Fixture::new();
        let target = fixture.path("claude-windows-cn.exe");
        let staged = fixture.path("new.exe");
        let backup = fixture.path("old.bak");
        let result = fixture.path("result.json");
        let script = fixture.path("update.ps1");
        fs::write(&target, b"old executable bytes").unwrap();
        fs::write(&staged, b"invalid new executable bytes").unwrap();
        // A nonexistent PID lets the script enter the replacement branch without waiting.
        util::write_script(
            &script,
            &updater_script(2_000_000_000, &target, &staged, &backup, &result, "9.9.9"),
        )
        .unwrap();
        util::run_script(&script).unwrap();
        let recorded: UpdateResult = serde_json::from_str(
            fs::read_to_string(&result)
                .unwrap()
                .trim_start_matches('\u{feff}'),
        )
        .unwrap();
        assert!(!recorded.ok, "invalid executable must fail to start");
        assert!(!recorded.message.is_empty());
        assert_eq!(fs::read(&backup).unwrap(), b"old executable bytes");
        assert_eq!(fs::read(&target).unwrap(), b"old executable bytes");
    }

    #[cfg(windows)]
    #[test]
    fn successful_replacement_records_new_version() {
        let fixture = Fixture::new();
        let target = fixture.path("claude-windows-cn.exe");
        let staged = fixture.path("new.exe");
        let manifest = fixture.path("install.json");
        let script = fixture.path("update.ps1");
        // Any real executable that exits on its own stands in for the new build.
        let system = PathBuf::from(env::var_os("WINDIR").unwrap()).join("System32");
        fs::copy(system.join("whoami.exe"), &target).unwrap();
        fs::copy(system.join("whoami.exe"), &staged).unwrap();
        fs::write(
            &manifest,
            r#"{"product":"claude-windows-cn","mode":"portable","installPath":"x","version":"0.0.1"}"#,
        )
        .unwrap();
        util::write_script(
            &script,
            &updater_script(
                2_000_000_000,
                &target,
                &staged,
                &fixture.path("old.bak"),
                &fixture.path("result.json"),
                "9.9.9",
            ),
        )
        .unwrap();
        util::run_script(&script).unwrap();
        // Read without BOM stripping, exactly as assistant::read_manifest does.
        let record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
        assert_eq!(record["version"], "9.9.9");
        assert_eq!(record["installPath"], "x");
        assert!(!staged.exists(), "installed staged copy should be removed");
        assert!(fixture.path("old.bak").is_file());
    }
}
