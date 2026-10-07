use crate::engine_cache::download_engine;
use crate::{
    claude::{self, ClaudePackage},
    operation::{OperationOutcome, OperationState},
    util::{self, ps_path, ps_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const ENGINE_URL: &str =
    "https://github.com/javaht/claude-desktop-zh-cn/archive/refs/heads/main.zip";
const ENGINE_DOWNLOAD_URL: &str =
    "https://codeload.github.com/javaht/claude-desktop-zh-cn/zip/refs/heads/main";

// Reused from claude-desktop-zh: retain its bundle batching and effort-label adaptation.
const FAST_HARDCODED_FRONTEND_PATCH_FUNCTION: &str = r#"
function Patch-HardcodedFrontendStrings {
    param(
        [string]$ResourcesPath,
        [string]$Language
    )

    $assetsDir = Join-Path $ResourcesPath "ion-dist\assets\v1"
    $jsFiles = @(Get-ChildItem (Join-Path $assetsDir "*.js") -ErrorAction SilentlyContinue)
    if ($jsFiles.Count -eq 0) {
        throw "未找到前端 JS bundle: $assetsDir"
    }

    $plainMap = @{}
    $plainSources = New-Object System.Collections.Generic.List[string]
    $rawPairs = New-Object System.Collections.Generic.List[object]
    foreach ($pair in @(Get-FrontendHardcodedReplacements $Language)) {
        $source = [string]$pair[0]
        $target = [string]$pair[1]
        if (Test-StructuralJsReplacement $source) {
            continue
        }
        if (Test-PlainUiTextReplacement $source) {
            if (-not $plainMap.ContainsKey($source)) {
                $plainMap[$source] = $target
                [void]$plainSources.Add($source)
            }
        } else {
            [void]$rawPairs.Add(@($source, $target))
        }
    }

    $plainRegex = $null
    if ($plainSources.Count -gt 0) {
        $escaped = foreach ($source in $plainSources) {
            [System.Text.RegularExpressions.Regex]::Escape($source)
        }
        $quoteClass = '["' + "'" + [char]96 + ']'
        $pattern = '(?<quote>' + $quoteClass + ')(?<source>' + ($escaped -join '|') + ')\k<quote>'
        $plainRegex = [System.Text.RegularExpressions.Regex]::new(
            $pattern,
            [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
        )
    }

    $patchedFiles = 0
    $patchedStrings = 0
    $fileIndex = 0
    foreach ($file in $jsFiles) {
        $fileIndex += 1
        if (($fileIndex -eq 1) -or ($fileIndex % 50 -eq 0) -or ($fileIndex -eq $jsFiles.Count)) {
            Write-Host "  scanning frontend bundles: $fileIndex/$($jsFiles.Count)" -ForegroundColor DarkGray
        }

        $text = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::UTF8)
        $patched = $text
        $count = 0

        foreach ($pair in $rawPairs) {
            $source = [string]$pair[0]
            if (-not $patched.Contains($source)) {
                continue
            }
            $target = [string]$pair[1]
            $index = $patched.IndexOf($source, [System.StringComparison]::Ordinal)
            while ($index -ge 0) {
                $count += 1
                $index = $patched.IndexOf($source, $index + $source.Length, [System.StringComparison]::Ordinal)
            }
            $patched = $patched.Replace($source, $target)
        }

        if ($patched.Contains('"low","medium","high","xhigh","max"')) {
            $script:__effortLabelReplacementCount = 0
            $effortLabelPattern = '(?<prefix>label:)(?<item>[$A-Za-z_][$\w]*)\.name,value:\k<item>\.id,checked:(?<checked>[^,{}]+),onSelect:\(\)=>(?<select>[$A-Za-z_][$\w]*)\(\k<item>\.id,!1\)\}'
            $effortLabelRegex = [System.Text.RegularExpressions.Regex]::new(
                $effortLabelPattern,
                [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
            )
            $patched = $effortLabelRegex.Replace($patched, {
                param($match)
                $script:__effortLabelReplacementCount += 1
                $item = $match.Groups["item"].Value
                $checked = $match.Groups["checked"].Value
                $select = $match.Groups["select"].Value
                return 'label:({low:"低",medium:"中",high:"高",xhigh:"超高",max:"最高"}[' + $item + '.id]??' + $item + '.name),value:' + $item + '.id,checked:' + $checked + ',onSelect:()=>'+ $select + '(' + $item + '.id,!1)}'
            })
            $count += $script:__effortLabelReplacementCount
            $script:__effortLabelReplacementCount = 0
        }

        if ($plainRegex) {
            $script:__frontendReplacementCount = 0
            $patched = $plainRegex.Replace($patched, {
                param($match)
                $source = $match.Groups["source"].Value
                $target = $plainMap[$source]
                if ($null -eq $target) {
                    return $match.Value
                }
                $script:__frontendReplacementCount += 1
                $quote = $match.Groups["quote"].Value
                return $quote + $target + $quote
            })
            $count += $script:__frontendReplacementCount
            $script:__frontendReplacementCount = 0
        }

        if ($patched -ne $text) {
            Backup-ModifiedFile $ResourcesPath $file.FullName
            [System.IO.File]::WriteAllText($file.FullName, $patched, $Utf8NoBom)
            $patchedFiles += 1
            $patchedStrings += $count
        }
    }

    Write-Host "  patched hardcoded frontend strings: $patchedStrings replacements in $patchedFiles files" -ForegroundColor Green
}
"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupFile {
    relative_path: String,
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingPatch {
    package_full_name: String,
    version: String,
    install_location: String,
    engine_sha256: String,
}

pub struct PatchState {
    pub applied_mode: Option<String>,
    pub backup_ready: bool,
    pub external_localization: bool,
    pub recovery_required: bool,
}

fn manifest_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("patch-backup.json"))
}

fn pending_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("patch-pending.json"))
}

pub fn recovery_message(package: &ClaudePackage) -> Result<String, String> {
    let root = read_pending()?
        .map(|pending| PathBuf::from(pending.install_location))
        .unwrap_or_else(|| package.root());
    Ok(format!(
        "上次汉化未完成，需人工核查。记录：{}；备份：{}。请保留日志和备份，不要删除整个助手数据目录。",
        pending_path()?.display(),
        root.join("app/resources/.zh-cn-backups").display()
    ))
}

fn read_pending() -> Result<Option<PendingPatch>, String> {
    let path = pending_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(path).map_err(|error| format!("读取未完成汉化记录失败：{error}"))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("未完成汉化记录无效，需人工核查：{error}"))
}

fn clear_pending() -> Result<(), String> {
    fs::remove_file(pending_path()?).map_err(|error| format!("清除未完成汉化记录失败：{error}"))
}

fn read_manifest() -> Result<Option<BackupManifest>, String> {
    let path = manifest_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| format!("读取补丁备份记录失败：{error}"))?;
    let manifest =
        serde_json::from_str(&raw).map_err(|error| format!("补丁备份记录无效：{error}"))?;
    Ok(Some(manifest))
}

fn save_manifest(manifest: &BackupManifest) -> Result<(), String> {
    util::write_json(&manifest_path()?, manifest)
        .map_err(|error| format!("保存备份记录失败：{error}"))
}

pub fn state(package: &ClaudePackage) -> Result<PatchState, String> {
    let pending = read_pending()?;
    let recovery_required = pending.is_some();
    let pending_for_current = pending.as_ref().is_some_and(|item| {
        item.package_full_name == package.package_full_name
            && item.version == package.version
            && item.install_location == package.install_location
    });
    let external_assets = has_localization_assets(&package.resources());
    let Some(manifest) = read_manifest()? else {
        return Ok(PatchState {
            applied_mode: None,
            backup_ready: false,
            external_localization: external_assets && !pending_for_current,
            recovery_required,
        });
    };
    if manifest.package_full_name != package.package_full_name
        || manifest.version != package.version
        || manifest.install_location != package.install_location
    {
        return Ok(PatchState {
            applied_mode: None,
            backup_ready: false,
            external_localization: external_assets && !pending_for_current,
            recovery_required,
        });
    }
    let valid = validate_backup(&manifest, package).is_ok();
    let localized = package
        .resources()
        .join("ion-dist")
        .join("i18n")
        .join("zh-CN.json")
        .is_file();
    Ok(PatchState {
        applied_mode: if localized && !recovery_required {
            manifest.applied_mode
        } else {
            None
        },
        backup_ready: valid,
        external_localization: external_assets && !valid && !pending_for_current,
        recovery_required,
    })
}

fn has_localization_assets(resources: &Path) -> bool {
    if resources.join(".zh-cn-backups").exists() {
        return true;
    }
    ["zh-CN", "zh-TW", "zh-HK"].iter().any(|language| {
        resources
            .join("ion-dist")
            .join("i18n")
            .join(format!("{language}.json"))
            .exists()
            || resources.join(format!("{language}.json")).exists()
    })
}

fn validate_backup(manifest: &BackupManifest, package: &ClaudePackage) -> Result<PathBuf, String> {
    if manifest.package_full_name != package.package_full_name
        || manifest.version != package.version
        || manifest.install_location != package.install_location
    {
        return Err("备份不属于当前 Claude Desktop 包和版本。".into());
    }
    let root = package.resources().join(".zh-cn-backups");
    let backup = PathBuf::from(&manifest.backup_set);
    if backup.parent() != Some(root.as_path()) || !backup.is_dir() || manifest.files.is_empty() {
        return Err("当前版本备份目录无效或没有备份文件。".into());
    }
    let actual = collect_backup_files(&backup)?;
    if actual.len() != manifest.files.len() {
        return Err("备份文件数量与记录不一致。".into());
    }
    for file in &manifest.files {
        validate_relative(&file.relative_path)?;
        let item = actual
            .iter()
            .find(|item| item.relative_path == file.relative_path)
            .ok_or_else(|| format!("备份缺少文件：{}", file.relative_path))?;
        if item.size != file.size || item.sha256 != file.sha256 {
            return Err(format!("备份文件摘要不匹配：{}", file.relative_path));
        }
    }
    Ok(backup)
}

fn validate_relative(relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("备份包含不安全路径：{relative}"));
    }
    Ok(())
}

fn collect_backup_files(root: &Path) -> Result<Vec<BackupFile>, String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|error| format!("读取备份目录失败：{error}"))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("备份目录包含符号链接。".into());
            }
            if metadata.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !metadata.is_file() {
                return Err("备份目录包含非常规文件。".into());
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('/', "\\");
            validate_relative(&relative)?;
            files.push(BackupFile {
                relative_path: relative,
                sha256: util::sha256(&entry.path())?,
                size: metadata.len(),
            });
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

struct Engine {
    root: PathBuf,
    script: PathBuf,
    sha256: String,
}

fn fetch_engine(operation: &OperationState) -> Result<Engine, String> {
    operation.step("检查最新汉化引擎");
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let work = util::data_dir()?
        .join("patch-engine")
        .join(format!("run-{}-{time}", std::process::id()));
    fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let zip = work.join("main.zip");
    let cache_dir = util::data_dir()?.join("patch-engine").join("cache");
    let cache = download_engine(ENGINE_DOWNLOAD_URL, &cache_dir, &zip, operation)?;
    let sha256 = cache.sha256.clone();
    operation.log(format!(
        "引擎来源：{ENGINE_URL}；检查时间：{time}；SHA256：{sha256}"
    ));
    let extracted = work.join("extracted");
    util::powershell(&format!(
        "Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
        ps_path(&zip),
        ps_path(&extracted)
    ))?;
    let roots = fs::read_dir(&extracted)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err("最新引擎压缩包目录结构不符合预期。".into());
    }
    let root = roots[0].clone();
    let source = root.join("scripts").join("install_windows.ps1");
    let contents = fs::read_to_string(&source)
        .map_err(|error| format!("最新引擎缺少 Windows 脚本：{error}"))?;
    if !root.join("resources").join("frontend-zh-CN.json").is_file()
        || !contents.contains("function Find-ClaudePath")
        || !contents.contains("function Get-ClaudeResourcesPath")
        || !contents.contains("function Restore-LatestBackup")
        || !contents.contains("function Uninstall-WindowsLanguagePack")
    {
        return Err("最新引擎结构不兼容，已停止操作。".into());
    }
    // Publish only archives that passed extraction and the engine structure checks.
    fs::create_dir_all(&cache_dir).map_err(|error| error.to_string())?;
    let archive_stage = cache_dir.join("archive.partial");
    fs::copy(&zip, &archive_stage).map_err(|error| error.to_string())?;
    util::replace_file(&archive_stage, &cache_dir.join(format!("{sha256}.zip")))?;
    let metadata_stage = cache_dir.join("metadata.partial");
    fs::write(
        &metadata_stage,
        serde_json::to_vec(&cache).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    util::replace_file(&metadata_stage, &cache_dir.join("metadata.json"))?;
    Ok(Engine {
        root,
        script: source,
        sha256,
    })
}

pub fn apply(mode: &str, operation: &OperationState) -> Result<OperationOutcome, String> {
    if !matches!(mode, "safe" | "full") {
        return Err("汉化模式无效。".into());
    }
    let package = claude::query_package()?.ok_or("未安装官方 Claude Desktop。")?;
    if read_pending()?.is_some() {
        return Err(recovery_message(&package)?);
    }
    let existing = read_manifest()?.filter(|manifest| {
        manifest.package_full_name == package.package_full_name
            && manifest.version == package.version
            && manifest.install_location == package.install_location
    });
    if let Some(ref manifest) = existing {
        validate_backup(manifest, &package)?;
    }
    let backup_root = package.resources().join(".zh-cn-backups");
    if existing.is_none() && has_localization_assets(&package.resources()) {
        return Err("检测到其他来源的中文资源或备份，助手不会接管。".into());
    }
    let engine = fetch_engine(operation)?;
    let script = adapt_engine(&engine, &package)?;
    let pending = if existing.is_none() {
        let pending = PendingPatch {
            package_full_name: package.package_full_name.clone(),
            version: package.version.clone(),
            install_location: package.install_location.clone(),
            engine_sha256: engine.sha256.clone(),
        };
        Some(pending)
    } else {
        None
    };
    let result = (|| {
        if let Some(mut manifest) = existing {
            operation.step("按当前版本备份恢复原始文件");
            run_engine(
                &engine,
                &script,
                &package,
                "uninstall",
                "safe",
                None,
                operation,
            )?;
            verify_restored(&manifest, &package)?;
            manifest.applied_mode = None;
            save_manifest(&manifest)?;
        }
        operation.step("应用简体中文资源");
        let upstream_mode = if mode == "safe" { "safe" } else { "official" };
        run_engine(
            &engine,
            &script,
            &package,
            "install",
            upstream_mode,
            pending.as_ref(),
            operation,
        )?;
        let set = unique_backup_set(&backup_root)?;
        let files = collect_backup_files(&set)?;
        if files.is_empty() {
            return Err("补丁操作完成，但没有可验证的原始文件备份。".into());
        }
        if !package
            .resources()
            .join("ion-dist")
            .join("i18n")
            .join("zh-CN.json")
            .is_file()
        {
            return Err("脚本退出成功，但未找到简体中文资源。".into());
        }
        let manifest = BackupManifest {
            package_full_name: package.package_full_name.clone(),
            version: package.version.clone(),
            install_location: package.install_location.clone(),
            backup_set: set.display().to_string(),
            files,
            applied_mode: Some(mode.into()),
            engine_sha256: engine.sha256.clone(),
        };
        save_manifest(&manifest)?;
        if pending.is_some() {
            clear_pending()?;
        }
        Ok(OperationOutcome::done("简体中文已应用。"))
    })();
    match (pending, result) {
        (Some(pending), Err(error)) => {
            operation.log(format!("汉化失败：{error}"));
            // The elevated wrapper records the transaction immediately before engine writes.
            // UAC / preflight failures have no transaction to roll back.
            if read_pending()?.is_none() {
                return Err(error);
            }
            match rollback_first_apply(&pending, &package, &engine, &script, operation) {
                Ok(()) => Err(format!("{error}；操作失败但已验证回滚。")),
                Err(rollback_error) => Err(format!(
                    "{error}；无法证明安全恢复：{rollback_error}。{}",
                    recovery_message(&package)?
                )),
            }
        }
        (_, result) => result,
    }
}

fn unique_backup_set(root: &Path) -> Result<PathBuf, String> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|error| format!("读取当前版本备份失败：{error}"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("当前版本备份根目录无效。".into());
    }
    let entries = fs::read_dir(root)
        .map_err(|error| format!("读取当前版本备份失败：{error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取当前版本备份失败：{error}"))?;
    if entries.len() != 1 {
        return Err("当前版本备份目录数量不符合预期。".into());
    }
    let entry = &entries[0];
    let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("当前版本备份不是常规目录。".into());
    }
    Ok(entry.path())
}

fn rollback_first_apply(
    pending: &PendingPatch,
    package: &ClaudePackage,
    engine: &Engine,
    script: &Path,
    operation: &OperationState,
) -> Result<(), String> {
    if pending.package_full_name != package.package_full_name
        || pending.version != package.version
        || pending.install_location != package.install_location
        || pending.engine_sha256 != engine.sha256
    {
        return Err("未完成记录与当前包或引擎不一致".into());
    }
    let set = unique_backup_set(&package.resources().join(".zh-cn-backups"))?;
    let files = collect_backup_files(&set)?;
    if files.is_empty() {
        return Err("本次备份为空".into());
    }
    let mut manifest = BackupManifest {
        package_full_name: package.package_full_name.clone(),
        version: package.version.clone(),
        install_location: package.install_location.clone(),
        backup_set: set.display().to_string(),
        files,
        applied_mode: None,
        engine_sha256: engine.sha256.clone(),
    };
    validate_backup(&manifest, package)?;
    operation.step("汉化失败，恢复本次备份");
    run_engine(
        engine,
        script,
        package,
        "uninstall",
        "safe",
        None,
        operation,
    )?;
    verify_restored(&manifest, package)?;
    manifest.applied_mode = None;
    save_manifest(&manifest)?;
    clear_pending()?;
    Ok(())
}

pub fn restore(operation: &OperationState) -> Result<OperationOutcome, String> {
    let package = claude::query_package()?.ok_or("未安装官方 Claude Desktop。")?;
    if read_pending()?.is_some() {
        return Err(recovery_message(&package)?);
    }
    let mut manifest = read_manifest()?.ok_or("没有本助手创建的当前版本备份，无法恢复。")?;
    validate_backup(&manifest, &package)?;
    let engine = fetch_engine(operation)?;
    let script = adapt_engine(&engine, &package)?;
    operation.step("恢复当前 Claude 版本的原始文件");
    run_engine(
        &engine,
        &script,
        &package,
        "uninstall",
        "safe",
        None,
        operation,
    )?;
    verify_restored(&manifest, &package)?;
    manifest.applied_mode = None;
    manifest.engine_sha256 = engine.sha256;
    save_manifest(&manifest)?;
    Ok(OperationOutcome::done("当前 Claude 版本的原始文件已恢复。"))
}

fn verify_restored(manifest: &BackupManifest, package: &ClaudePackage) -> Result<(), String> {
    for file in &manifest.files {
        let target = backup_target(&package, &file.relative_path)?;
        if !target.is_file() || util::sha256(&target)? != file.sha256 {
            return Err(format!("恢复后文件校验失败：{}", file.relative_path));
        }
    }
    for language_file in [
        package
            .resources()
            .join("ion-dist")
            .join("i18n")
            .join("zh-CN.json"),
        package.resources().join("zh-CN.json"),
        package
            .resources()
            .join("ion-dist")
            .join("i18n")
            .join("statsig")
            .join("zh-CN.json"),
    ] {
        if language_file.exists() {
            return Err(format!(
                "恢复后仍存在简体中文资源：{}",
                language_file.display()
            ));
        }
    }
    Ok(())
}

fn backup_target(package: &ClaudePackage, relative: &str) -> Result<PathBuf, String> {
    validate_relative(relative)?;
    if let Some(rest) = relative.strip_prefix("_app\\") {
        validate_relative(rest)?;
        Ok(package.root().join("app").join(rest))
    } else {
        Ok(package.resources().join(relative))
    }
}

fn replace_function(source: &str, name: &str, replacement: &str) -> Result<String, String> {
    let marker = format!("function {name}");
    let start = source
        .match_indices(&marker)
        .find(|(index, _)| {
            source[index + marker.len()..]
                .chars()
                .next()
                .is_some_and(|next| next.is_whitespace() || matches!(next, '{' | '('))
        })
        .map(|(index, _)| index)
        .ok_or_else(|| format!("最新引擎缺少 {name}。"))?;
    let body = source[start..]
        .find('{')
        .map(|index| start + index)
        .ok_or_else(|| format!("{name} 结构无效。"))?;
    let mut depth = 0i32;
    for (offset, character) in source[body..].char_indices() {
        if character == '{' {
            depth += 1;
        }
        if character == '}' {
            depth -= 1;
            if depth == 0 {
                let end = body + offset + 1;
                return Ok(format!(
                    "{}{}\n{}",
                    &source[..start],
                    replacement,
                    &source[end..]
                ));
            }
        }
    }
    Err(format!("最新引擎 {name} 括号不匹配。"))
}

fn adapt_engine(engine: &Engine, package: &ClaudePackage) -> Result<PathBuf, String> {
    let source = fs::read_to_string(&engine.script).map_err(|error| error.to_string())?;
    // write_script adds one UTF-8 BOM; retaining the source BOM breaks PowerShell param().
    let mut content = source.trim_start_matches('\u{feff}').to_owned();
    let marker = "$script:DetectedUnpackagedClaudePaths = @(Get-UnpackagedClaudePaths)";
    if !content.contains(marker) {
        return Err("最新引擎的 Claude 目标选择结构已变化。".into());
    }
    content = content.replace(marker, "$script:DetectedUnpackagedClaudePaths = @()");
    let acl_call = "Enable-WriteAccess $resourcesPath";
    if content.matches(acl_call).count() != 1 {
        return Err("最新引擎的写权限调用结构已变化。".into());
    }
    content = content.replace(
        acl_call,
        "Write-Host '跳过引擎内部 ACL 更新；外层已授予原调用用户写权限。'",
    );
    content = replace_function(
        &content,
        "Patch-HardcodedFrontendStrings",
        FAST_HARDCODED_FRONTEND_PATCH_FUNCTION,
    )?;
    let (sid, session) = util::caller_identity()?;
    content = replace_function(
        &content,
        "Stop-ClaudeProcesses",
        &format!(
            "function Stop-ClaudeProcesses {{\n{}\n}}",
            claude::close_desktop_script(package, &sid, session)
        ),
    )?;
    let find = format!(
        "function Find-ClaudePath {{ return {} }}",
        ps_quote(&package.install_location)
    );
    content = replace_function(&content, "Find-ClaudePath", &find)?;
    let config = format!(
        r#"function Get-ClaudeConfigPaths {{
        $roaming = {roaming}; $local = {local}; $family = {family}
        return @(
            (Join-Path $roaming 'Claude\config.json'),
            (Join-Path $local ('Packages\' + $family + '\LocalCache\Roaming\Claude\config.json'))
        )
    }}"#,
        roaming = ps_quote(env::var("APPDATA").map_err(|_| "无法读取当前用户 AppData。")?),
        local = ps_quote(env::var("LOCALAPPDATA").map_err(|_| "无法读取当前用户 LocalAppData。")?),
        family = ps_quote(&package.package_family_name),
    );
    content = replace_function(&content, "Get-ClaudeConfigPaths", &config)?;
    let process = format!(
        r#"function Test-ClaudeDesktopProcessPath {{
        param([string]$Path)
        return ($Path -and $Path.StartsWith({prefix}, [System.StringComparison]::OrdinalIgnoreCase))
    }}"#,
        prefix = ps_quote(format!("{}\\app\\", package.install_location))
    );
    content = replace_function(&content, "Test-ClaudeDesktopProcessPath", &process)?;
    for name in [
        "Disable-FridaResidentForDiskInstall",
        "Remove-LegacyAppxForkArtifacts",
        "Repair-ClaudeDesktopShortcut",
        "Start-CoworkVMService",
    ] {
        content = replace_function(&content, name, &format!("function {name} {{ }}"))?;
    }
    content = replace_function(
        &content,
        "Restart-Claude",
        "function Restart-Claude { param([string]$ClaudePath) }",
    )?;
    content = replace_function(
        &content,
        "Invoke-PreInstallCleanup",
        "function Invoke-PreInstallCleanup { }",
    )?;
    let backup_guard = r#"function Restore-LatestBackup {
        param([string]$ResourcesPath)
        $root=Join-Path $ResourcesPath '.zh-cn-backups'
        $sets=@(Get-ChildItem -LiteralPath $root -Directory -ErrorAction Stop)
        if ($sets.Count -ne 1) { throw '当前版本备份目录无效。' }
        $backup=$sets[0].FullName
        $files=@(Get-ChildItem -LiteralPath $backup -File -Recurse -ErrorAction Stop)
        if ($files.Count -eq 0) { throw '当前版本备份为空。' }
        foreach ($file in $files) {
            $relative=$file.FullName.Substring($backup.Length).TrimStart('\','/')
            if ($relative -match '(^|[\\/])\.\.([\\/]|$)') { throw '备份包含不安全路径。' }
            if ($relative.StartsWith('_app\',[System.StringComparison]::OrdinalIgnoreCase)) {
                $target=Join-Path (Split-Path -Parent $ResourcesPath) $relative.Substring(5)
            } else { $target=Join-Path $ResourcesPath $relative }
            New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
            Copy-Item -LiteralPath $file.FullName -Destination $target -Force -ErrorAction Stop
        }
    }"#;
    content = replace_function(&content, "Restore-LatestBackup", backup_guard)?;
    let uninstall = r#"function Uninstall-WindowsLanguagePack {
        $paths=Get-ClaudeResourcesPath; $resourcesPath=$paths['Resources']
        Stop-ClaudeProcesses
        Restore-LatestBackup $resourcesPath
        Remove-LanguageFiles $resourcesPath
        # Restored bundles already contain the original language lists; do not rewrite them.
        Set-ClaudeLocale 'en-US'
    }"#;
    content = replace_function(&content, "Uninstall-WindowsLanguagePack", uninstall)?;
    let adapted = engine
        .root
        .join("scripts")
        .join("install_windows_assistant.ps1");
    util::write_script(&adapted, &content)?;
    let syntax = format!("$tokens=$null; $errors=$null; [System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$tokens, [ref]$errors) | Out-Null; if ($errors.Count -gt 0) {{ throw ($errors | ForEach-Object {{ $_.Message }} | Out-String) }}", ps_path(&adapted));
    util::powershell(&syntax).map_err(|error| format!("适配后的汉化脚本语法错误：{error}"))?;
    Ok(adapted)
}

// Called only when the same package's service was running before this operation.
pub(crate) const RESTORE_COWORK_SERVICE_FUNCTION: &str = r#"
function Restore-AssistantCoworkService {
    param([string]$ExpectedPath)
    $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
    if (-not $service -or $service.PathName.Trim().Trim('"') -ne $ExpectedPath) { throw 'Cowork 服务归属已变化，无法恢复。' }
    if ($service.State -eq 'Running') { return }
    Start-Service -Name CoworkVMService -ErrorAction Stop
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    do {
        $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
        if ($service -and $service.State -eq 'Running') { Write-Output '已恢复 CoworkVMService'; return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw '恢复 CoworkVMService 运行状态超时。'
}
"#;

fn run_engine(
    engine: &Engine,
    script: &Path,
    package: &ClaudePackage,
    action: &str,
    mode: &str,
    pending: Option<&PendingPatch>,
    operation: &OperationState,
) -> Result<String, String> {
    let work = engine.root.parent().ok_or("引擎工作目录无效。")?;
    let wrapper = work.join(format!("run-{action}.ps1"));
    let record_path = pending_path()?;
    let script_content = engine_wrapper(
        script,
        package,
        action,
        mode,
        pending.map(|p| (p, record_path.as_path())),
    )?;
    util::write_script(&wrapper, &script_content)?;
    util::run_elevated_script_with_logs(&wrapper, work, |chunk| operation.log(chunk))
}

fn engine_wrapper(
    script: &Path,
    package: &ClaudePackage,
    action: &str,
    mode: &str,
    pending: Option<(&PendingPatch, &Path)>,
) -> Result<String, String> {
    let caller_sid =
        util::powershell("[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value")?;
    if caller_sid.is_empty() {
        return Err("无法确认原调用用户 SID。".into());
    }
    let record_pending = if let Some((pending, path)) = pending {
        // Use the original user's absolute path even when UAC uses another account.
        // CreateNew refuses to overwrite another unfinished transaction.
        format!(
            r#"
            $record=[System.IO.File]::Open({path}, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {{
                $bytes=[System.Text.Encoding]::UTF8.GetBytes({json})
                $record.Write($bytes, 0, $bytes.Length)
            }} finally {{ $record.Dispose() }}
        "#,
            path = ps_path(path),
            json = ps_quote(serde_json::to_string(pending).map_err(|e| e.to_string())?)
        )
    } else {
        String::new()
    };
    let expected_root = package.root();
    let script_content = format!(
        r#"$ErrorActionPreference='Stop'
        [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
        $OutputEncoding = [Console]::OutputEncoding
        $sid={sid}; $expected={expected}; $full={full}; $app=Join-Path $expected 'app'
        $registered=@(Get-AppxPackage -User $sid -Name Claude -ErrorAction Stop | Where-Object {{ $_.PackageFullName -eq $full -and $_.InstallLocation -eq $expected }})
        if ($registered.Count -ne 1) {{ throw '提权后无法确认原调用用户的 Claude Appx 包。' }}
        if (-not (Test-Path -LiteralPath (Join-Path $app 'Claude.exe'))) {{ throw 'Claude 目标文件不存在。' }}
        & takeown.exe /F $app /A /R /D Y | Out-Null
        if ($LASTEXITCODE -ne 0) {{ throw '获取 Claude app 写权限失败。' }}
        & icacls.exe $app /grant ('*' + $sid + ':(OI)(CI)M') /T /C /Q | Out-Null
        if ($LASTEXITCODE -ne 0) {{ throw '设置 Claude app 写权限失败。' }}
        $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
        $restoreCowork=($service -and $service.State -eq 'Running' -and $service.PathName.Trim().Trim('"') -eq {service_path})
        $failure=$null
        try {{
            {record_pending}
            & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {script} {action} 'zh-CN' -PatchMode {mode} -OriginalUserSid $sid -OriginalUserProfile {profile} -OriginalAppData {roaming} -OriginalLocalAppData {local}
            if ($LASTEXITCODE -ne 0) {{ throw "汉化脚本退出码 $LASTEXITCODE" }}
        }} catch {{ $failure=$_ }} finally {{
            if ($restoreCowork) {{
                try {{
                    {restore_service}
                    Restore-AssistantCoworkService {service_path}
                }} catch {{
                    if ($failure) {{ throw ("$failure；恢复 Cowork 服务也失败：$_") }}
                    throw
                }}
            }}
        }}
        if ($failure) {{ throw $failure }}
    "#,
        restore_service = RESTORE_COWORK_SERVICE_FUNCTION,
        service_path = ps_path(&package.resources().join("cowork-svc.exe")),
        sid = ps_quote(caller_sid),
        expected = ps_path(&expected_root),
        full = ps_quote(&package.package_full_name),
        script = ps_path(script),
        action = ps_quote(action),
        mode = ps_quote(mode),
        profile = ps_quote(env::var("USERPROFILE").map_err(|_| "无法读取当前用户配置目录。")?),
        roaming = ps_quote(env::var("APPDATA").map_err(|_| "无法读取当前用户 AppData。")?),
        local = ps_quote(env::var("LOCALAPPDATA").map_err(|_| "无法读取当前用户 LocalAppData。")?),
    );
    Ok(script_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_record_starts_only_after_preflight_and_survives_engine_failure() {
        let root = env::temp_dir().join(format!("issue-2-regression-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let package = ClaudePackage {
            package_full_name: "fixture".into(),
            version: "1".into(),
            install_location: root.join("package").display().to_string(),
            package_family_name: "fixture".into(),
            architecture: "X64".into(),
        };
        let pending = PendingPatch {
            package_full_name: package.package_full_name.clone(),
            version: package.version.clone(),
            install_location: package.install_location.clone(),
            engine_sha256: "fixture".into(),
        };
        fs::create_dir_all(package.exe().parent().unwrap()).unwrap();
        fs::write(package.exe(), "fixture").unwrap();
        let record = root.join("pending.json");
        let wrapper = engine_wrapper(
            &root.join("unused.ps1"),
            &package,
            "install",
            "safe",
            Some((&pending, &record)),
        )
        .unwrap();
        for (stage, expected, recorded) in [
            ("identity", "提权后无法确认", false),
            ("takeown", "获取 Claude app 写权限失败", false),
            ("icacls", "设置 Claude app 写权限失败", false),
            ("engine", "fixture engine failed", true),
        ] {
            // Run the real wrapper with only OS mutations and the engine replaced.
            // It never touches an installed package or invokes UAC.
            let mocks = format!(
                r#"
                $stage={stage}
                function Get-AppxPackage {{ if ($stage -ne 'identity') {{ [pscustomobject]@{{ PackageFullName='fixture'; InstallLocation={location} }} }} }}
                function takeown.exe {{ $global:LASTEXITCODE=[int]($stage -eq 'takeown') }}
                function icacls.exe {{ $global:LASTEXITCODE=[int]($stage -eq 'icacls') }}
                function Get-CimInstance {{ }}
                function powershell.exe {{
                    if (-not (Test-Path -LiteralPath {record})) {{ throw 'missing transaction record before engine writes' }}
                    throw 'fixture engine failed'
                }}
            "#,
                stage = ps_quote(stage),
                location = ps_quote(&package.install_location),
                record = ps_path(&record)
            );
            let script = root.join("wrapper.ps1");
            util::write_script(&script, &format!("{mocks}\n{wrapper}")).unwrap();
            let error = util::run_script(&script).unwrap_err();
            assert!(error.contains(expected), "{stage}: {error}");
            assert_eq!(record.exists(), recorded, "{stage}");
        }
        let saved: PendingPatch = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        assert_eq!(saved.install_location, package.install_location);
        assert_eq!(saved.engine_sha256, pending.engine_sha256);
        // An existing recovery record must never be overwritten.
        fs::write(&record, "previous transaction").unwrap();
        assert!(util::run_script(&root.join("wrapper.ps1")).is_err());
        assert_eq!(fs::read_to_string(&record).unwrap(), "previous transaction");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "Downloads the live engine twice in an isolated process; no Claude writes"]
    fn live_engine_cache_reuses_validated_archive() {
        let root = env::temp_dir().join(format!("engine-cache-live-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let original = env::var_os("LOCALAPPDATA");
        env::set_var("LOCALAPPDATA", &root);
        let operation = OperationState::new();
        let first = fetch_engine(&operation).unwrap();
        let second = fetch_engine(&operation).unwrap();
        assert_eq!(first.sha256, second.sha256);
        assert_ne!(first.root, second.root);
        assert!(operation
            .snapshot()
            .logs
            .iter()
            .any(|line| line.contains("复用本地缓存")));
        println!("{}", operation.snapshot().logs.join("\n"));
        if let Some(original) = original {
            env::set_var("LOCALAPPDATA", original);
        } else {
            env::remove_var("LOCALAPPDATA");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reused_frontend_patch_translates_text_and_effort_without_structural_edits() {
        let root = std::env::temp_dir().join(format!(
            "claude-windows-cn-frontend-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let assets = root.join("ion-dist/assets/v1");
        fs::create_dir_all(&assets).unwrap();
        let input = r#"const a="Hello"; const keep="structural"; const efforts=["low","medium","high","xhigh","max"]; const menu={label:e.name,value:e.id,checked:x===e.id,onSelect:()=>setEffort(e.id,!1)}"#;
        let bundle = assets.join("fixture.js");
        fs::write(&bundle, input).unwrap();
        let script = format!(
            r#"
$ErrorActionPreference='Stop'
$Utf8NoBom=New-Object System.Text.UTF8Encoding $false
function Get-FrontendHardcodedReplacements {{ param($Language); return @(@('Hello','你好'),@('structural','不得替换')) }}
function Test-StructuralJsReplacement {{ param($Source); return ($Source -eq 'structural') }}
function Test-PlainUiTextReplacement {{ param($Source); return $true }}
function Backup-ModifiedFile {{ param($ResourcesPath,$File); Copy-Item -LiteralPath $File -Destination ($File+'.original') }}
{patch}
Patch-HardcodedFrontendStrings {root} 'zh-CN'
"#,
            patch = FAST_HARDCODED_FRONTEND_PATCH_FUNCTION,
            root = ps_path(&root)
        );
        let script_path = root.join("check.ps1");
        util::write_script(&script_path, &script).unwrap();
        let result = util::run_script(&script_path);
        let output = fs::read_to_string(&bundle).unwrap();
        let backup = fs::read_to_string(assets.join("fixture.js.original"));
        fs::remove_dir_all(&root).unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(output.contains(r#"const a="你好""#), "{output}");
        assert!(output.contains(r#"const keep="structural""#));
        assert!(
            output.contains(
                r#"label:({low:"低",medium:"中",high:"高",xhigh:"超高",max:"最高"}[e.id]??e.name)"#
            ),
            "{output}"
        );
        assert_eq!(backup.unwrap(), input);
    }

    #[test]
    fn cowork_restore_rejects_foreign_service_and_preserves_running_state() {
        let script = format!(
            r#"
$ErrorActionPreference='Stop'
$script:state='Stopped'; $script:path='C:\fixture\cowork-svc.exe'; $script:starts=0
function Get-CimInstance {{ [pscustomobject]@{{State=$script:state;PathName=$script:path}} }}
function Start-Service {{ $script:starts++; $script:state='Running' }}
{restore}
Restore-AssistantCoworkService 'C:\fixture\cowork-svc.exe'
Restore-AssistantCoworkService 'C:\fixture\cowork-svc.exe'
if ($script:starts -ne 1) {{ throw 'Expected exactly one service restart' }}
$script:state='Stopped'; $script:path='C:\foreign\cowork-svc.exe'
$rejected=$false
try {{ Restore-AssistantCoworkService 'C:\fixture\cowork-svc.exe' }} catch {{ $rejected=$true }}
if (-not $rejected -or $script:starts -ne 1) {{ throw 'Foreign service must not be started' }}
"#,
            restore = RESTORE_COWORK_SERVICE_FUNCTION
        );
        assert!(util::powershell(&script).is_ok());
    }

    #[test]
    fn engine_adapter_parses_shared_shutdown_and_reused_frontend_function() {
        let root = std::env::temp_dir().join(format!(
            "claude-windows-cn-adapter-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("scripts")).unwrap();
        // A supplied upstream snapshot exercises the same adapter without executing it.
        let source = if let Ok(path) = env::var("CLAUDE_ASSISTANT_ENGINE_FIXTURE") {
            fs::read_to_string(path).unwrap()
        } else {
            let mut source = String::from("\u{feff}param([string]$Fixture = 'ok')\n$script:DetectedUnpackagedClaudePaths = @(Get-UnpackagedClaudePaths)\nEnable-WriteAccess $resourcesPath\n");
            for name in [
                "Find-ClaudePath",
                "Get-ClaudeConfigPaths",
                "Test-ClaudeDesktopProcessPath",
                "Stop-ClaudeProcesses",
                "Disable-FridaResidentForDiskInstall",
                "Remove-LegacyAppxForkArtifacts",
                "Repair-ClaudeDesktopShortcut",
                "Start-CoworkVMService",
                "Restart-Claude",
                "Invoke-PreInstallCleanup",
                "Restore-LatestBackup",
                "Uninstall-WindowsLanguagePack",
                "Patch-HardcodedFrontendStrings",
            ] {
                source.push_str(&format!("function {name} {{ }}\n"));
            }
            source
        };
        let script = root.join("scripts/install_windows.ps1");
        fs::write(&script, source).unwrap();
        let engine = Engine {
            root: root.clone(),
            script,
            sha256: String::new(),
        };
        let package = ClaudePackage {
            version: "1.0.0.0".into(),
            package_full_name: "Claude_1.0.0.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: "Claude_pzs8sxrjxfjjc".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.0.0.0_x64__pzs8sxrjxfjjc"
                .into(),
            architecture: "X64".into(),
        };
        let adapted = adapt_engine(&engine, &package).unwrap();
        let output = fs::read_to_string(adapted).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(!output.contains("Enable-WriteAccess $resourcesPath"));
        assert!(output.contains("function Stop-ClaudeProcesses {\n$ErrorActionPreference"));
        assert!(output.contains("GetOwnerSid"));
        assert!(output.contains("__effortLabelReplacementCount"));
    }

    #[test]
    #[ignore = "Downloads latest engine and patches an isolated official MSIX copy; run alone"]
    fn official_msix_isolated_patch_roundtrip() {
        let msix = PathBuf::from(
            env::var("CLAUDE_ASSISTANT_MSIX_FIXTURE").expect("MSIX fixture required"),
        );
        let reviewed = fs::read_to_string(
            env::var("CLAUDE_ASSISTANT_ENGINE_FIXTURE").expect("Reviewed engine snapshot required"),
        )
        .unwrap();
        let root = env::temp_dir().join(format!(
            "claude-windows-cn-roundtrip-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        println!("ISOLATED_EVIDENCE={}", root.display());
        for (key, directory) in [
            ("APPDATA", "roaming"),
            ("LOCALAPPDATA", "local"),
            ("USERPROFILE", "profile"),
        ] {
            let directory = root.join(directory);
            fs::create_dir_all(&directory).unwrap();
            env::set_var(key, directory);
        }
        env::set_var("CLAUDE_ZH_SKIP_UPDATE_CHECK", "1");
        let package_root = root.join("official-package");
        util::powershell(&format!(r#"
$ErrorActionPreference='Stop'
if ((Get-AuthenticodeSignature -LiteralPath {msix}).Status -ne 'Valid') {{ throw 'MSIX signature invalid' }}
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::ExtractToDirectory({msix}, {target})
"#, msix=ps_path(&msix), target=ps_path(&package_root))).unwrap();
        let package = ClaudePackage {
            version: "2.7032.0.0".into(),
            architecture: "X64".into(),
            package_full_name: "Claude_2.7032.0.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: "Claude_pzs8sxrjxfjjc".into(),
            install_location: package_root.display().to_string(),
        };
        let original_exe = util::sha256(&package.exe()).unwrap();
        let original_asar = util::sha256(&package.resources().join("app.asar")).unwrap();
        let operation = OperationState::new();
        let mut evidence = Vec::new();
        let mut manifest = None;
        for (index, (action, mode)) in [
            ("install", "safe"),
            ("uninstall", "safe"),
            ("install", "official"),
            ("uninstall", "safe"),
        ]
        .into_iter()
        .enumerate()
        {
            let engine = fetch_engine(&operation).unwrap();
            let source = fs::read_to_string(&engine.script).unwrap();
            assert_eq!(
                source.trim_start_matches('\u{feff}').replace("\r\n", "\n"),
                reviewed
                    .trim_start_matches('\u{feff}')
                    .replace("\r\n", "\n"),
                "Latest engine changed; review its side effects before executing"
            );
            let script = adapt_engine(&engine, &package).unwrap();
            let content = fs::read_to_string(&script).unwrap().replace(
                "Global\\ClaudeDesktopZhCn-Installer",
                &format!("Local\\ClaudeWindowsCN-Isolated-{}", std::process::id()),
            );
            util::write_script(&script, content.trim_start_matches('\u{feff}')).unwrap();
            let wrapper = root.join(format!("isolated-{index}-{action}-{mode}.ps1"));
            util::write_script(&wrapper, &format!(r#"
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object System.Text.UTF8Encoding $false
function Get-AppxPackage {{ [pscustomobject]@{{PackageFullName={full}; InstallLocation={target}}} }}
function Get-CimInstance {{ param($ClassName,$Filter); if ($ClassName -notin @('Win32_Process','Win32_Service')) {{ throw 'Unexpected CIM query' }} }}
function Invoke-CimMethod {{ throw 'Forbidden process mutation in isolated test' }}
function Get-Service {{ throw 'Forbidden service access in isolated test' }}
function Start-Service {{ throw 'Forbidden service start in isolated test' }}
function Stop-Service {{ throw 'Forbidden service stop in isolated test' }}
function Start-Process {{ throw 'Forbidden process start in isolated test' }}
function Stop-Process {{ throw 'Forbidden process stop in isolated test' }}
function Set-Acl {{ throw 'Forbidden ACL mutation in isolated test' }}
function New-ItemProperty {{ throw 'Forbidden registry mutation in isolated test' }}
function Remove-ItemProperty {{ throw 'Forbidden registry mutation in isolated test' }}
function Add-AppxPackage {{ throw 'Forbidden registration in isolated test' }}
function Remove-AppxPackage {{ throw 'Forbidden registration in isolated test' }}
function taskkill {{ throw 'Forbidden process kill in isolated test' }}
& {script} {action} 'zh-CN' -PatchMode {mode}
exit $LASTEXITCODE
"#, full=ps_quote(&package.package_full_name), target=ps_path(&package_root), script=ps_path(&script), action=ps_quote(action), mode=ps_quote(mode))).unwrap();
            let result = util::run_script(&wrapper);
            fs::write(
                root.join(format!("{index}-{action}-{mode}.log")),
                result
                    .as_ref()
                    .map(String::as_str)
                    .unwrap_or_else(|error| error.as_str()),
            )
            .unwrap();
            assert!(
                result.is_ok(),
                "step {index}: {result:?}; evidence={}",
                root.display()
            );
            if action == "install" {
                let set = unique_backup_set(&package.resources().join(".zh-cn-backups")).unwrap();
                let files = collect_backup_files(&set).unwrap();
                assert!(!files.is_empty());
                assert!(package
                    .resources()
                    .join("ion-dist/i18n/zh-CN.json")
                    .is_file());
                manifest = Some(BackupManifest {
                    package_full_name: package.package_full_name.clone(),
                    version: package.version.clone(),
                    install_location: package.install_location.clone(),
                    backup_set: set.display().to_string(),
                    files,
                    applied_mode: Some(mode.into()),
                    engine_sha256: engine.sha256.clone(),
                });
                validate_backup(manifest.as_ref().unwrap(), &package).unwrap();
            } else {
                verify_restored(manifest.as_ref().unwrap(), &package).unwrap();
            }
            let exe = util::sha256(&package.exe()).unwrap();
            let asar = util::sha256(&package.resources().join("app.asar")).unwrap();
            if action == "uninstall" || mode == "safe" {
                assert_eq!(exe, original_exe);
                assert_eq!(asar, original_asar);
            } else {
                assert_ne!(exe, original_exe);
                assert_ne!(asar, original_asar);
            }
            let locale: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(root.join("roaming/Claude/config.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                locale["locale"],
                if action == "install" {
                    "zh-CN"
                } else {
                    "en-US"
                }
            );
            evidence.push(serde_json::json!({"step":index,"action":action,"mode":mode,"engineSha256":engine.sha256,"exeSha256":exe,"asarSha256":asar,"backupFiles":manifest.as_ref().unwrap().files.len(),"locale":locale["locale"],"ok":true}));
            fs::write(
                root.join("results.json"),
                serde_json::to_vec_pretty(&evidence).unwrap(),
            )
            .unwrap();
            println!("ISOLATED_STEP_OK {index} {action} {mode}");
        }
    }

    #[test]
    fn backup_path_cannot_escape_package() {
        assert!(validate_relative("..\\other").is_err());
        assert!(validate_relative(r"C:\Windows\system.ini").is_err());
        assert!(validate_relative("ion-dist\\assets\\x.js").is_ok());
    }

    #[test]
    fn script_adapter_requires_existing_function() {
        assert!(replace_function("function X { return 1 }", "Y", "function Y {}").is_err());
        let source = "function Stop-ClaudeProcessesGracefully { return 1 }\nfunction Stop-ClaudeProcesses { return 2 }";
        let adapted = replace_function(
            source,
            "Stop-ClaudeProcesses",
            "function Stop-ClaudeProcesses { return 3 }",
        )
        .unwrap();
        assert!(adapted.contains("function Stop-ClaudeProcessesGracefully { return 1 }"));
        assert!(!adapted.contains("return 2"));
        assert!(adapted.contains("return 3"));
    }

    #[test]
    fn detects_existing_localization_assets() {
        let root = std::env::temp_dir().join(format!(
            "claude-windows-cn-assets-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        assert!(!has_localization_assets(&root));
        fs::create_dir(root.join(".zh-cn-backups")).unwrap();
        assert!(has_localization_assets(&root));
        assert!(unique_backup_set(&root.join(".zh-cn-backups")).is_err());
        fs::create_dir(root.join(".zh-cn-backups").join("only-set")).unwrap();
        assert!(unique_backup_set(&root.join(".zh-cn-backups")).is_ok());
        fs::create_dir(root.join(".zh-cn-backups").join("unknown-set")).unwrap();
        assert!(unique_backup_set(&root.join(".zh-cn-backups")).is_err());
        fs::remove_dir_all(root.join(".zh-cn-backups")).unwrap();
        let i18n = root.join("ion-dist").join("i18n");
        fs::create_dir_all(&i18n).unwrap();
        fs::write(i18n.join("zh-CN.json"), "{}").unwrap();
        assert!(has_localization_assets(&root));
        fs::remove_dir_all(root).unwrap();
    }
}
