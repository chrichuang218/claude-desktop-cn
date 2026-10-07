pub use crate::util::compare_versions;
use crate::{
    operation::{OperationOutcome, OperationState},
    patch,
    util::{self, run_shell, shell_quote as sh_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const BUNDLE_ID: &str = "com.anthropic.claudefordesktop";
const RELEASES_URL: &str = "https://downloads.claude.ai/releases/darwin/universal/RELEASES.json";
const DOWNLOAD_PREFIX: &str = "https://downloads.claude.ai/releases/darwin/universal/";
const SIGNING_REQUIREMENT: &str =
    "=anchor apple generic and certificate leaf[subject.OU] = \"Q6L2SF6YDW\"";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ClaudePackage {
    pub package_full_name: String,
    pub version: String,
    pub install_location: String,
}

impl ClaudePackage {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.install_location)
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

fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("启动 macOS 命令失败：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "macOS 命令失败（{:?}）：{}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "无法确定当前 macOS 用户目录。".into())
}

fn allowed_roots() -> Result<[PathBuf; 2], String> {
    Ok([
        PathBuf::from("/Applications/Claude.app"),
        home()?.join("Applications/Claude.app"),
    ])
}

fn no_symlink_ancestors(path: &Path) -> Result<(), String> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) if info.file_type().is_symlink() => {
                return Err(format!("目标路径包含符号链接，已停止：{}", part.display()))
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(format!("检查路径 {} 失败：{error}", part.display())),
        }
    }
    Ok(())
}

fn validate_target(path: &Path) -> Result<(), String> {
    if !allowed_roots()?.contains(&path.to_path_buf()) {
        return Err("Claude 目标必须位于 /Applications 或当前用户的 Applications 目录。".into());
    }
    no_symlink_ancestors(path)
}

fn plist(app: &Path, key: &str) -> Result<String, String> {
    run(Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-", "--"])
        .arg(app.join("Contents/Info.plist")))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part.parse::<u64>().is_ok()
        })
}

pub(crate) fn read_package(app: &Path) -> Result<ClaudePackage, String> {
    no_symlink_ancestors(&app.join("Contents/Info.plist"))?;
    if plist(app, "CFBundleIdentifier")? != BUNDLE_ID
        || plist(app, "CFBundleExecutable")? != "Claude"
    {
        return Err(format!(
            "{} 不是预期的 Claude Desktop 应用。",
            app.display()
        ));
    }
    let version = plist(app, "CFBundleShortVersionString")?;
    let build = plist(app, "CFBundleVersion")?;
    if !valid_version(&version) || !valid_version(&build) {
        return Err("Claude 应用版本信息无效。".into());
    }
    let exe = app.join("Contents/MacOS/Claude");
    let resources = app.join("Contents/Resources");
    no_symlink_ancestors(&exe)?;
    no_symlink_ancestors(&resources)?;
    no_symlink_ancestors(&resources.join("app.asar"))?;
    if !exe.is_file() || !resources.is_dir() || !resources.join("app.asar").is_file() {
        return Err("Claude 应用包缺少主程序或 app.asar。".into());
    }
    let binary = run(Command::new("/usr/bin/file").arg("-b").arg(&exe))?;
    let architecture = match (binary.contains("arm64"), binary.contains("x86_64")) {
        (true, true) => "universal",
        (true, false) => "arm64",
        (false, true) => "x86_64",
        _ => return Err("Claude 主程序不是受支持的 macOS 架构。".into()),
    };
    if architecture != "universal"
        && architecture
            != if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x86_64"
            }
    {
        return Err("Claude 应用架构与当前助手不匹配。".into());
    }
    Ok(ClaudePackage {
        package_full_name: format!("{BUNDLE_ID}_{version}_{build}"),
        version,
        install_location: app.to_string_lossy().into(),
    })
}

pub fn query_package() -> Result<Option<ClaudePackage>, String> {
    let mut found = None;
    for path in allowed_roots()? {
        validate_target(&path)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("读取 Claude 安装位置失败：{error}")),
            Ok(_) => (),
        }
        if found.is_some() {
            return Err("/Applications 与当前用户的 Applications 中均存在 Claude.app，请保留一个明确的安装目标。".into());
        }
        found = Some(read_package(&path)?);
    }
    Ok(found)
}

pub fn status() -> Result<ClaudeStatus, String> {
    let package = query_package()?;
    let cached = crate::self_update::cached_claude_update()?;
    let state = package.as_ref().map(patch::state).transpose()?;
    let latest = cached
        .as_ref()
        .and_then(|value| value.latest_version.clone());
    let update_available = package.as_ref().and_then(|package| {
        let latest = latest.as_deref()?;
        if cached.as_ref()?.error.is_some() || !valid_version(latest) {
            return None;
        }
        Some(compare_versions(latest, &package.version) == Ordering::Greater)
    });
    let recovery = state.as_ref().is_some_and(|state| state.recovery_required);
    let external = state
        .as_ref()
        .is_some_and(|state| state.external_localization);
    Ok(ClaudeStatus {
        installed: package.is_some(),
        version: package
            .as_ref()
            .map_or(String::new(), |p| p.version.clone()),
        package_full_name: package
            .as_ref()
            .map_or(String::new(), |p| p.package_full_name.clone()),
        install_path: package
            .as_ref()
            .map_or(String::new(), |p| p.install_location.clone()),
        applied_mode: state.as_ref().and_then(|state| state.applied_mode.clone()),
        backup_ready: state.as_ref().is_some_and(|state| state.backup_ready),
        external_localization: external,
        patch_recovery_required: recovery,
        message: if recovery {
            "上次汉化未完成，需人工核查本次备份与文件。"
        } else if external {
            "检测到其他来源的中文资源或备份，助手不会接管。"
        } else if package.is_some() {
            "已检测到官方 Claude Desktop。"
        } else {
            "未安装官方 Claude Desktop。"
        }
        .into(),
        update_available,
        latest_version: latest,
        update_check_error: cached.as_ref().and_then(|value| value.error.clone()),
        last_update_check: cached.map(|value| value.checked_at),
    })
}

// libproc reports the executable path independently of argv/process names. This
// avoids closing another user's Claude, the CLI, or a similarly named program.
#[cfg(target_os = "macos")]
fn process_path(pid: i32) -> Option<PathBuf> {
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
    }
    let mut bytes = [0u8; 4096];
    let count = unsafe { proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if count <= 0 {
        return None;
    }
    let end = bytes.iter().position(|byte| *byte == 0)?;
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

fn desktop_pids(package: &ClaudePackage) -> Result<Vec<i32>, String> {
    let uid = run(Command::new("/usr/bin/id").arg("-u"))?;
    let rows = run(Command::new("/bin/ps").args(["-axo", "uid=,pid="]))?;
    let prefix = package.root().join("Contents");
    let mut result = Vec::new();
    for row in rows.lines() {
        let mut fields = row.split_whitespace();
        if fields.next() != Some(uid.as_str()) {
            continue;
        }
        let pid: i32 = fields
            .next()
            .ok_or("进程列表缺少 PID。")?
            .parse()
            .map_err(|_| "进程 PID 无效。")?;
        if process_path(pid).is_some_and(|path| path.starts_with(&prefix)) {
            result.push(pid);
        }
    }
    Ok(result)
}

pub fn close_desktop(package: &ClaudePackage) -> Result<(), String> {
    validate_target(&package.root())?;
    let live = query_package()?.ok_or("Claude Desktop 安装位置已变化。")?;
    if live.package_full_name != package.package_full_name || live.root() != package.root() {
        return Err("Claude Desktop 在操作期间已更新，请刷新后重试。".into());
    }
    for pid in desktop_pids(package)? {
        // Refresh path/owner immediately before sending the signal. kill itself
        // runs without elevation, so it can never terminate another user's app.
        if !desktop_pids(package)?.contains(&pid) {
            continue;
        }
        if let Err(error) = run(Command::new("/bin/kill").args(["-TERM", &pid.to_string()])) {
            if desktop_pids(package)?.contains(&pid) {
                return Err(error);
            }
        }
    }
    for _ in 0..60 {
        if desktop_pids(package)?.is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("Claude Desktop 尚未退出；请保存任务并完全退出后重试。".into())
}

pub fn launch() -> Result<(), String> {
    let package = query_package()?.ok_or("未安装官方 Claude Desktop。")?;
    run(Command::new("/usr/bin/open").arg(package.root()))?;
    for _ in 0..20 {
        if !desktop_pids(&package)?.is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("已发送启动请求，但未检测到该 Claude.app 的进程。".into())
}

#[derive(Deserialize)]
struct ReleaseFeed {
    #[serde(rename = "currentRelease")]
    current: String,
    releases: Vec<FeedRelease>,
}

#[derive(Deserialize)]
struct FeedRelease {
    version: String,
    #[serde(rename = "updateTo")]
    update: ReleaseMetadata,
}

#[derive(Deserialize)]
struct ReleaseMetadata {
    version: String,
    url: String,
}

fn parse_metadata(raw: &str) -> Result<ReleaseMetadata, String> {
    let feed: ReleaseFeed =
        serde_json::from_str(raw).map_err(|error| format!("官方 macOS 更新清单无效：{error}"))?;
    if !valid_version(&feed.current) {
        return Err("官方 macOS 版本无效。".into());
    }
    let mut matching = feed
        .releases
        .into_iter()
        .filter(|entry| entry.version == feed.current);
    let metadata = matching
        .next()
        .ok_or("官方更新清单缺少当前发行包。")?
        .update;
    if matching.next().is_some() || metadata.version != feed.current {
        return Err("官方更新清单包含不一致的发行版本。".into());
    }
    let prefix = format!("{DOWNLOAD_PREFIX}{}/Claude-", metadata.version);
    let digest = metadata
        .url
        .strip_prefix(&prefix)
        .and_then(|tail| tail.strip_suffix(".zip"));
    if !digest
        .is_some_and(|digest| !digest.is_empty() && digest.bytes().all(|ch| ch.is_ascii_hexdigit()))
    {
        return Err("官方 macOS 下载地址不符合预期。".into());
    }
    Ok(metadata)
}

fn client() -> Result<reqwest::blocking::Client, String> {
    let builder: reqwest::blocking::ClientBuilder = reqwest::Client::builder()
        .timeout(Duration::from_secs(20 * 60))
        .into();
    builder
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("创建官方请求失败：{error}"))
}

fn official_metadata() -> Result<ReleaseMetadata, String> {
    let response = client()?
        .get(RELEASES_URL)
        .send()
        .map_err(|error| format!("读取官方 macOS 版本失败：{}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!("官方更新接口返回 HTTP {}。", response.status()));
    }
    let mut raw = String::new();
    response
        .take(1024 * 1024)
        .read_to_string(&mut raw)
        .map_err(|error| format!("读取更新清单失败：{error}"))?;
    parse_metadata(&raw)
}

pub fn daily_update_info() -> Result<(bool, String), String> {
    let metadata = official_metadata()?;
    let available = query_package()?.as_ref().is_none_or(|package| {
        compare_versions(&metadata.version, &package.version) == Ordering::Greater
    });
    Ok((available, metadata.version))
}

pub fn check_update(operation: &OperationState) -> Result<OperationOutcome, String> {
    operation.step("查询官方 Claude Desktop macOS 版本");
    let result = daily_update_info();
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

pub(crate) fn verify_official(app: &Path) -> Result<(), String> {
    read_package(app)?;
    run(Command::new("/usr/bin/codesign")
        .args([
            "--verify",
            "--deep",
            "--strict",
            "--test-requirement",
            SIGNING_REQUIREMENT,
        ])
        .arg(app))
    .map(|_| ())
}

fn nonce() -> Result<String, String> {
    Ok(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
    ))
}

fn writable_parent(target: &Path) -> Result<bool, String> {
    let parent = target.parent().ok_or("Claude 目标缺少父目录。")?;
    let probe = parent.join(format!(".claude-cn-write-{}", nonce()?));
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            fs::remove_file(probe).map_err(|e| e.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(format!("无法检查目标目录写权限：{error}")),
    }
    if target.exists() {
        let owner = run(Command::new("/usr/bin/stat").args(["-f", "%u"]).arg(target))?;
        if owner != run(Command::new("/usr/bin/id").arg("-u"))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Copy before touching the installed app; retain the original until the caller
/// verifies the replacement. A failed verification restores the entire app.
pub(crate) fn replace_bundle(
    source: &Path,
    target: &Path,
    verify: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    validate_target(target)?;
    let parent = target.parent().ok_or("Claude 目标缺少父目录。")?;
    if !parent.exists() && target == home()?.join("Applications/Claude.app") {
        fs::create_dir(parent)
            .map_err(|error| format!("创建用户 Applications 目录失败：{error}"))?;
    }
    replace_bundle_at(source, target, verify)
}

fn replace_bundle_at(
    source: &Path,
    target: &Path,
    verify: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    read_package(source)?;
    let parent = target.parent().ok_or("Claude 目标缺少父目录。")?;
    no_symlink_ancestors(parent)?;
    let elevated = !writable_parent(target)?;
    let tag = nonce()?;
    let stage = parent.join(format!(".claude-cn-stage-{tag}.app"));
    let previous = parent.join(format!(".claude-cn-previous-{tag}.app"));
    let old = if target.exists() {
        Some(read_package(target)?)
    } else {
        None
    };
    let target_q = sh_quote(target);
    let stage_q = sh_quote(&stage);
    let previous_q = sh_quote(&previous);
    // Repeat the path checks inside the privileged process, after any password
    // prompt, and again immediately before changing the installed app.
    let guard = target
        .ancestors()
        .map(|path| format!("test ! -L {}", sh_quote(path)))
        .collect::<Vec<_>>()
        .join("\n");
    let check = if let Some(old) = &old {
        let build = plist(target, "CFBundleVersion")?;
        format!("test \"$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - -- {target}/Contents/Info.plist)\" = {bundle}\ntest \"$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - -- {target}/Contents/Info.plist)\" = {version}\ntest \"$(/usr/bin/plutil -extract CFBundleVersion raw -o - -- {target}/Contents/Info.plist)\" = {build}\n/bin/mv {target} {previous}",
            target = target_q, version = sh_quote(&old.version), previous = previous_q,
            bundle = sh_quote(BUNDLE_ID), build = sh_quote(build))
    } else {
        format!("test ! -e {target_q} && test ! -L {target_q}")
    };
    let install = format!(
        r#"set -eu
{guard}
test ! -e {stage} && test ! -L {stage}
test ! -e {previous} && test ! -L {previous}
/usr/bin/ditto {source} {stage}
{guard}
{check}
if ! /bin/mv {stage} {target}; then
    if test -d {previous}; then /bin/mv {previous} {target}; fi
    exit 1
fi
"#,
        stage = stage_q,
        previous = previous_q,
        source = sh_quote(source),
        target = target_q
    );
    if let Err(error) = run_shell(&install, elevated) {
        return Err(format!(
            "替换 Claude.app 失败：{error}；暂存或回退文件（若有）保留在 {}。",
            parent.display()
        ));
    }
    if let Err(error) = verify(target) {
        let rollback = if old.is_some() {
            format!("set -eu\n{guard}\ntest ! -L {previous_q}\n/bin/mv {target_q} {stage_q}\nif ! /bin/mv {previous_q} {target_q}; then /bin/mv {stage_q} {target_q}; exit 1; fi\n/bin/rm -rf {stage_q}")
        } else {
            format!("set -eu\n{guard}\n/bin/mv {target_q} {stage_q}\n/bin/rm -rf {stage_q}")
        };
        return match run_shell(&rollback, elevated) {
            Ok(_) => Err(format!("新应用验证失败，已回退：{error}")),
            Err(rollback_error) => Err(format!(
                "新应用验证失败：{error}；回退也失败：{rollback_error}；原包保留位置：{}",
                previous.display()
            )),
        };
    }
    if old.is_some() {
        if let Err(error) = run_shell(
            &format!("set -eu\n{guard}\ntest ! -L {previous_q}\n/bin/rm -rf {previous_q}"),
            elevated,
        ) {
            return Ok(Some(format!(
                "新应用已验证，但清理旧副本 {} 失败：{error}",
                previous.display()
            )));
        }
    }
    Ok(None)
}

fn download_zip(url: &str, destination: &Path, operation: &OperationState) -> Result<(), String> {
    let mut response = client()?
        .get(url)
        .send()
        .map_err(|error| format!("下载官方 Claude 失败：{}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!("官方下载返回 HTTP {}。", response.status()));
    }
    let total = response.content_length();
    let partial = destination.with_extension("zip.partial");
    let result = (|| {
        let mut file = fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut downloaded = 0u64;
        let mut last = Instant::now();
        let mut buffer = [0u8; 128 * 1024];
        operation.download_progress(0, total);
        loop {
            let count = response
                .read(&mut buffer)
                .map_err(|error| format!("读取官方下载流失败（{:?}）。", error.kind()))?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])
                .map_err(|e| e.to_string())?;
            downloaded += count as u64;
            if last.elapsed() >= Duration::from_millis(250) {
                operation.download_progress(downloaded, total);
                last = Instant::now();
            }
        }
        if downloaded == 0 || total.is_some_and(|total| total != downloaded) {
            return Err("官方 Claude macOS 安装包下载不完整。".into());
        }
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        util::replace_file(&partial, destination)?;
        operation.download_progress(downloaded, total);
        Ok(())
    })();
    if result.is_err() && partial.exists() {
        fs::remove_file(&partial)
            .map_err(|error| format!("下载失败且清理临时文件失败：{error}"))?;
    }
    result
}

fn extract_official(zip: &Path, work: &Path, version: &str) -> Result<PathBuf, String> {
    let entries = run(Command::new("/usr/bin/tar").arg("-tf").arg(zip))?;
    validate_archive_entries(&entries)?;
    // macOS bsdtar reads ZIP and refuses traversal through archive-created
    // symlinks. Never extract with -P (which disables those safety checks).
    run(Command::new("/usr/bin/tar")
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(work)
        .arg("--no-same-owner"))?;
    let app = work.join("Claude.app");
    verify_official(&app)?;
    if read_package(&app)?.version != version {
        return Err("官方安装包版本与更新清单不一致。".into());
    }
    let minimum = plist(&app, "LSMinimumSystemVersion")?;
    let current = run(Command::new("/usr/bin/sw_vers").arg("-productVersion"))?;
    if !valid_version(&minimum) || compare_versions(&current, &minimum) == Ordering::Less {
        return Err(format!(
            "此版本 Claude Desktop 需要 macOS {minimum} 或更新版本。"
        ));
    }
    Ok(app)
}

fn validate_archive_entries(entries: &str) -> Result<(), String> {
    if entries.is_empty() {
        return Err("官方安装压缩包为空。".into());
    }
    for name in entries.lines() {
        if !(name == "Claude.app"
            || name.starts_with("Claude.app/")
            || name.starts_with("__MACOSX/"))
            || name.contains('\\')
            || name.split('/').any(|part| part == "..")
        {
            return Err("官方安装压缩包包含目标应用之外的路径。".into());
        }
    }
    Ok(())
}

pub fn install(operation: &OperationState, only_update: bool) -> Result<OperationOutcome, String> {
    operation.step("获取官方 Claude Desktop macOS 安装包信息");
    let metadata = official_metadata()?;
    let current = query_package()?;
    match current.as_ref() {
        Some(package)
            if !only_update
                || compare_versions(&metadata.version, &package.version) != Ordering::Greater =>
        {
            return Ok(OperationOutcome::done(format!(
                "当前已安装 Claude Desktop {}。",
                package.version
            )));
        }
        None if only_update => return Err("尚未安装 Claude Desktop，请先安装。".into()),
        _ => (),
    }
    let directory = util::data_dir()?.join("official-macos");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let zip = directory.join(format!("Claude-{}.zip", metadata.version));
    let work = directory.join(format!("extract-{}", nonce()?));
    fs::create_dir(&work).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut app = None;
        if zip.is_file() {
            operation.step("校验官方 macOS 安装包缓存");
            match extract_official(&zip, &work, &metadata.version) {
                Ok(verified) => {
                    operation.log("已验证缓存版本、应用签名与 Anthropic 发布者，复用下载缓存。");
                    app = Some(verified);
                }
                Err(error) => {
                    operation.log(format!("缓存校验失败，将重新下载：{error}"));
                    fs::remove_dir_all(&work).map_err(|e| e.to_string())?;
                    fs::create_dir(&work).map_err(|e| e.to_string())?;
                }
            }
        }
        let app = match app {
            Some(app) => app,
            None => {
                operation.step("下载官方 macOS 安装包");
                download_zip(&metadata.url, &zip, operation)?;
                operation.step("校验官方应用签名、架构与版本");
                extract_official(&zip, &work, &metadata.version)?
            }
        };
        operation.log(format!(
            "官方版本：{}；来源：{}；SHA256：{}",
            metadata.version,
            metadata.url,
            util::sha256(&zip)?
        ));
        if let Some(package) = &current {
            operation.step("关闭当前用户的 Claude Desktop");
            close_desktop(package)?;
        }
        let target = match &current {
            Some(package) => package.root(),
            None => home()?.join("Applications/Claude.app"),
        };
        operation.step("安装官方 Claude Desktop 并验证");
        if let Some(warning) = replace_bundle(&app, &target, |installed| {
            verify_official(installed)?;
            let found = query_package()?.ok_or("安装后未检测到 Claude Desktop。")?;
            if found.root() != target || found.version != metadata.version {
                return Err("安装后的应用路径或版本不一致。".into());
            }
            Ok(())
        })? {
            operation.log(warning);
        }
        Ok(OperationOutcome::done(format!(
            "Claude Desktop {} 已安装并通过官方签名检查。",
            metadata.version
        )))
    })();
    match fs::remove_dir_all(&work) {
        Ok(()) => result,
        Err(error) => {
            operation.log(format!("清理解压目录 {} 失败：{error}", work.display()));
            result
        }
    }
}

pub fn create_shortcut(operation: &OperationState) -> Result<OperationOutcome, String> {
    let package = query_package()?.ok_or("未安装官方 Claude Desktop，无法创建快捷方式。")?;
    operation.step("创建 Claude Desktop 桌面快捷方式");
    let desktop = home()?.join("Desktop");
    no_symlink_ancestors(&desktop)?;
    if !desktop.is_dir() {
        return Err("当前用户桌面目录不存在。".into());
    }
    let link = desktop.join("Claude Desktop.app");
    match fs::symlink_metadata(&link) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(&link).map_err(|e| e.to_string())? == package.root() =>
        {
            ()
        }
        Ok(_) => return Err("桌面已有同名项目且不属于当前官方 Claude Desktop。".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            run(Command::new("/bin/ln")
                .arg("-s")
                .arg(package.root())
                .arg(&link))?;
        }
        Err(error) => return Err(format!("读取桌面快捷方式失败：{error}")),
    }
    operation.log(format!("Claude Desktop 快捷方式：{}", link.display()));
    Ok(OperationOutcome::done(
        "Claude Desktop 桌面快捷方式已就绪。",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_requirement_is_inline_and_rejects_another_publisher() {
        // /bin/ls is signed by Apple, not Anthropic. A parsed requirement must
        // reject its identity, rather than try to open the expression as a file.
        let output = Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--strict",
                "--test-requirement",
                SIGNING_REQUIREMENT,
                "/bin/ls",
            ])
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            error.contains("code failed to satisfy specified code requirement"),
            "{error}"
        );
    }

    #[test]
    fn official_feed_requires_matching_current_version_and_exact_official_url() {
        let feed = r#"{"currentRelease":"2.9939.2","releases":[{"version":"2.9939.2","updateTo":{"version":"2.9939.2","url":"https://downloads.claude.ai/releases/darwin/universal/2.9939.2/Claude-d3e50475d5d6bb0c317560310200249dd61b87d8.zip"}}]}"#;
        assert_eq!(parse_metadata(feed).unwrap().version, "2.9939.2");
        for invalid in [
            feed.replace(
                "https://downloads.claude.ai/",
                "https://downloads.claude.ai.evil.invalid/",
            ),
            feed.replace("/2.9939.2/", "/2.9939.1/"),
            feed.replace(".zip", ".zip?token=unexpected"),
            feed.replace("Claude-d3", "Claude-../d3"),
            feed.replacen("\"version\":\"2.9939.2\"", "\"version\":\"2.9939.1\"", 1),
        ] {
            assert!(parse_metadata(&invalid).is_err());
        }
        assert_eq!(compare_versions("2.9939.2", "2.9939.2.0"), Ordering::Equal);
        assert_eq!(sh_quote("/Users/a'b/$x"), "'/Users/a'\\''b/$x'");
        assert!(!valid_version("2.+1.0"));
        assert!(!valid_version("2..0"));
        assert!(validate_archive_entries(
            "Claude.app/Contents/Info.plist\n__MACOSX/Claude.app/._Contents"
        )
        .is_ok());
        for entry in [
            "../Claude.app/file",
            "Claude.app/../../outside",
            "/Applications/Claude.app",
            "Claude.app\\..\\outside",
        ] {
            assert!(validate_archive_entries(entry).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_transaction_rolls_back_failed_verification_and_reports_cleanup_warning() {
        use std::os::unix::fs::symlink;

        // These are isolated, unsigned fixtures, never the user's installed app.
        let directory = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("claude-bundle-transaction-{}", nonce().unwrap()));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("replacement.app");
        let target = directory.join("installed.app");
        let make_bundle = |app: &Path, version: &str, bytes: &[u8]| {
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::create_dir(app.join("Contents/Resources")).unwrap();
            fs::copy("/usr/bin/true", app.join("Contents/MacOS/Claude")).unwrap();
            fs::write(app.join("Contents/Resources/app.asar"), bytes).unwrap();
            fs::write(app.join("Contents/Info.plist"), format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string><key>CFBundleExecutable</key><string>Claude</string><key>CFBundleVersion</key><string>{version}</string><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>"
            )).unwrap();
        };
        let snapshot = |app: &Path| {
            [
                "Contents/Info.plist",
                "Contents/MacOS/Claude",
                "Contents/Resources/app.asar",
            ]
            .map(|path| fs::read(app.join(path)).unwrap())
        };
        make_bundle(&target, "1.0.0", b"original bytes");
        make_bundle(&source, "2.0.0", b"replacement bytes");
        let before = snapshot(&target);
        let failure = replace_bundle_at(&source, &target, |_| {
            Err("fixture verification failure".into())
        })
        .unwrap_err();
        assert!(failure.contains("已回退"), "{failure}");
        assert_eq!(snapshot(&target), before);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);

        let protected = directory.join("preserved-original.app");
        let warning = replace_bundle_at(&source, &target, |_| {
            let previous = fs::read_dir(&directory)
                .map_err(|e| e.to_string())?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".claude-cn-previous-")
                })
                .ok_or("missing original fixture")?;
            // Force a cleanup failure without depending on permission behavior
            // (or root privileges), and prove cleanup cannot follow this link.
            fs::rename(&previous, &protected).map_err(|e| e.to_string())?;
            symlink(&protected, &previous).map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        assert!(warning.is_some());
        assert_eq!(snapshot(&target), snapshot(&source));
        assert_eq!(snapshot(&protected), before);
        fs::remove_dir_all(&directory).unwrap();
    }
}
