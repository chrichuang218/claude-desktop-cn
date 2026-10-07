pub use crate::util::compare_versions;
use crate::{
    operation::{OperationOutcome, OperationState},
    patch,
    util::{self, ps_path, ps_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const FAMILY: &str = "Claude_pzs8sxrjxfjjc";
const APP_ID: &str = "Claude_pzs8sxrjxfjjc!Claude";
const MSIX_URL: &str = "https://api.anthropic.com/api/desktop/win32/x64/msix/latest/redirect";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ClaudePackage {
    pub package_full_name: String,
    pub package_family_name: String,
    pub version: String,
    pub architecture: String,
    pub install_location: String,
}

impl ClaudePackage {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.install_location)
    }

    pub fn resources(&self) -> PathBuf {
        self.root().join("app").join("resources")
    }

    pub fn exe(&self) -> PathBuf {
        self.root().join("app").join("Claude.exe")
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeStatus {
    pub installed: bool,
    pub version: String,
    pub package_full_name: String,
    pub install_path: String,
    pub applied_mode: Option<String>,
    pub backup_ready: bool,
    pub external_localization: bool,
    pub patch_recovery_required: bool,
    pub message: String,
    pub update_available: Option<bool>,
    pub latest_version: Option<String>,
    pub update_check_error: Option<String>,
    pub last_update_check: Option<String>,
}

pub fn query_package() -> Result<Option<ClaudePackage>, String> {
    let script = format!(
        "$items = @(Get-AppxPackage -Name Claude | Where-Object {{ $_.PackageFamilyName -eq '{}' -and $_.Architecture -eq 'X64' }}); if ($items.Count -gt 1) {{ throw '当前用户有多个 Claude x64 包，无法确定目标。' }}; if ($items.Count -eq 1) {{ $items[0] | Select-Object PackageFullName,PackageFamilyName,Version,InstallLocation,@{{Name='Architecture';Expression={{$_.Architecture.ToString()}}}} | ConvertTo-Json -Compress -Depth 3 }}",
        FAMILY
    );
    let raw = util::powershell(&script)?;
    if raw.is_empty() {
        return Ok(None);
    }
    let package: ClaudePackage =
        serde_json::from_str(&raw).map_err(|error| format!("Claude Appx 状态无法解析：{error}"))?;
    validate_package(&package)?;
    Ok(Some(package))
}

fn validate_package(package: &ClaudePackage) -> Result<(), String> {
    if package.package_family_name != FAMILY || !package.architecture.eq_ignore_ascii_case("X64") {
        return Err("检测到的包不是官方 Claude Desktop x64。".into());
    }
    if !package.package_full_name.starts_with("Claude_")
        || !package.package_full_name.ends_with("_x64__pzs8sxrjxfjjc")
    {
        return Err("Claude Appx 包标识不符合预期。".into());
    }
    let root = package.root();
    let windows_apps = Path::new(r"C:\Program Files\WindowsApps");
    if root.parent() != Some(windows_apps)
        || root.file_name().and_then(|value| value.to_str())
            != Some(package.package_full_name.as_str())
        || !package.exe().is_file()
    {
        return Err("Claude Appx 安装路径无效，已停止操作。".into());
    }
    Ok(())
}

pub fn status() -> Result<ClaudeStatus, String> {
    let Some(package) = query_package()? else {
        let cached = crate::self_update::cached_claude_update()?;
        return Ok(ClaudeStatus {
            installed: false,
            version: String::new(),
            package_full_name: String::new(),
            install_path: String::new(),
            applied_mode: None,
            backup_ready: false,
            external_localization: false,
            patch_recovery_required: false,
            message: "未安装官方 Claude Desktop。".into(),
            update_available: None,
            latest_version: cached
                .as_ref()
                .and_then(|value| value.latest_version.clone()),
            update_check_error: cached.as_ref().and_then(|value| value.error.clone()),
            last_update_check: cached.map(|value| value.checked_at),
        });
    };
    let state = patch::state(&package)?;
    let cached = crate::self_update::cached_claude_update()?;
    Ok(ClaudeStatus {
        installed: true,
        version: package.version.clone(),
        package_full_name: package.package_full_name.clone(),
        install_path: package.install_location.clone(),
        applied_mode: state.applied_mode,
        backup_ready: state.backup_ready,
        external_localization: state.external_localization,
        patch_recovery_required: state.recovery_required,
        message: if state.recovery_required {
            patch::recovery_message(&package)?
        } else if state.external_localization {
            "检测到其他来源的中文资源或备份，助手不会接管。".into()
        } else {
            "已检测到官方 Claude Desktop。".into()
        },
        update_available: current_update_available(&package.version, cached.as_ref()),
        latest_version: cached
            .as_ref()
            .and_then(|value| value.latest_version.clone()),
        update_check_error: cached.as_ref().and_then(|value| value.error.clone()),
        last_update_check: cached.map(|value| value.checked_at),
    })
}

fn current_update_available(
    installed: &str,
    cached: Option<&crate::self_update::CachedUpdate>,
) -> Option<bool> {
    let cached = cached.filter(|value| value.error.is_none())?;
    let latest = cached.latest_version.as_deref()?;
    // Appx adds a fourth version component; compare against the current package,
    // never the availability recorded before an installation or update.
    let valid = |version: &str| {
        !version.is_empty() && version.split('.').all(|part| part.parse::<u64>().is_ok())
    };
    if !valid(installed) || !valid(latest) {
        return None;
    }
    Some(compare_versions(latest, installed) == Ordering::Greater)
}

pub fn launch() -> Result<(), String> {
    let package = query_package()?.ok_or_else(|| "未安装官方 Claude Desktop。".to_string())?;
    let mut command = Command::new("explorer.exe");
    util::hide_window(&mut command);
    command
        .arg(format!(r"shell:AppsFolder\{APP_ID}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("启动 Claude Desktop 失败：{error}"))?;
    for _ in 0..20 {
        if desktop_running(&package)? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err("已发送 Claude Desktop 启动请求，但未检测到该 Appx 的进程。".into())
}

fn desktop_running(package: &ClaudePackage) -> Result<bool, String> {
    let script = desktop_running_script(package);
    Ok(util::powershell(&script)?.eq_ignore_ascii_case("true"))
}

// A Desktop parent can exit its children between the CIM snapshot and method call.
// Only accept typed NotFound after confirming that the original instance is gone.
const PROCESS_CIM_METHOD: &str = r#"
function Invoke-AssistantProcessMethod {
    param($Process, [string]$MethodName, [hashtable]$Arguments=@{})
    try {
        $result=Invoke-CimMethod -InputObject $Process -MethodName $MethodName -Arguments $Arguments -ErrorAction Stop
    } catch [Microsoft.Management.Infrastructure.CimException] {
        if ($_.Exception.NativeErrorCode -ne [Microsoft.Management.Infrastructure.NativeErrorCode]::NotFound) { throw }
        $live=Get-CimInstance Win32_Process -Filter ('ProcessId=' + $Process.ProcessId) -ErrorAction Stop
        if ($live -and $live.CreationDate -eq $Process.CreationDate) { throw }
        return $null
    }
    if ($result.ReturnValue -ne 0) {
        throw ('Claude 进程方法 ' + $MethodName + ' 失败：' + $Process.ProcessId + '，错误码：' + $result.ReturnValue)
    }
    return $result
}
"#;

fn desktop_running_script(package: &ClaudePackage) -> String {
    format!(
        "{PROCESS_CIM_METHOD}\n$session=(Get-Process -Id $PID).SessionId; $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $found=$false; foreach ($item in @(Get-CimInstance Win32_Process -Filter \"Name='Claude.exe'\" -ErrorAction Stop)) {{ if ($item.SessionId -ne $session -or $item.ExecutablePath -ne {}) {{ continue }}; $owner=Invoke-AssistantProcessMethod -Process $item -MethodName GetOwnerSid; if ($null -eq $owner) {{ continue }}; if ($owner.ReturnValue -ne 0) {{ throw '无法确认 Claude Desktop 进程所属用户。' }}; if ($owner.Sid -eq $sid) {{ $found=$true }} }}; $found",
        ps_path(&package.exe()),
    )
}

fn close_desktop_for_update(
    package: &ClaudePackage,
    operation: &OperationState,
) -> Result<bool, String> {
    validate_package(package)?;
    let (sid, session) = util::caller_identity()?;
    let restore_service = util::powershell(&format!(
        "$service=Get-CimInstance Win32_Service -Filter \"Name='CoworkVMService'\" -ErrorAction Stop; [bool]($service -and $service.State -eq 'Running' -and $service.PathName.Trim().Trim('\"') -eq {})",
        ps_path(&package.resources().join("cowork-svc.exe"))
    ))?.eq_ignore_ascii_case("true");
    let result = run_desktop_script(&close_desktop_script(package, &sid, session), operation);
    if let Err(ref error) = result {
        if restore_service {
            if let Err(restore_error) = restore_desktop_service(package, operation) {
                return Err(format!("{error}；恢复 Cowork 服务失败：{restore_error}"));
            }
        }
    }
    result.map(|()| restore_service)
}

fn restore_desktop_service(
    package: &ClaudePackage,
    operation: &OperationState,
) -> Result<(), String> {
    // Registration may have switched packages even when a later check failed.
    // Only restore the original service while the original user's package is unchanged.
    if query_package()?
        .is_some_and(|current| current.package_full_name == package.package_full_name)
    {
        operation.log("操作失败，恢复原先运行的 CoworkVMService");
        run_desktop_script(
            &format!(
                "{}\nRestore-AssistantCoworkService {}",
                patch::RESTORE_COWORK_SERVICE_FUNCTION,
                ps_path(&package.resources().join("cowork-svc.exe"))
            ),
            operation,
        )?;
    }
    Ok(())
}

fn run_desktop_script(contents: &str, operation: &OperationState) -> Result<(), String> {
    let work = util::data_dir()?.join("close-desktop").join(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
            .to_string(),
    );
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let script = work.join("close-desktop.ps1");
    util::write_script(&script, contents)?;
    // Reuse the updater's service -> terminate -> wait sequence through the existing
    // elevated helper. Appx registration stays in the original user's process.
    let elevated = util::powershell(
        "([System.Security.Principal.WindowsPrincipal][System.Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)",
    )?.eq_ignore_ascii_case("true");
    let result = if elevated {
        util::run_script(&script).map(|log| operation.log(log))
    } else {
        util::run_elevated_script_with_logs(&script, &work, |chunk| operation.log(chunk))
            .map(|_| ())
    };
    result.map_err(|error| {
        operation.log(format!("关闭 Claude Desktop 完整错误：{error}"));
        "关闭 Claude Desktop 失败，详细原因见执行日志。".into()
    })
}

pub(crate) fn close_desktop_script(package: &ClaudePackage, sid: &str, session: u32) -> String {
    let job_helper = include_str!("claude_job.cs");
    format!(
        r#"$ErrorActionPreference='Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$target={path}
$callerSid={sid}
$session={session}
{PROCESS_CIM_METHOD}
$registered=@(Get-AppxPackage -User $callerSid -Name Claude -ErrorAction Stop | Where-Object {{ $_.PackageFullName -eq {full} -and $_.InstallLocation -eq {root} }})
if ($registered.Count -ne 1) {{ throw '提权后无法确认原调用用户的 Claude Appx 包。' }}
if (-not ('ClaudeDesktopJob' -as [type])) {{
    Add-Type -TypeDefinition @'
{job_helper}
'@
}}
function Get-TargetClaudeProcesses {{
    foreach ($item in @(Get-CimInstance Win32_Process -Filter "Name='Claude.exe'" -ErrorAction Stop)) {{
        if ($item.SessionId -ne $session -or $item.ExecutablePath -ne $target) {{ continue }}
        $owner=Invoke-AssistantProcessMethod -Process $item -MethodName GetOwnerSid
        if ($null -eq $owner) {{ continue }}
        if ($owner.ReturnValue -ne 0) {{ throw ('无法确认 Claude Desktop 进程所属用户：' + $item.ProcessId) }}
        if ($owner.Sid -eq $callerSid) {{ $item }}
    }}
}}
# Same sequence as the original updater: stop Cowork, terminate Desktop, verify exit.
$service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
if ($service -and $service.State -ne 'Stopped') {{
    $servicePath=$service.PathName.Trim().Trim('"')
    if ($servicePath -ne {service_path}) {{ throw 'CoworkVMService 不属于当前 Claude 包，已停止操作。' }}
    Write-Output '停止当前官方包的 CoworkVMService'
    Stop-Service -Name CoworkVMService -ErrorAction Stop
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    do {{
        $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
        if (-not $service -or $service.State -eq 'Stopped') {{ break }}
        if ([DateTime]::UtcNow -ge $deadline) {{ throw '等待 CoworkVMService 停止超时。' }}
        Start-Sleep -Milliseconds 250
    }} while ($true)
}}
Write-Output '结束当前用户、当前会话的官方 Claude Desktop 进程'
# Close package-job descendants first, including orphans without package identity.
# Keep the exact-executable fallback for Desktop versions without a container job.
[ClaudeDesktopJob]::Close({full}, $callerSid, [uint32]$session)
foreach ($item in @(Get-TargetClaudeProcesses)) {{
    # Refresh each CIM instance to avoid acting on an exited/reused process ID.
    $current=Get-CimInstance Win32_Process -Filter ('ProcessId=' + $item.ProcessId) -ErrorAction Stop
    # Exiting children can briefly remain in CIM with no executable path.
    # Wait for that instance to disappear; never terminate an unverifiable process.
    $identityDeadline=[DateTime]::UtcNow.AddSeconds(2)
    while ($current -and $current.CreationDate -eq $item.CreationDate -and -not $current.ExecutablePath -and [DateTime]::UtcNow -lt $identityDeadline) {{
        Start-Sleep -Milliseconds 100
        $current=Get-CimInstance Win32_Process -Filter ('ProcessId=' + $item.ProcessId) -ErrorAction Stop
    }}
    if (-not $current -or $current.CreationDate -ne $item.CreationDate) {{ continue }}
    if ($current.ExecutablePath -ne $target -or $current.SessionId -ne $session) {{ throw ('Claude 进程身份无法确认：PID=' + $item.ProcessId + '；预期路径=' + $target + '；实际路径=' + $current.ExecutablePath + '；预期会话=' + $session + '；实际会话=' + $current.SessionId) }}
    $owner=Invoke-AssistantProcessMethod -Process $current -MethodName GetOwnerSid
    if ($null -eq $owner) {{ continue }}
    if ($owner.ReturnValue -ne 0 -or $owner.Sid -ne $callerSid) {{ throw 'Claude 进程用户已变化，请重试。' }}
    Invoke-AssistantProcessMethod -Process $current -MethodName Terminate -Arguments @{{Reason=[uint32]1}} | Out-Null
}}
$deadline=[DateTime]::UtcNow.AddSeconds(5)
do {{
    $remaining=@(Get-TargetClaudeProcesses)
    if ($remaining.Count -eq 0) {{ Write-Output '官方 Claude Desktop 已退出'; return }}
    Start-Sleep -Milliseconds 250
}} while ([DateTime]::UtcNow -lt $deadline)
throw ('Claude Desktop 仍在运行，进程 ID：' + (($remaining | ForEach-Object ProcessId) -join ', '))
"#,
        path = ps_path(&package.exe()),
        sid = ps_quote(sid),
        full = ps_quote(&package.package_full_name),
        root = ps_path(&package.root()),
        service_path = ps_path(&package.resources().join("cowork-svc.exe")),
    )
}

struct MsixMetadata {
    version: String,
    url: String,
}

fn official_metadata() -> Result<MsixMetadata, String> {
    let script = format!(
        "$request=[System.Net.HttpWebRequest]::Create({}); $request.AllowAutoRedirect=$false; $request.Timeout=60000; try {{ $response=$request.GetResponse() }} catch [System.Net.WebException] {{ $response=$_.Exception.Response; if (-not $response) {{ throw }} }}; try {{ if ([int]$response.StatusCode -lt 300 -or [int]$response.StatusCode -ge 400) {{ throw ('官方 MSIX 接口返回 HTTP ' + [int]$response.StatusCode) }}; $response.Headers['Location'] }} finally {{ $response.Close() }}",
        ps_quote(MSIX_URL)
    );
    let url = util::powershell(&script)?;
    let url = if url.starts_with('/') {
        format!("https://downloads.claude.ai{url}")
    } else {
        url
    };
    if !url.starts_with("https://downloads.claude.ai/") {
        return Err("官方 MSIX 跳转地址不符合预期。".into());
    }
    let version = url
        .split_once("/releases/win32/x64/")
        .and_then(|(_, tail)| tail.split('/').next())
        .filter(|value| {
            !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        })
        .ok_or_else(|| "官方 MSIX 地址没有可识别版本。".to_string())?;
    Ok(MsixMetadata {
        version: version.into(),
        url,
    })
}

pub fn check_update(operation: &OperationState) -> Result<OperationOutcome, String> {
    operation.step("查询官方 Claude Desktop 版本");
    let result = (|| {
        let metadata = official_metadata()?;
        let current = query_package()?;
        let available = current.as_ref().is_none_or(|package| {
            compare_versions(&metadata.version, &package.version) == Ordering::Greater
        });
        operation.log(format!(
            "当前版本：{}；官方版本：{}",
            current
                .as_ref()
                .map_or("未安装", |package| &package.version),
            metadata.version
        ));
        Ok((available, metadata.version))
    })();
    crate::self_update::save_claude_check(&result)?;
    let (available, version) = result?;
    Ok(OperationOutcome::update(
        if available {
            format!("发现 Claude Desktop 版本 {version}。")
        } else {
            format!("Claude Desktop 已是最新版本 {version}。")
        },
        available,
        version,
    ))
}

pub fn daily_update_info() -> Result<(bool, String), String> {
    let metadata = official_metadata()?;
    let current = query_package()?;
    let available = current.as_ref().is_none_or(|package| {
        compare_versions(&metadata.version, &package.version) == Ordering::Greater
    });
    Ok((available, metadata.version))
}

pub fn install(operation: &OperationState, only_update: bool) -> Result<OperationOutcome, String> {
    operation.step("获取官方 Claude Desktop 安装包信息");
    let metadata = official_metadata()?;
    let current = query_package()?;
    if let Some(ref package) = current {
        if only_update && compare_versions(&metadata.version, &package.version) != Ordering::Greater
        {
            operation.step("核查当前用户的应用注册");
            verify_registration(package)?;
            return Ok(OperationOutcome::done(format!(
                "Claude Desktop 已是最新版本 {}。",
                package.version
            )));
        }
        if !only_update {
            operation.step("核查当前用户的应用注册");
            verify_registration(package)?;
            return Ok(OperationOutcome::done(format!(
                "当前用户已安装 Claude Desktop {}。",
                package.version
            )));
        }
    } else if only_update {
        return Err("当前用户未安装 Claude Desktop，请先安装。".into());
    }
    let download_dir = util::data_dir()?.join("official-msix");
    fs::create_dir_all(&download_dir).map_err(|error| error.to_string())?;
    let msix = download_dir.join(format!("Claude-{}.msix", metadata.version));
    let reuse = if msix.is_file() {
        operation.step("校验已下载的官方 MSIX 缓存");
        match util::powershell(&cached_msix_check_script(&msix, &metadata.version)) {
            Ok(result) if result == "VALID" => {
                operation.log("已验证本地 MSIX 签名、发布者、包身份与目标版本，复用下载缓存。");
                true
            }
            result => {
                let reason = result.err().unwrap_or_else(|| "缓存校验结果无效。".into());
                operation.log(format!("MSIX 缓存不可复用，将重新下载：{reason}"));
                false
            }
        }
    } else {
        false
    };
    if !reuse {
        operation.step("下载官方 x64 MSIX");
        download_msix(&metadata.url, &msix, operation)?;
    }
    operation.step("校验官方 MSIX");
    operation.log(format!(
        "官方版本：{}；来源：{}；SHA256：{}",
        metadata.version,
        metadata
            .url
            .split(['?', '#'])
            .next()
            .unwrap_or(&metadata.url),
        util::sha256(&msix)?
    ));
    // PowerShell can report UnknownError for an MSIX container. Appx registration validates the
    // package signature; the installed Claude.exe is checked below before reporting success.
    let signature = util::powershell(&format!(
        "$sig=Get-AuthenticodeSignature -FilePath {}; if ($sig.Status -eq 'Valid' -and $sig.SignerCertificate.Subject -notmatch 'Anthropic') {{ throw 'MSIX 发布者不是 Anthropic。' }}; if ($sig.Status -in @('NotSigned','HashMismatch','NotTrusted')) {{ throw ('MSIX 签名无效：' + $sig.Status) }}; $sig.Status.ToString()",
        ps_path(&msix)
    ))?;
    operation.log(format!(
        "MSIX Authenticode 预检查：{signature}；Appx 注册负责校验包签名，安装后会复核 Claude.exe 发布者。"
    ));
    let restore_service = if let Some(ref package) = current {
        operation.step("关闭当前官方 Claude Desktop");
        close_desktop_for_update(package, operation)?
    } else {
        false
    };
    let result = (|| {
        operation.step("注册官方 Claude Desktop Appx");
        let script = msix_install_command(&msix);
        if let Err(error) = util::powershell(&script) {
            operation.log(format!("Add-AppxPackage 完整错误：{error}"));
            return Err(appx_install_error_summary(&error).into());
        }
        operation.step("等待当前用户的 Claude Appx 注册完成");
        let package = wait_for_installed_version(&metadata.version, query_package, || {
            std::thread::sleep(Duration::from_secs(1));
        })?;
        operation.step("核查当前用户的应用注册");
        verify_registration(&package)?;
        operation.step("校验新安装的官方 Claude.exe 签名");
        verify_official_executable(&package)?;
        Ok(OperationOutcome::done(format!(
            "Claude Desktop {} 已安装并通过注册检查。",
            package.version
        )))
    })();
    if let Err(error) = result {
        if restore_service {
            if let Some(ref package) = current {
                if let Err(restore_error) = restore_desktop_service(package, operation) {
                    return Err(format!("{error}；恢复 Cowork 服务失败：{restore_error}"));
                }
            }
        }
        return Err(error);
    }
    result
}

fn appx_install_error_summary(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("0x80073d02") || lower.contains("0x8007001f") {
        "Windows 仍报告 Claude 包被占用，请关闭相关任务后重试并查看执行日志；已校验的下载包会保留。"
    } else {
        "Appx 注册失败，详细原因见执行日志。"
    }
}

// Same package-scoped shutdown flags as the original updater; registration
// remains in the caller's user context and never removes the installed package.
fn msix_install_command(msix: &Path) -> String {
    format!(
        "Add-AppxPackage -Path {} -ForceApplicationShutdown -ForceUpdateFromAnyVersion -ErrorAction Stop",
        ps_path(msix),
    )
}

fn cached_msix_check_script(path: &Path, version: &str) -> String {
    format!(
        r#"
$path={path}
$sig=Get-AuthenticodeSignature -FilePath $path
if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'Anthropic') {{ throw '缓存 MSIX 签名或发布者无效。' }}
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zip=[System.IO.Compression.ZipFile]::OpenRead($path)
try {{
    $entry=$zip.GetEntry('AppxManifest.xml')
    if ($null -eq $entry) {{ throw '缓存 MSIX 缺少 AppxManifest.xml。' }}
    $reader=New-Object System.IO.StreamReader($entry.Open())
    try {{ [xml]$manifest=$reader.ReadToEnd() }} finally {{ $reader.Dispose() }}
    $identity=$manifest.Package.Identity
    $parts=@({version}.Split('.'))
    while ($parts.Count -lt 4) {{ $parts+= '0' }}
    $expected=[version]($parts -join '.')
    if ($identity.Name -ne 'Claude' -or $identity.ProcessorArchitecture -ne 'x64' -or [version]$identity.Version -ne $expected) {{ throw '缓存 MSIX 包身份、架构或版本不符。' }}
    'VALID'
}} finally {{ $zip.Dispose() }}
"#,
        path = ps_path(path),
        version = ps_quote(version)
    )
}

fn download_msix(url: &str, destination: &Path, operation: &OperationState) -> Result<(), String> {
    if !url.starts_with("https://downloads.claude.ai/") {
        return Err("官方 MSIX 下载地址不符合预期。".into());
    }
    operation.log("建立官方下载连接");
    // Async timeout covers the entire transfer; the blocking timeout bounds each
    // send/read wait, including the first body byte after response headers.
    // Do not set async read_timeout here: reqwest 0.13's blocking body reader
    // polls that timer outside Tokio and panics on the first body read.
    let builder: reqwest::blocking::ClientBuilder = reqwest::Client::builder()
        .timeout(Duration::from_secs(20 * 60))
        .into();
    let client = builder
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| format!("创建官方下载请求失败：{error}"))?;
    operation.log("请求官方 MSIX 下载流");
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("下载官方 MSIX 失败：{}", error.without_url()))?
        .error_for_status()
        .map_err(|error| format!("官方 MSIX 请求失败：{}", error.without_url()))?;
    let total = response.content_length();
    operation.log("已收到下载响应，开始接收 MSIX 数据");
    save_msix_stream(&mut response, total, destination, operation)
}

fn save_msix_stream<R: Read>(
    source: &mut R,
    total: Option<u64>,
    destination: &Path,
    operation: &OperationState,
) -> Result<(), String> {
    let partial = destination.with_extension("msix.partial");
    let result = (|| {
        let mut file = fs::File::create(&partial)
            .map_err(|error| format!("创建 MSIX 临时文件失败：{error}"))?;
        copy_download(source, &mut file, total, |done, total| {
            operation.download_progress(done, total);
        })?;
        file.sync_all()
            .map_err(|error| format!("写入 MSIX 临时文件失败：{error}"))?;
        drop(file);
        util::replace_file(&partial, destination)
            .map_err(|error| format!("保存官方 MSIX 失败：{error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn copy_download<R: Read, W: Write>(
    source: &mut R,
    destination: &mut W,
    total: Option<u64>,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let mut downloaded = 0u64;
    let mut last_report = Instant::now();
    progress(0, total);
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut
                || error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<reqwest::Error>())
                    .is_some_and(reqwest::Error::is_timeout)
            {
                "读取官方 MSIX 下载流超时（单次读取上限 60 秒，整个下载上限 20 分钟）。".to_string()
            } else {
                format!("读取官方 MSIX 下载流失败（{:?}）。", error.kind())
            }
        })?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .map_err(|error| format!("写入官方 MSIX 失败：{error}"))?;
        downloaded += count as u64;
        if last_report.elapsed() >= Duration::from_millis(250) {
            progress(downloaded, total);
            last_report = Instant::now();
        }
    }
    if downloaded == 0 || total.is_some_and(|total| downloaded != total) {
        return Err(format!(
            "官方 MSIX 下载不完整：收到 {downloaded} 字节，预期 {} 字节。",
            total.map_or("未知".into(), |value| value.to_string())
        ));
    }
    progress(downloaded, total);
    Ok(())
}

// Ported from the updater's main.rs: Appx registration may become visible later
// than Add-AppxPackage returns. Keep the last observed state in a bounded retry.
fn wait_for_installed_version(
    latest: &str,
    mut query: impl FnMut() -> Result<Option<ClaudePackage>, String>,
    mut pause: impl FnMut(),
) -> Result<ClaudePackage, String> {
    let mut last = "当前用户未注册 Claude Desktop".to_string();
    for attempt in 0..30 {
        match query() {
            Ok(Some(package)) if compare_versions(&package.version, latest) != Ordering::Less => {
                return Ok(package);
            }
            Ok(Some(package)) => last = format!("当前版本为 {}", package.version),
            Ok(None) => last = "当前用户未注册 Claude Desktop".into(),
            Err(error) => last = error,
        }
        if attempt < 29 {
            pause();
        }
    }
    Err(format!("等待 Claude Desktop {latest} 注册超时：{last}。"))
}

fn registration_check_script(manifest: &Path) -> String {
    // Reuse the old updater's five XML element/attribute checks and its actual
    // AppID + URL Protocol probes, using Windows' XML parser instead of a new crate.
    format!(
        r#"
[xml]$xml=Get-Content -LiteralPath {manifest} -Raw -ErrorAction Stop
$startup=@($xml.SelectNodes("//*[local-name()='StartupTask']") | Where-Object {{ $_.TaskId -eq 'ClaudeStartup' }}).Count -gt 0
$protocol=@($xml.SelectNodes("//*[local-name()='Protocol']") | Where-Object {{ $_.Name -eq 'claude' }}).Count -gt 0
$service=@($xml.SelectNodes("//*[local-name()='Service']") | Where-Object {{ $_.Name -eq 'CoworkVMService' }}).Count -gt 0
$firewall=@($xml.SelectNodes("//*[local-name()='FirewallRules']") | ForEach-Object {{ $_.Executable.Replace('/','\') }})
$app=@(Get-StartApps | Where-Object {{ $_.AppID -eq '{APP_ID}' }}).Count -gt 0
$key=Get-Item -LiteralPath 'Registry::HKEY_CLASSES_ROOT\claude' -ErrorAction SilentlyContinue
$url=$null -ne $key -and $null -ne $key.GetValue('URL Protocol', $null)
[bool]($app -and $url -and $startup -and $protocol -and $service -and ($firewall -contains 'app\Claude.exe') -and ($firewall -contains 'app\resources\cowork-svc.exe'))
"#,
        manifest = ps_path(manifest)
    )
}

fn verify_registration(package: &ClaudePackage) -> Result<(), String> {
    let manifest = package.root().join("AppxManifest.xml");
    let check = registration_check_script(&manifest);
    if !util::powershell(&check)?.eq_ignore_ascii_case("true") {
        let register = format!(
            "Add-AppxPackage -Path {} -Register -DisableDevelopmentMode -ErrorAction Stop",
            ps_path(&manifest)
        );
        util::powershell(&register)?;
        if !util::powershell(&check)?.eq_ignore_ascii_case("true") {
            return Err(
                "Claude AppID、claude URL 协议注册或启动任务/服务/防火墙 XML 声明仍不完整。".into(),
            );
        }
    }
    Ok(())
}

// Existing full localization intentionally changes Claude.exe's ASAR integrity
// field. Registration checks must remain usable for that package; signature
// validation belongs to the newly downloaded official installation only.
fn official_executable_check_script(package: &ClaudePackage) -> String {
    format!(
        "$sig=Get-AuthenticodeSignature -FilePath {}; if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'Anthropic') {{ throw ('官方 Claude.exe 签名校验失败：' + $sig.Status) }}; 'Valid Anthropic'",
        ps_path(&package.exe())
    )
}

fn verify_official_executable(package: &ClaudePackage) -> Result<(), String> {
    let signature = util::powershell(&official_executable_check_script(package))?;
    if signature != "Valid Anthropic" {
        return Err("官方程序签名结果无法确认。".into());
    }
    Ok(())
}

fn desktop_shortcut_script(package: &ClaudePackage, desktop: &Path, icon: &Path) -> String {
    format!(
        r#"$desktop={desktop}
if (-not (Test-Path -LiteralPath $desktop -PathType Container)) {{ throw '无法确定当前用户桌面。' }}
$link=Join-Path $desktop 'Claude Desktop.lnk'
$shell=New-Object -ComObject WScript.Shell
$target=Join-Path $env:WINDIR 'explorer.exe'
$args={args}
$exists=Test-Path -LiteralPath $link
$shortcut=$shell.CreateShortcut($link)
if ($exists -and ($shortcut.TargetPath -ne $target -or $shortcut.Arguments -ne $args)) {{ throw '桌面已有同名快捷方式且目标不是官方 Claude Desktop。' }}
# Keep official artwork outside its versioned WindowsApps directory.
Add-Type -AssemblyName System.Drawing
$source=[Drawing.Image]::FromFile({source})
$sizes=@(16,24,32,48,64,128,256)
$frames=New-Object 'System.Collections.Generic.List[byte[]]'
try {{
    foreach ($size in $sizes) {{
        $bitmap=New-Object Drawing.Bitmap($size,$size)
        $graphics=[Drawing.Graphics]::FromImage($bitmap)
        $png=New-Object IO.MemoryStream
        try {{
            $graphics.InterpolationMode=[Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.PixelOffsetMode=[Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $graphics.DrawImage($source,0,0,$size,$size)
            $bitmap.Save($png,[Drawing.Imaging.ImageFormat]::Png)
            $frames.Add($png.ToArray())
        }} finally {{ $png.Dispose(); $graphics.Dispose(); $bitmap.Dispose() }}
    }}
    $icon={icon}
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($icon)) | Out-Null
    $stream=[IO.File]::Create($icon)
    $writer=New-Object IO.BinaryWriter($stream)
    try {{
        $writer.Write([byte[]]@(0,0,1,0))
        $writer.Write([uint16]$sizes.Count)
        $offset=6+16*$sizes.Count
        for ($i=0; $i -lt $sizes.Count; $i++) {{
            # ICO encodes a 256-pixel dimension as zero.
            $dimension=[byte]($sizes[$i] % 256)
            $writer.Write([byte[]]@($dimension,$dimension,0,0,1,0,32,0))
            $writer.Write([uint32]$frames[$i].Length)
            $writer.Write([uint32]$offset)
            $offset+=$frames[$i].Length
        }}
        foreach ($frame in $frames) {{ $writer.Write($frame) }}
    }} finally {{ $writer.Dispose(); $stream.Dispose() }}
}} finally {{ $source.Dispose() }}
$shortcut.TargetPath=$target
$shortcut.Arguments=$args
$shortcut.IconLocation=$icon+',0'
$shortcut.Description='Claude Desktop'
$shortcut.Save()
if (-not (Test-Path -LiteralPath $link)) {{ throw '快捷方式创建失败。' }}
if ($exists) {{ 'UPDATED' }} else {{ 'CREATED' }}
$link"#,
        desktop = ps_path(desktop),
        args = ps_quote(format!(r"shell:AppsFolder\{APP_ID}")),
        source = ps_path(
            &package
                .root()
                .join("assets/Square150x150Logo.scale-200.png")
        ),
        icon = ps_path(icon),
    )
}

pub fn create_shortcut(operation: &OperationState) -> Result<OperationOutcome, String> {
    let package = query_package()?
        .ok_or_else(|| "未安装官方 Claude Desktop，无法创建快捷方式。".to_string())?;
    operation.step("创建 Claude Desktop 桌面快捷方式");
    let desktop = util::powershell("[Environment]::GetFolderPath('Desktop')")?;
    let icon = util::data_dir()?.join("icons/claude-desktop.ico");
    let output = util::powershell(&desktop_shortcut_script(
        &package,
        Path::new(&desktop),
        &icon,
    ))?;
    operation.log(format!("Claude Desktop 快捷方式：{output}"));
    Ok(OperationOutcome::done(if output.starts_with("UPDATED") {
        "Claude Desktop 桌面快捷方式已更新。"
    } else {
        "Claude Desktop 桌面快捷方式已创建。"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    #[cfg(windows)]
    fn desktop_shortcut_sets_official_icon_repairs_owned_link_and_preserves_foreign_link() {
        let root =
            std::env::temp_dir().join(format!("claude-shortcut-icon-{}", std::process::id()));
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(
            root.join("assets/Square150x150Logo.scale-200.png"),
            include_bytes!("../icons/icon.png"),
        )
        .unwrap();
        let package = ClaudePackage {
            package_full_name: String::new(),
            package_family_name: FAMILY.into(),
            version: "1.0.0.0".into(),
            architecture: "X64".into(),
            install_location: root.to_string_lossy().into(),
        };
        let icon = root.join("cache/claude-desktop.ico");
        let script = desktop_shortcut_script(&package, &root, &icon);
        assert!(util::powershell(&script).unwrap().starts_with("CREATED"));
        let link = root.join("Claude Desktop.lnk");
        let read = format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.IconLocation",
            ps_path(&link)
        );
        assert!(util::powershell(&read)
            .unwrap()
            .starts_with(&icon.to_string_lossy().to_string()));
        let clear = format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.IconLocation=(Join-Path $env:WINDIR 'explorer.exe')+',0'; $s.Save()", ps_path(&link));
        util::powershell(&clear).unwrap();
        assert!(util::powershell(&script).unwrap().starts_with("UPDATED"));
        assert!(util::powershell(&read)
            .unwrap()
            .starts_with(&icon.to_string_lossy().to_string()));
        let foreign = format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.Arguments='foreign'; $s.Save()", ps_path(&link));
        util::powershell(&foreign).unwrap();
        let before = fs::read(&link).unwrap();
        assert!(util::powershell(&script).is_err());
        assert_eq!(before, fs::read(&link).unwrap());
        let bytes = fs::read(&icon).unwrap();
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 7, 0]);
        let mut expected_offset = 6 + 16 * 7;
        for (index, size) in [16u32, 24, 32, 48, 64, 128, 256].iter().enumerate() {
            let entry = &bytes[6 + index * 16..6 + (index + 1) * 16];
            assert_eq!(
                &entry[..8],
                &[(*size % 256) as u8, (*size % 256) as u8, 0, 0, 1, 0, 32, 0]
            );
            let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            assert_eq!(offset, expected_offset);
            let png = &bytes[offset..offset + length];
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), *size);
            assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), *size);
            expected_offset += length;
        }
        assert_eq!(expected_offset, bytes.len());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_availability_uses_current_package_instead_of_cached_boolean() {
        assert_eq!(compare_versions("2.7032.0.0", "2.7032.0"), Ordering::Equal);
        let mut cached = crate::self_update::CachedUpdate {
            checked_at: "1".into(),
            update_available: Some(true),
            latest_version: Some("2.7032.0".into()),
            error: None,
        };
        for (installed, expected) in [
            ("2.7032.0.0", Some(false)),
            ("2.7032.0", Some(false)),
            ("2.7033.0.0", Some(false)),
            ("2.2553.0.0", Some(true)),
            ("", None),
            ("unknown", None),
        ] {
            assert_eq!(current_update_available(installed, Some(&cached)), expected);
        }
        cached.update_available = Some(false);
        assert_eq!(
            current_update_available("2.2553.0.0", Some(&cached)),
            Some(true)
        );
        cached.error = Some("offline".into());
        assert_eq!(current_update_available("2.2553.0.0", Some(&cached)), None);
        cached.error = None;
        cached.latest_version = None;
        assert_eq!(current_update_available("2.2553.0.0", Some(&cached)), None);
        cached.latest_version = Some("unknown".into());
        assert_eq!(current_update_available("2.2553.0.0", Some(&cached)), None);
        assert_eq!(current_update_available("2.2553.0.0", None), None);
    }

    #[test]
    fn startup_probe_checks_owner_session_and_path_without_admin_process_query() {
        let package = ClaudePackage {
            package_full_name: String::new(),
            package_family_name: FAMILY.into(),
            version: String::new(),
            architecture: "X64".into(),
            install_location: r"C:\fixture\Claude".into(),
        };
        let probe = desktop_running_script(&package);
        for (owner, path, session_offset, expected) in [
            ("$me", ps_path(&package.exe()), 0, "True"),
            ("'S-1-5-21-999'", ps_path(&package.exe()), 0, "False"),
            ("$me", "'C:\\tools\\claude.exe'".into(), 0, "False"),
            ("$me", ps_path(&package.exe()), 1, "False"),
        ] {
            let script = format!(
                r#"
$me=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$mockSession=(Get-Process -Id $PID).SessionId + {session_offset}
function Get-CimInstance {{ [pscustomobject]@{{SessionId=$mockSession;ExecutablePath={path}}} }}
function Invoke-CimMethod {{ [pscustomobject]@{{ReturnValue=0;Sid={owner}}} }}
{probe}
"#
            );
            assert_eq!(util::powershell(&script).unwrap(), expected);
        }
    }

    struct TimeoutReader;

    impl Read for TimeoutReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
    }

    struct SensitiveErrorReader;

    impl Read for SensitiveErrorReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other(
                "proxy at https://downloads.claude.ai/private?token=secret",
            ))
        }
    }

    #[test]
    fn rejects_another_package_as_target() {
        let package = ClaudePackage {
            package_full_name: "Claude_2.1.0.0_x64__other".into(),
            package_family_name: FAMILY.into(),
            version: "2.1.0.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_2.1.0.0_x64__other".into(),
        };
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn close_flow_clears_orphaned_job_without_touching_unrelated_processes() {
        let (sid, session) = util::caller_identity().unwrap();
        let full = format!(
            "Claude_JobFixture_{}_{}_x64__pzs8sxrjxfjjc",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let package = ClaudePackage {
            package_full_name: full.clone(),
            package_family_name: FAMILY.into(),
            version: "0.0.0.0".into(),
            architecture: "X64".into(),
            install_location: format!(r"C:\fixture\{full}"),
        };
        let closing = close_desktop_script(&package, &sid, session);
        let legacy_closing = closing.replace(
            &format!(
                "[ClaudeDesktopJob]::Close({}, $callerSid, [uint32]$session)",
                ps_quote(&full)
            ),
            "# Legacy executable-path-only shutdown",
        );
        let script = format!(
            r#"$ErrorActionPreference='Stop'
$fixturePackage={full}
$fixtureSid={sid}
$fixtureSession=[uint32]{session}
function Get-AppxPackage {{ [pscustomobject]@{{PackageFullName=$fixturePackage;InstallLocation={root}}} }}
function Get-CimInstance {{ return $null }}
Add-Type -TypeDefinition @'
{helper}
'@
$closeFixture={{ {closing} }}
$legacyCloseFixture={{ {legacy_closing} }}
{fixture}
"#,
            full = ps_quote(&full),
            sid = ps_quote(&sid),
            root = ps_path(&package.root()),
            helper = include_str!("claude_job.cs"),
            fixture = include_str!("../../tests/windows_claude_job.ps1"),
        );
        let path = std::env::temp_dir().join(format!("{full}.ps1"));
        util::write_script(&path, &script).unwrap();
        let result = util::run_script(&path);
        fs::remove_file(path).unwrap();
        let output = result.unwrap();
        assert!(output.lines().any(|line| line.trim() == "PASS"), "{output}");
    }

    #[test]
    fn close_flow_reuses_updater_sequence_and_limits_targets() {
        let package = ClaudePackage {
            package_full_name: "Claude_2.1.0.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: FAMILY.into(),
            version: "2.1.0.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_2.1.0.0_x64__pzs8sxrjxfjjc"
                .into(),
        };
        let mocks = format!(
            r#"
$global:events=@()
$global:service=[pscustomobject]@{{State='Running';PathName={service}}}
$global:processes=@(
[pscustomobject]@{{ProcessId=101;SessionId=8;ExecutablePath={exe};CreationDate=1;Owner='S-1-5-21-1'}},
[pscustomobject]@{{ProcessId=102;SessionId=8;ExecutablePath={exe};CreationDate=1;Owner='S-1-5-21-2'}},
[pscustomobject]@{{ProcessId=103;SessionId=9;ExecutablePath={exe};CreationDate=1;Owner='S-1-5-21-1'}},
[pscustomobject]@{{ProcessId=104;SessionId=8;ExecutablePath='C:\tools\claude.exe';CreationDate=1;Owner='S-1-5-21-1'}})
function Get-AppxPackage {{ param($User,$Name) [pscustomobject]@{{PackageFullName={full};InstallLocation={root}}} }}
function Get-CimInstance {{ param($ClassName,$Filter)
    if ($ClassName -eq 'Win32_Service') {{ return $global:service }}
    if ($Filter -eq 'ProcessId=101' -and $global:race) {{
        $copy=$global:processes[0] | Select-Object *
        if ($global:race -eq 'reused') {{ $copy.CreationDate=2; $copy.ExecutablePath='C:\foreign.exe' }} else {{ $copy.ExecutablePath=$null }}
        if ($global:race -ne 'unverifiable') {{
            $global:processes=@($global:processes | Where-Object ProcessId -ne 101)
            $global:race=$null
        }}
        return $copy
    }}
    if ($Filter -like 'ProcessId=*') {{ return @($global:processes | Where-Object {{ $_.ProcessId -eq [int]$Filter.Substring(10) }}) }}
    return $global:processes
}}
function Invoke-CimMethod {{ param($InputObject,$MethodName,$Arguments)
    if ($MethodName -eq 'GetOwnerSid') {{ return [pscustomobject]@{{ReturnValue=0;Sid=$InputObject.Owner}} }}
    if ($MethodName -ne 'Terminate') {{ throw 'Unexpected CIM mutation' }}
    $global:events+=('terminate:' + $InputObject.ProcessId)
    $global:processes=@($global:processes | Where-Object {{ $_.ProcessId -ne $InputObject.ProcessId }})
    [pscustomobject]@{{ReturnValue=0}}
}}
function Stop-Service {{ param($Name) $global:events+='service'; $global:service.State='Stopped' }}
"#,
            service = ps_path(&package.resources().join("cowork-svc.exe")),
            exe = ps_path(&package.exe()),
            full = ps_quote(&package.package_full_name),
            root = ps_path(&package.root())
        );
        let closing = close_desktop_script(&package, "S-1-5-21-1", 8);
        let scenarios = [
            ("", false, "service,terminate:101"),
            ("$global:service=$null", false, "terminate:101"),
            ("$global:race='exiting'", false, "service"),
            ("$global:race='reused'", false, "service"),
            ("$global:race='unverifiable'", true, "service"),
            (
                r"$global:service.PathName='C:\foreign\cowork-svc.exe'",
                true,
                "",
            ),
            ("function Get-AppxPackage { return $null }", true, ""),
        ];
        for (setup, fails, events) in scenarios {
            let script = format!(
                r#"{mocks}
{setup}
$failed=$false
try {{ & {{ {closing} }} | Out-Null }} catch {{ $failed=$true }}
if ($failed -ne ${fails}) {{ throw 'Unexpected close outcome' }}
if (($global:events -join ',') -ne '{events}') {{ throw ('Unexpected actions: ' + ($global:events -join ',')) }}
if (@($global:processes | Where-Object {{ $_.ProcessId -in @(102,103,104) }}).Count -ne 3) {{ throw 'Unrelated process modified' }}
'PASS'
"#
            );
            assert_eq!(util::powershell(&script).unwrap(), "PASS");
        }
    }

    #[test]
    fn process_exit_race_accepts_only_confirmed_not_found() {
        let script = format!(
            r#"
# Produce a real typed CIM NotFound without touching Claude or any existing process.
$child=Start-Process -FilePath "$PSHOME\powershell.exe" -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 2' -WindowStyle Hidden -PassThru
$stale=Get-CimInstance Win32_Process -Filter ('ProcessId=' + $child.Id) -ErrorAction Stop
$child.WaitForExit()
$missing=$null
try {{ Invoke-CimMethod -InputObject $stale -MethodName GetOwnerSid -ErrorAction Stop | Out-Null }} catch [Microsoft.Management.Infrastructure.CimException] {{ $missing=$_.Exception }}
if (-not $missing -or $missing.NativeErrorCode -ne [Microsoft.Management.Infrastructure.NativeErrorCode]::NotFound) {{ throw 'Expected real CIM NotFound' }}
{PROCESS_CIM_METHOD}
$original=[pscustomobject]@{{ProcessId=101;CreationDate=1}}
$live=$null
$errorMode='missing'
function Get-CimInstance {{ return $live }}
function Invoke-CimMethod {{
    if ($errorMode -eq 'missing') {{ throw $missing }}
    if ($errorMode -eq 'denied-code') {{ return [pscustomobject]@{{ReturnValue=2}} }}
    throw [System.UnauthorizedAccessException]::new('Access denied')
}}
foreach ($method in @('GetOwnerSid','Terminate')) {{
    $live=$null
    if ($null -ne (Invoke-AssistantProcessMethod $original $method)) {{ throw 'Exited process should be skipped' }}
    $live=[pscustomobject]@{{ProcessId=101;CreationDate=2}}
    if ($null -ne (Invoke-AssistantProcessMethod $original $method)) {{ throw 'Reused PID should be skipped, never acted on' }}
    $live=$original
    $failed=$false
    try {{ Invoke-AssistantProcessMethod $original $method | Out-Null }} catch {{ $failed=$true }}
    if (-not $failed) {{ throw 'NotFound with original process still present must fail' }}
    $live=$null
    foreach ($errorMode in @('denied-code','denied-exception')) {{
        $failed=$false
        try {{ Invoke-AssistantProcessMethod $original $method | Out-Null }} catch {{ $failed=$true }}
        if (-not $failed) {{ throw 'Access denial must fail even if PID disappears' }}
    }}
    $errorMode='missing'
}}
'PASS'
"#
        );
        assert_eq!(util::powershell(&script).unwrap(), "PASS");
    }

    #[test]
    fn waits_for_appx_visibility_and_rejects_timeout() {
        let mut attempts = 0;
        let mut pauses = 0;
        let result = wait_for_installed_version(
            "2.0.0",
            || {
                attempts += 1;
                if attempts == 1 {
                    return Ok(None);
                }
                Ok(Some(ClaudePackage {
                    package_full_name: String::new(),
                    package_family_name: FAMILY.into(),
                    version: if attempts == 2 { "1.0.0" } else { "2.0.0.0" }.into(),
                    architecture: "X64".into(),
                    install_location: String::new(),
                }))
            },
            || pauses += 1,
        )
        .unwrap();
        assert_eq!(result.version, "2.0.0.0");
        assert_eq!((attempts, pauses), (3, 2));
        let mut attempts = 0;
        let error = wait_for_installed_version(
            "2.0.0",
            || {
                attempts += 1;
                Err("probe error".into())
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(attempts, 30);
        assert!(error.contains("probe error"));
    }

    #[test]
    fn newly_installed_executable_requires_valid_anthropic_signature() {
        let package = ClaudePackage {
            package_full_name: String::new(),
            package_family_name: FAMILY.into(),
            version: String::new(),
            architecture: "X64".into(),
            install_location: r"C:\fixture\Claude".into(),
        };
        let check = official_executable_check_script(&package);
        for (status, publisher, valid) in [
            ("Valid", "CN=Anthropic", true),
            ("HashMismatch", "CN=Anthropic", false),
            ("NotSigned", "", false),
            ("Valid", "CN=Other", false),
        ] {
            let script = format!(
                r#"
function Get-AuthenticodeSignature {{ [pscustomobject]@{{Status='{status}';SignerCertificate=[pscustomobject]@{{Subject='{publisher}'}}}} }}
{check}
"#
            );
            let result = util::powershell(&script);
            assert_eq!(result.is_ok(), valid, "{result:?}");
        }
    }

    #[test]
    fn registration_checks_real_protocol_and_xml_attributes() {
        let xml = r#"<Package xmlns:d='urn:test'><Extensions>
<d:StartupTask TaskId='ClaudeStartup'/><d:Protocol Name='claude'/>
<d:Service Name='CoworkVMService'/><d:FirewallRules Executable='app/Claude.exe'/>
<d:FirewallRules Executable='app/resources/cowork-svc.exe'/>
</Extensions></Package>"#;
        let check = registration_check_script(Path::new("unused-test-manifest.xml"));
        for (xml, has_url, expected) in [
            (xml.to_string(), true, "True"),
            (xml.to_string(), false, "False"),
            (xml.replace("TaskId='ClaudeStartup'", "TaskId='Other'"), true, "False"),
            (xml.replace("app/resources/cowork-svc.exe", "other.exe"), true, "False"),
            ("<Package><!-- ClaudeStartup claude CoworkVMService app/Claude.exe app/resources/cowork-svc.exe --></Package>".into(), true, "False"),
        ] {
            let script = format!(r#"
function Get-Content {{ {xml} }}
function Get-AuthenticodeSignature {{ throw 'Registration must permit an already localized executable' }}
function Get-StartApps {{ [pscustomobject]@{{AppID='{APP_ID}'}} }}
function Get-Item {{
    if (-not ${has_url}) {{ return $null }}
    $key=New-Object PSObject
    $key | Add-Member ScriptMethod GetValue {{ param($name,$default) return '' }}
    $key
}}
{check}
"#, xml=ps_quote(xml));
            assert_eq!(util::powershell(&script).unwrap(), expected);
        }
    }

    #[test]
    fn appx_busy_errors_explain_retry_and_preserved_download() {
        for error in ["HRESULT: 0x80073D02", "部署错误 0x8007001f (0x80073CF9)"] {
            assert_eq!(
                appx_install_error_summary(error),
                "Windows 仍报告 Claude 包被占用，请关闭相关任务后重试并查看执行日志；已校验的下载包会保留。"
            );
        }
        assert_eq!(
            appx_install_error_summary("0x80073CF9"),
            "Appx 注册失败，详细原因见执行日志。"
        );
    }

    #[test]
    fn msix_install_reuses_package_shutdown_flags_without_changing_user() {
        let script = msix_install_command(Path::new(r"C:\cache\Claude's app.msix"));
        assert_eq!(script, "Add-AppxPackage -Path 'C:\\cache\\Claude''s app.msix' -ForceApplicationShutdown -ForceUpdateFromAnyVersion -ErrorAction Stop");
    }

    #[test]
    fn cached_msix_requires_signature_and_matching_manifest() {
        let path = std::env::temp_dir().join(format!(
            "claude-cache-fixture-{}-{}.msix",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let check = cached_msix_check_script(&path, "2.7032.0");
        for (status, subject, version, name, arch, corrupt, valid) in [
            (
                "Valid",
                "CN=Anthropic, PBC",
                "2.7032.0.0",
                "Claude",
                "x64",
                false,
                true,
            ),
            (
                "HashMismatch",
                "CN=Anthropic, PBC",
                "2.7032.0.0",
                "Claude",
                "x64",
                false,
                false,
            ),
            (
                "Valid",
                "CN=Other",
                "2.7032.0.0",
                "Claude",
                "x64",
                false,
                false,
            ),
            (
                "Valid",
                "CN=Anthropic, PBC",
                "2.2553.0.0",
                "Claude",
                "x64",
                false,
                false,
            ),
            (
                "Valid",
                "CN=Anthropic, PBC",
                "2.7032.0.0",
                "Other",
                "x64",
                false,
                false,
            ),
            (
                "Valid",
                "CN=Anthropic, PBC",
                "2.7032.0.0",
                "Claude",
                "arm64",
                false,
                false,
            ),
            (
                "Valid",
                "CN=Anthropic, PBC",
                "2.7032.0.0",
                "Claude",
                "x64",
                true,
                false,
            ),
        ] {
            let script = format!(
                r#"
$fixture={path}
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
try {{
    if (${corrupt}) {{ [System.IO.File]::WriteAllText($fixture,'broken zip') }} else {{
        $zip=[System.IO.Compression.ZipFile]::Open($fixture,[System.IO.Compression.ZipArchiveMode]::Create)
        try {{
            $writer=New-Object System.IO.StreamWriter($zip.CreateEntry('AppxManifest.xml').Open())
            try {{ $writer.Write('<Package><Identity Name="{name}" ProcessorArchitecture="{arch}" Version="{version}"/></Package>') }} finally {{ $writer.Dispose() }}
        }} finally {{ $zip.Dispose() }}
    }}
    function Get-AuthenticodeSignature {{ [pscustomobject]@{{ Status='{status}';SignerCertificate=[pscustomobject]@{{Subject='{subject}'}} }} }}
    $before=(Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
    $accepted=$false
    try {{ $accepted=(& {{ {check} }}) -eq 'VALID' }} catch {{ $accepted=$false }}
    if ($accepted -ne ${valid}) {{ throw 'Unexpected cache decision' }}
    if ((Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash -ne $before) {{ throw 'Cache modified during validation' }}
    'PASS'
}} finally {{ if (Test-Path -LiteralPath $fixture) {{ Remove-Item -LiteralPath $fixture }} }}
"#,
                path = ps_path(&path)
            );
            assert_eq!(util::powershell(&script).unwrap(), "PASS");
        }
    }

    #[test]
    fn downloaded_bytes_reach_operation_snapshot() {
        let operation = OperationState::new();
        let directory = std::env::temp_dir().join(format!(
            "claude-msix-download-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("Claude-test.msix");
        fs::write(&destination, b"previous").unwrap();

        let mut incomplete = Cursor::new(vec![7u8; 3]);
        assert!(save_msix_stream(&mut incomplete, Some(4), &destination, &operation).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!destination.with_extension("msix.partial").exists());

        assert!(
            save_msix_stream(&mut TimeoutReader, None, &destination, &operation)
                .unwrap_err()
                .contains("读取官方 MSIX 下载流超时")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!destination.with_extension("msix.partial").exists());

        let error = save_msix_stream(&mut SensitiveErrorReader, None, &destination, &operation)
            .unwrap_err();
        assert!(error.contains("读取官方 MSIX 下载流失败"));
        assert!(!error.contains("token=secret"));
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!destination.with_extension("msix.partial").exists());

        let mut source = Cursor::new(vec![7u8; 1024]);
        save_msix_stream(&mut source, Some(1024), &destination, &operation).unwrap();
        assert_eq!(fs::metadata(&destination).unwrap().len(), 1024);
        assert_eq!(operation.snapshot().progress, Some(100));
        assert!(operation.snapshot().step.contains("下载官方 x64 MSIX"));

        let unknown_total = OperationState::new();
        let mut source = Cursor::new(vec![8u8; 64]);
        save_msix_stream(&mut source, None, &destination, &unknown_total).unwrap();
        assert_eq!(unknown_total.snapshot().progress, None);
        assert!(unknown_total.snapshot().step.contains("已下载"));
        fs::remove_dir_all(directory).unwrap();
    }
}
