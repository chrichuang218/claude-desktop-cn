// Platform modules provide release transport, installation, and asset names.
const OWNER_REPO: &str = "chrichuang218/claude-desktop-cn";

#[derive(Clone, Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug)]
struct ReleaseInfo {
    version: String,
    asset_url: String,
    checksum_url: String,
    release_url: String,
    notes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedUpdate {
    pub checked_at: String,
    pub update_available: Option<bool>,
    pub latest_version: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub message: String,
}

pub fn last_update_result() -> Result<Option<UpdateResult>, String> {
    let path = util::data_dir()?
        .join("self-update")
        .join("apply-result.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let result = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("助手更新结果无效：{error}"))?;
    Ok(Some(result))
}

#[derive(Deserialize)]
struct ReleaseResponse {
    status: u16,
    content: String,
}

fn parse_release_response(json: &str, allow_missing: bool) -> Result<Option<Release>, String> {
    let response: ReleaseResponse =
        serde_json::from_str(json).map_err(|error| format!("GitHub API 响应无效：{error}"))?;
    match response.status {
        404 if allow_missing => Ok(None),
        200 => serde_json::from_str(&response.content)
            .map(Some)
            .map_err(|error| format!("GitHub Release 数据无效：{error}")),
        code => Err(format!("GitHub API HTTP {code}")),
    }
}

fn validate_release_assets(release: Release) -> Result<ReleaseInfo, String> {
    let executable = release
        .assets
        .iter()
        .find(|asset| asset.name == ASSET)
        .ok_or_else(|| format!("最新 Release 缺少 {ASSET}。"))?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == CHECKSUM)
        .ok_or_else(|| format!("最新 Release 缺少 {CHECKSUM}。"))?;
    for asset in [executable, checksum] {
        if !asset.browser_download_url.starts_with(&format!(
            "https://github.com/{OWNER_REPO}/releases/download/"
        )) {
            return Err("Release 文件下载地址不属于本助手仓库。".into());
        }
    }
    let version = release.tag_name.trim_start_matches('v').to_string();
    if version.is_empty()
        || !version
            .chars()
            .all(|value| value.is_ascii_digit() || value == '.')
    {
        return Err("Release 版本号无法识别。".into());
    }
    Ok(ReleaseInfo {
        version,
        asset_url: executable.browser_download_url.clone(),
        checksum_url: checksum.browser_download_url.clone(),
        release_url: release.html_url,
        notes: release.body.as_deref().and_then(summarize_notes),
    })
}

// Keep the update dialog compact: drop Markdown markers and cap the summary.
fn summarize_notes(body: &str) -> Option<String> {
    let lines = body
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches('#')
                .trim_start_matches(['-', '*'])
                .trim()
                .replace("**", "")
                .replace('`', "")
        })
        .filter(|line| !line.is_empty())
        .take(12)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let text = lines.join("
");
    Some(if text.chars().count() > 600 {
        format!("{}…", text.chars().take(599).collect::<String>())
    } else {
        text
    })
}

#[cfg(test)]
mod repository_tests {
    use super::*;

    #[test]
    fn no_release_is_a_normal_check_result_but_other_failures_remain_errors() {
        let missing = r#"{"status":404,"content":""}"#;
        assert!(parse_release_response(missing, true).unwrap().is_none());
        // A tag that disappeared after the redirect is not proof of no releases.
        assert!(parse_release_response(missing, false).is_err());
        for status in [403, 429, 500] {
            let json = format!(r#"{{"status":{status},"content":""}}"#);
            assert!(parse_release_response(&json, true).is_err());
        }
        assert!(parse_release_response(r#"{"status":200,"content":"null"}"#, true).is_err());
        assert!(parse_release_response("invalid response", true).is_err());
        let cached = assistant_check_cache(&Ok(None));
        assert_eq!(cached.update_available, Some(false));
        assert_eq!(cached.latest_version, None);
        assert_eq!(cached.error, None);
        let outcome = assistant_check_outcome(None, &OperationState::new()).unwrap();
        assert_eq!(outcome.update_available, Some(false));
        assert_eq!(outcome.latest_version, None);
        assert_eq!(outcome.message, "暂无可用的助手更新。");
        let failed = assistant_check_cache(&Err("network unavailable".into()));
        assert_eq!(failed.update_available, None);
        assert_eq!(failed.latest_version, None);
        assert_eq!(failed.error.as_deref(), Some("network unavailable"));
    }

    #[test]
    fn claude_check_cache_preserves_success_and_clears_stale_results_on_error() {
        for available in [true, false] {
            let cached = claude_check_cache(&Ok((available, "2.7032.0".into())));
            let restored: CachedUpdate =
                serde_json::from_slice(&serde_json::to_vec(&cached).unwrap()).unwrap();
            assert_eq!(restored.update_available, Some(available));
            assert_eq!(restored.latest_version.as_deref(), Some("2.7032.0"));
            assert!(restored.error.is_none());
            assert!(!restored.checked_at.is_empty());
        }
        let failed = claude_check_cache(&Err("network unavailable".into()));
        assert_eq!(failed.update_available, None);
        assert_eq!(failed.latest_version, None);
        assert_eq!(failed.error.as_deref(), Some("network unavailable"));
    }

    #[test]
    fn cached_assistant_flag_follows_the_running_version() {
        let cached = |latest: &str| CachedUpdate {
            checked_at: "1".into(),
            update_available: Some(true),
            latest_version: Some(latest.into()),
            error: None,
            release_notes: None,
        };
        // Written by an older binary, read after updating to that release.
        let current = env!("CARGO_PKG_VERSION");
        assert_eq!(refresh_assistant_availability(cached(current)).update_available, Some(false));
        assert_eq!(refresh_assistant_availability(cached("999.0.0")).update_available, Some(true));
        let failed = CachedUpdate { latest_version: None, update_available: None, ..cached(current) };
        assert_eq!(refresh_assistant_availability(failed).update_available, None);
    }

    pub(super) fn fake_release(names: &[&str]) -> Release {
        Release {
            tag_name: "v9.9.9".into(),
            html_url: format!("https://github.com/{OWNER_REPO}/releases/tag/v9.9.9"),
            body: None,
            assets: names
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    browser_download_url: format!(
                        "https://github.com/{OWNER_REPO}/releases/download/v9.9.9/{name}"
                    ),
                })
                .collect(),
        }
    }

    #[test]
    fn release_urls_require_the_current_repository() {
        for (base, accepted) in [
            (format!("https://github.com/{OWNER_REPO}"), true),
            (
                "https://github.com/chrichuang218/claude-windows-cn".into(),
                false,
            ),
            (format!("https://github.com/{OWNER_REPO}-other"), false),
            (
                "https://github.com/another-user/claude-desktop-cn".into(),
                false,
            ),
            (
                format!("https://github.com.example.org/{OWNER_REPO}"),
                false,
            ),
            (format!("http://github.com/{OWNER_REPO}"), false),
        ] {
            let release = Release {
                tag_name: "v1.0.0".into(),
                html_url: format!("{base}/releases/tag/v1.0.0"),
                body: None,
                assets: [ASSET, CHECKSUM]
                    .into_iter()
                    .map(|name| Asset {
                        name: name.into(),
                        browser_download_url: format!("{base}/releases/download/v1.0.0/{name}"),
                    })
                    .collect(),
            };
            assert_eq!(validate_release_assets(release).is_ok(), accepted, "{base}");
        }
    }
}

pub fn check(operation: &OperationState) -> Result<OperationOutcome, String> {
    operation.step("查询助手新版本");
    let result = latest_release();
    save_assistant_cache(&assistant_check_cache(&result))?;
    assistant_check_outcome(result?, operation)
}

fn assistant_check_outcome(
    release: Option<ReleaseInfo>,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    let Some(release) = release else {
        return Ok(OperationOutcome {
            message: "暂无可用的助手更新。".into(),
            update_available: Some(false),
            latest_version: None,
        });
    };
    let available = claude::compare_versions(&release.version, env!("CARGO_PKG_VERSION"))
        == std::cmp::Ordering::Greater;
    operation.log(format!(
        "当前版本：{}；最新版本：{}；来源：{}",
        env!("CARGO_PKG_VERSION"),
        release.version,
        release.release_url
    ));
    Ok(OperationOutcome::update(
        if available {
            format!("发现助手新版本 {}。", release.version)
        } else {
            format!("助手已是最新版本 {}。", release.version)
        },
        available,
        release.version,
    ))
}

fn parse_checksum(raw: &str) -> Result<String, String> {
    let parts = raw
        .trim_start_matches('\u{feff}')
        .split_whitespace()
        .collect::<Vec<_>>();
    let hash = parts.first().copied().unwrap_or("");
    if hash.len() != 64 || !hash.chars().all(|value| value.is_ascii_hexdigit()) {
        return Err("Release SHA256 文件内容无效。".into());
    }
    if parts.len() > 1 && parts[1].trim_start_matches('*') != ASSET {
        return Err("Release SHA256 文件指向其他程序。".into());
    }
    Ok(hash.to_ascii_lowercase())
}

fn verify_checksum(executable: &Path, checksum: &Path) -> Result<String, String> {
    let expected =
        parse_checksum(&fs::read_to_string(checksum).map_err(|error| error.to_string())?)?;
    let actual = util::sha256(executable)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "助手更新摘要不匹配：期望 {expected}，实际 {actual}。"
        ));
    }
    Ok(actual)
}

fn run_self_test(executable: &Path) -> Result<(), String> {
    let mut command = Command::new(executable);
    util::hide_window(&mut command);
    let mut child = command
        .arg("--self-test")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("新版助手自检启动失败：{error}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("新版助手自检失败，退出码 {:?}。", status.code()))
            };
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            return Err("新版助手自检超时。".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn cache_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("daily-update.json"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyCache {
    assistant: Option<CachedUpdate>,
    claude: Option<CachedUpdate>,
    checked_epoch: u64,
}

fn read_cache() -> Result<DailyCache, String> {
    let path = cache_path()?;
    if !path.exists() {
        return Ok(DailyCache {
            assistant: None,
            claude: None,
            checked_epoch: 0,
        });
    }
    serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|error| format!("读取每日更新记录失败：{error}"))
}

fn write_cache(cache: &DailyCache) -> Result<(), String> {
    let path = cache_path()?;
    fs::create_dir_all(path.parent().ok_or("更新记录目录无效。")?)
        .map_err(|error| error.to_string())?;
    let stage = path.with_extension("json.tmp");
    fs::write(
        &stage,
        serde_json::to_vec_pretty(cache).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    util::replace_file(&stage, &path)
}

fn save_assistant_cache(update: &CachedUpdate) -> Result<(), String> {
    let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
    let mut cache = read_cache()?;
    cache.assistant = Some(update.clone());
    write_cache(&cache)
}

pub fn cached_assistant_update() -> Result<Option<CachedUpdate>, String> {
    let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
    Ok(read_cache()?.assistant.map(refresh_assistant_availability))
}

// The cache outlives the binary that wrote it; after a self-update the stored
// flag still compares against the old version, so recompute it on read.
fn refresh_assistant_availability(mut cached: CachedUpdate) -> CachedUpdate {
    if let Some(latest) = cached.latest_version.as_deref() {
        cached.update_available = Some(
            claude::compare_versions(latest, env!("CARGO_PKG_VERSION"))
                == std::cmp::Ordering::Greater,
        );
    }
    cached
}

pub fn cached_claude_update() -> Result<Option<CachedUpdate>, String> {
    let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
    Ok(read_cache()?.claude)
}

pub fn save_claude_check(result: &Result<(bool, String), String>) -> Result<(), String> {
    let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
    let mut cache = read_cache()?;
    cache.claude = Some(claude_check_cache(result));
    write_cache(&cache)
}

fn claude_check_cache(result: &Result<(bool, String), String>) -> CachedUpdate {
    match result {
        Ok((available, version)) => CachedUpdate {
            checked_at: now_string(),
            update_available: Some(*available),
            latest_version: Some(version.clone()),
            error: None,
            release_notes: None,
        },
        Err(error) => CachedUpdate {
            checked_at: now_string(),
            update_available: None,
            latest_version: None,
            error: Some(error.clone()),
            release_notes: None,
        },
    }
}

fn assistant_check_cache(result: &Result<Option<ReleaseInfo>, String>) -> CachedUpdate {
    match result {
        Ok(release) => CachedUpdate {
            checked_at: now_string(),
            update_available: Some(release.as_ref().is_some_and(|info| {
                claude::compare_versions(&info.version, env!("CARGO_PKG_VERSION"))
                    == std::cmp::Ordering::Greater
            })),
            latest_version: release.as_ref().map(|info| info.version.clone()),
            error: None,
            release_notes: release.as_ref().and_then(|info| info.notes.clone()),
        },
        Err(error) => CachedUpdate {
            checked_at: now_string(),
            update_available: None,
            latest_version: None,
            error: Some(error.clone()),
            release_notes: None,
        },
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_string() -> String {
    now_epoch().to_string()
}

pub fn start_daily_scheduler() {
    thread::spawn(|| loop {
        if let Ok(config) = crate::assistant::load_config() {
            if config.daily_update_check {
                let _ = daily_check_if_due();
            }
        }
        thread::sleep(Duration::from_secs(60 * 60));
    });
}

pub fn trigger_daily_check() {
    thread::spawn(|| {
        let _ = daily_check_if_due();
    });
}

fn daily_check_if_due() -> Result<(), String> {
    let previous_claude = {
        let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
        let cache = read_cache()?;
        if now_epoch().saturating_sub(cache.checked_epoch) < 24 * 60 * 60 {
            return Ok(());
        }
        cache.claude
    };
    let assistant = Some(assistant_check_cache(&latest_release()));
    let claude = Some(claude_check_cache(&claude::daily_update_info()));
    let _guard = CACHE_LOCK.lock().map_err(|error| error.to_string())?;
    let mut cache = read_cache()?;
    if now_epoch().saturating_sub(cache.checked_epoch) < 24 * 60 * 60 {
        return Ok(());
    }
    cache.assistant = assistant;
    // A manual check may finish while the daily network requests are pending.
    if cache.claude == previous_claude {
        cache.claude = claude;
    }
    cache.checked_epoch = now_epoch();
    write_cache(&cache)
}
