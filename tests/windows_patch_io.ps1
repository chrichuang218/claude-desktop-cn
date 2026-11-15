param([string]$FunctionsPath)
$ErrorActionPreference = 'Stop'
. ([scriptblock]::Create([IO.File]::ReadAllText($FunctionsPath, [Text.Encoding]::UTF8)))
$root = Join-Path ([IO.Path]::GetTempPath()) ('claude-patch-io-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$script:exe = Join-Path $root 'fixture.bin'
$script:backupCount = 0
function Get-ClaudeExePathFromResources($path) { return $script:exe }
function Get-AsarHeaderHash($path) { return ('b' * 64) }
function Get-ClaudeExeIntegrityMarker($ExeText, $AsarName) { return ($AsarName + ':') }
function Backup-AppFile($resources, $path) { $script:backupCount++ }
function Reset-Fixture { [IO.File]::WriteAllText($script:exe, ('app.asar:' + ('a' * 64) + 'ion-dist.asar:' + ('a' * 64)), [Text.Encoding]::ASCII) }
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Threading;
public static class PatchReadLock {
    public static Thread Hold(string path) {
        var ready = new ManualResetEventSlim();
        var thread = new Thread(() => {
            using (var file = File.Open(path, FileMode.Open, FileAccess.Read, FileShare.Read)) {
                ready.Set();
                Thread.Sleep(750);
            }
        });
        thread.IsBackground = true;
        thread.Start();
        ready.Wait();
        ready.Dispose();
        return thread;
    }
}
'@
try {
    foreach ($name in @('app.asar','ion-dist.asar')) { [IO.File]::WriteAllText((Join-Path $root $name), '') }
    Reset-Fixture
    $thread = [PatchReadLock]::Hold($script:exe)
    Sync-ClaudeExeAsarIntegrity $root
    $thread.Join()
    $expected = 'app.asar:' + ('b' * 64) + 'ion-dist.asar:' + ('b' * 64)
    if ([IO.File]::ReadAllText($script:exe) -ne $expected -or $script:backupCount -ne 1) { throw 'Transient lock did not recover both hashes' }

    # No-op updates must succeed even while another reader denies write access.
    $reader = [IO.File]::Open($script:exe, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        Sync-ClaudeExeAsarIntegrity $root
        if ($script:backupCount -ne 1) { throw 'Unchanged EXE was backed up again' }
        $timedOut = $false
        try {
            Invoke-AssistantFileRetry -Path $script:exe -TimeoutSeconds 0 -Action {
                $file = [IO.File]::Open($script:exe, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read)
                $file.Dispose()
            }
        } catch { $timedOut = $_.Exception.Message.Contains('0 ') -and $_.Exception.Message.Contains($script:exe) }
        if (-not $timedOut) { throw 'Persistent lock did not time out' }
    } finally { $reader.Dispose() }

    $backup = Join-Path $root 'original.bin'
    [IO.File]::WriteAllText($backup, 'original')
    $thread = [PatchReadLock]::Hold($script:exe)
    Invoke-AssistantFileRetry -Path $script:exe -Action {
        Copy-Item -LiteralPath $backup -Destination $script:exe -Force -ErrorAction Stop
    }
    $thread.Join()
    if ([IO.File]::ReadAllText($script:exe) -ne 'original') { throw 'Backup restore did not recover from sharing violation' }

    # A permission failure must never be retried or presented as a transient lock.
    $script:attempts = 0
    try {
        Invoke-AssistantFileRetry -Path $script:exe -Action {
            $script:attempts++
            throw [UnauthorizedAccessException]::new('fixture access denied')
        }
        throw 'Access error was swallowed'
    } catch {
        if ($_.Exception.Message -ne 'fixture access denied' -or $script:attempts -ne 1) { throw }
    }
    Write-Output 'PATCH_IO_OK transient-lock unchanged-hashes bounded-timeout backup-copy permission-error'
} finally {
    if ($thread -and $thread.IsAlive) { $thread.Join() }
    $resolved = [IO.Path]::GetFullPath($root)
    $prefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\claude-patch-io-'
    if (-not $resolved.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unexpected fixture cleanup path' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
