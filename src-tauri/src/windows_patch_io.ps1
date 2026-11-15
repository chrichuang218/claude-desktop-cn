function Invoke-AssistantFileRetry {
    param([scriptblock]$Action, [string]$Path, [int]$TimeoutSeconds = 20)

    $timer = [Diagnostics.Stopwatch]::StartNew()
    $reported = $false
    while ($true) {
        try { return (& $Action) }
        catch {
            $errorRecord = $_
            $exception = $_.Exception
            while ($exception.InnerException) { $exception = $exception.InnerException }
            $code = $exception.HResult -band 65535
            # Only sharing/lock violations are transient; permissions and invalid data must fail.
            if ($code -notin @(32, 33)) { throw $errorRecord }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
                throw "等待文件占用释放超时（$TimeoutSeconds 秒）：$Path；$($exception.Message)"
            }
            if (-not $reported) {
                Write-Host "  文件暂被占用，正在等待释放：$Path"
                $reported = $true
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

function Sync-ClaudeExeAsarIntegrity {
    param([string]$ResourcesPath)

    $exePath = Get-ClaudeExePathFromResources $ResourcesPath
    $exeText = Invoke-AssistantFileRetry -Path $exePath -Action {
        [IO.File]::ReadAllText($exePath, [Text.Encoding]::ASCII)
    }
    $updates = @()
    foreach ($asarName in @('app.asar', 'ion-dist.asar')) {
        $asarPath = Join-Path $ResourcesPath $asarName
        if (-not (Test-Path -LiteralPath $asarPath -PathType Leaf)) { continue }
        $headerHash = Get-AsarHeaderHash $asarPath
        $marker = Get-ClaudeExeIntegrityMarker -ExeText $exeText -AsarName $asarName
        $index = $exeText.IndexOf($marker, [StringComparison]::Ordinal)
        $offset = $index + $marker.Length
        if ($index -lt 0 -or ($offset + 64) -gt $exeText.Length) {
            throw "Claude.exe $asarName integrity marker has invalid bounds."
        }
        $currentHash = $exeText.Substring($offset, 64)
        if ($currentHash -notmatch '^[0-9a-fA-F]{64}$' -or $headerHash -notmatch '^[0-9a-fA-F]{64}$') {
            throw "Claude.exe $asarName integrity value is not a SHA256 hex string."
        }
        if ($currentHash -eq $headerHash) {
            Write-Host "  Claude.exe $asarName integrity already matches"
            continue
        }
        $updates += [pscustomobject]@{ Name=$asarName; Offset=$offset; Before=$currentHash; After=$headerHash }
    }
    if ($updates.Count -eq 0) { return }

    Invoke-AssistantFileRetry -Path $exePath -Action { Backup-AppFile $ResourcesPath $exePath }
    # One handle for all changed hashes avoids a scanner taking the file between writes.
    $stream = Invoke-AssistantFileRetry -Path $exePath -Action {
        [IO.File]::Open($exePath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read)
    }
    try {
        foreach ($update in $updates) {
            $bytes = [byte[]]::new(64)
            $stream.Seek([int64]$update.Offset, [IO.SeekOrigin]::Begin) | Out-Null
            if ($stream.Read($bytes, 0, 64) -ne 64 -or [Text.Encoding]::ASCII.GetString($bytes) -ne $update.Before) {
                throw "Claude.exe $($update.Name) integrity changed before write."
            }
        }
        foreach ($update in $updates) {
            $bytes = [Text.Encoding]::ASCII.GetBytes($update.After)
            $stream.Seek([int64]$update.Offset, [IO.SeekOrigin]::Begin) | Out-Null
            $stream.Write($bytes, 0, $bytes.Length)
        }
        $stream.Flush($true)
        foreach ($update in $updates) {
            $bytes = [byte[]]::new(64)
            $stream.Seek([int64]$update.Offset, [IO.SeekOrigin]::Begin) | Out-Null
            if ($stream.Read($bytes, 0, 64) -ne 64 -or [Text.Encoding]::ASCII.GetString($bytes) -ne $update.After) {
                throw "Claude.exe $($update.Name) integrity verification failed."
            }
            Write-Host "  updated Claude.exe $($update.Name) integrity: $($update.Before) -> $($update.After)"
        }
    }
    finally { $stream.Dispose() }
}
