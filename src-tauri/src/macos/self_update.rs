use crate::{
    assistant, claude,
    operation::{OperationOutcome, OperationState},
    util,
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_arch = "aarch64")]
const ASSET: &str = "claude-cn-macos-arm64.app.tar.gz";
#[cfg(not(target_arch = "aarch64"))]
const ASSET: &str = "claude-cn-macos-x64.app.tar.gz";
#[cfg(target_arch = "aarch64")]
const CHECKSUM: &str = "claude-cn-macos-arm64.app.tar.gz.sha256";
#[cfg(not(target_arch = "aarch64"))]
const CHECKSUM: &str = "claude-cn-macos-x64.app.tar.gz.sha256";
static CACHE_LOCK: Mutex<()> = Mutex::new(());

include!("../self_update_shared.rs");

/// Startup cleanup: each update removes its own extraction; stale ones were left by a crash.
/// The single archive is overwritten by the next update and kept as diagnostic evidence.
pub fn clean_cache(data: &Path) -> Vec<String> {
    let work = data.join("self-update");
    util::remove_entries(data, &work, |name| {
        name.starts_with("unpack-") && util::is_stale(&work.join(name))
    })
}

fn release_api(url: &str, allow_missing: bool) -> Result<Option<Release>, String> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claude-windows-cn")
        .https_only(true)
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let content = response.text().map_err(|error| error.to_string())?;
    parse_release_response(
        &serde_json::json!({"status": status, "content": content}).to_string(),
        allow_missing,
    )
}

fn latest_release() -> Result<Option<ReleaseInfo>, String> {
    let release = release_api(
        &format!("https://api.github.com/repos/{OWNER_REPO}/releases/latest"),
        true,
    )
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
    let result = install_update(operation);
    if let Err(error) = &result {
        let path = util::data_dir()?.join("self-update/apply-result.json");
        fs::create_dir_all(path.parent().ok_or("更新结果路径无效。")?)
            .map_err(|record| format!("{error}；创建结果目录失败：{record}"))?;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&UpdateResult {
                ok: false,
                message: error.clone(),
            })
            .map_err(|record| record.to_string())?,
        )
        .map_err(|record| format!("{error}；写入更新结果失败：{record}"))?;
    }
    result
}

fn install_update(operation: &OperationState) -> Result<OperationOutcome, String> {
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
    let target = assistant::current_bundle()?;
    if target.file_name().and_then(|name| name.to_str()) != Some(assistant::APP_NAME) {
        return Err("当前应用包名称不是正式助手名称，不能原位自更新。".into());
    }
    let work = util::data_dir()?.join("self-update");
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let archive = work.join(ASSET);
    let checksum = work.join(CHECKSUM);
    operation.step("下载助手应用包及 SHA256 摘要");
    util::download(&release.asset_url, &archive)?;
    util::download(&release.checksum_url, &checksum)?;
    let actual = verify_checksum(&archive, &checksum)?;
    operation.log(format!("下载地址：{}；SHA256：{actual}", release.asset_url));
    let unpacked = work.join(format!("unpack-{}-{}", std::process::id(), now_epoch()));
    fs::create_dir(&unpacked).map_err(|error| format!("创建独立解压目录失败：{error}"))?;
    let result_path = work.join("apply-result.json");
    let result: Result<(), String> = (|| {
        extract_archive(&archive, &unpacked)?;
        let staged = unpacked.join(assistant::APP_NAME);
        assistant::validate_bundle(&staged, Some(&release.version))?;
        verify_architecture(&assistant::bundle_executable(&staged))?;
        operation.step("运行新版助手无副作用自检");
        run_self_test(&assistant::bundle_executable(&staged))?;
        let elevated = assistant::needs_elevation(&target)?;
        operation.step("替换助手应用包");
        let swap = assistant::replace_bundle(&staged, &target, elevated)?;
        let replaced: Result<(), String> = (|| {
            assistant::validate_bundle(&target, Some(&release.version))?;
            run_self_test(&assistant::bundle_executable(&target))?;
            // macOS can atomically rename a running app. LaunchServices starts
            // the new bundle as the original user before the old GUI exits.
            let output = Command::new("/usr/bin/open")
                .arg("-n")
                .arg(&target)
                .output()
                .map_err(|error| format!("新版助手启动失败：{error}"))?;
            if !output.status.success() {
                return Err(format!(
                    "新版助手启动失败：{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            Ok(())
        })();
        if let Err(error) = replaced {
            swap.rollback()
                .map_err(|rollback| format!("{error}；回退失败：{rollback}"))?;
            return Err(format!("{error}；已恢复旧版应用包。"));
        }
        if let Err(error) = swap.finish() {
            operation.log(format!("新版已启动，旧版备份已保留：{error}"));
        }
        Ok(())
    })();
    let outcome = UpdateResult {
        ok: result.is_ok(),
        message: result
            .as_ref()
            .map(|_| format!("助手已更新至 {}。", release.version))
            .unwrap_or_else(Clone::clone),
    };
    fs::write(
        &result_path,
        serde_json::to_vec_pretty(&outcome).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("写入更新结果失败：{error}"))?;
    // This directory was created by this operation and contains only its
    // validated release archive. Keep the archive/checksum as diagnostic evidence.
    if let Err(error) = fs::remove_dir_all(&unpacked) {
        operation.log(format!(
            "无法清理更新解压目录 {}：{error}",
            unpacked.display()
        ));
    }
    result?;
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(2));
        std::process::exit(0);
    });
    Ok(OperationOutcome::done(
        "助手已完成更新并启动新版，旧版窗口即将退出。",
    ))
}

fn valid_archive_entry(name: &str) -> bool {
    let path = Path::new(name.trim_end_matches('/'));
    let mut parts = path.components();
    matches!(parts.next(), Some(std::path::Component::Normal(value)) if value == assistant::APP_NAME)
        && parts.all(|value| matches!(value, std::path::Component::Normal(_)))
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<(), String> {
    // The release contains exactly one app. Reject traversal and non-app roots
    // before tar writes anything; BSD tar also rejects escaping link targets.
    let listed = Command::new("/usr/bin/tar")
        .args(["-tzf"])
        .arg(archive)
        .output()
        .map_err(|error| error.to_string())?;
    let names = String::from_utf8(listed.stdout).map_err(|error| error.to_string())?;
    if !listed.status.success()
        || names.is_empty()
        || names.lines().any(|name| !valid_archive_entry(name))
    {
        return Err("助手压缩包包含无效路径或不是单个正式应用包。".into());
    }
    let output = Command::new("/usr/bin/tar")
        .args(["-xzf"])
        .arg(archive)
        .args(["--no-same-owner", "-C"])
        .arg(destination)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "解压助手应用包失败：{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}
fn verify_architecture(executable: &Path) -> Result<(), String> {
    let expected = if cfg!(target_arch = "aarch64") {
        0x0100_000c
    } else {
        0x0100_0007
    };
    let mut header = Vec::new();
    fs::File::open(executable)
        .map_err(|error| error.to_string())?
        .take(4096)
        .read_to_end(&mut header)
        .map_err(|error| error.to_string())?;
    if !macho_has_architecture(&header, expected) {
        return Err(format!(
            "助手应用包不是有效的 {} Mach-O 程序。",
            std::env::consts::ARCH
        ));
    }
    Ok(())
}

fn macho_has_architecture(header: &[u8], expected: u32) -> bool {
    let Some(magic) = header.get(..4) else {
        return false;
    };
    let (little, entry_size) = match magic {
        [0xcf, 0xfa, 0xed, 0xfe] => (true, 0),
        [0xfe, 0xed, 0xfa, 0xcf] => (false, 0),
        [0xca, 0xfe, 0xba, 0xbe] => (false, 20),
        [0xbe, 0xba, 0xfe, 0xca] => (true, 20),
        [0xca, 0xfe, 0xba, 0xbf] => (false, 32),
        [0xbf, 0xba, 0xfe, 0xca] => (true, 32),
        _ => return false,
    };
    let number = |bytes: &[u8]| {
        let value: [u8; 4] = bytes.try_into().unwrap();
        if little {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        }
    };
    let Some(value) = header.get(4..8) else {
        return false;
    };
    if entry_size == 0 {
        return header.len() >= 32 && number(value) == expected;
    }
    let count = number(value) as usize;
    count > 0
        && count <= 32
        && header.len() >= 8 + count * entry_size
        && header[8..]
            .chunks_exact(entry_size)
            .take(count)
            .any(|entry| number(&entry[..4]) == expected)
}

pub fn self_test() -> bool {
    env!("CARGO_PKG_NAME") == "claude-windows-cn"
        && assistant::current_bundle()
            .and_then(|bundle| verify_architecture(&assistant::bundle_executable(&bundle)))
            .is_ok()
        && std::panic::catch_unwind(|| {
            let _ = crate::app_context();
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::repository_tests::fake_release;
    use super::*;

    #[test]
    fn macho_header_rejects_wrong_architecture_and_truncated_fat_tables() {
        let arm = 0x0100_000c_u32;
        let intel = 0x0100_0007_u32;
        let mut thin = vec![0_u8; 32];
        thin[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        thin[4..8].copy_from_slice(&arm.to_le_bytes());
        assert!(macho_has_architecture(&thin, arm));
        assert!(!macho_has_architecture(&thin, intel));
        assert!(!macho_has_architecture(&thin[..7], arm));
        let mut fat = vec![0_u8; 48];
        fat[..4].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
        fat[4..8].copy_from_slice(&2_u32.to_be_bytes());
        fat[8..12].copy_from_slice(&arm.to_be_bytes());
        fat[28..32].copy_from_slice(&intel.to_be_bytes());
        assert!(macho_has_architecture(&fat, arm));
        assert!(macho_has_architecture(&fat, intel));
        assert!(!macho_has_architecture(&fat[..47], arm));
        assert!(!macho_has_architecture(b"not an executable", arm));
    }

    #[test]
    fn release_requires_matching_architecture_and_checksum() {
        assert!(validate_release_assets(fake_release(&[ASSET, CHECKSUM])).is_ok());
        assert!(validate_release_assets(fake_release(&[CHECKSUM])).is_err());
        assert!(validate_release_assets(fake_release(&[ASSET])).is_err());
        assert!(validate_release_assets(fake_release(&[
            "claude-windows-cn.exe",
            "claude-windows-cn.exe.sha256"
        ]))
        .is_err());
        let mut wrong_host = fake_release(&[ASSET, CHECKSUM]);
        wrong_host.assets[0].browser_download_url = "https://example.org/update".into();
        assert!(validate_release_assets(wrong_host).is_err());
    }

    #[test]
    fn checksum_and_archive_entries_are_bound_to_our_bundle() {
        let hash = "a".repeat(64);
        assert!(parse_checksum(&format!("{hash}  {ASSET}")).is_ok());
        assert!(parse_checksum(&format!("{hash}  other.app.tar.gz")).is_err());
        assert!(parse_checksum("broken").is_err());
        assert!(valid_archive_entry(
            "Claude 中文助手.app/Contents/MacOS/claude-windows-cn"
        ));
        for name in [
            "/Claude 中文助手.app",
            "../Claude 中文助手.app",
            "Other.app/file",
            "Claude 中文助手.app/../../external",
        ] {
            assert!(!valid_archive_entry(name), "{name}");
        }
    }
}
