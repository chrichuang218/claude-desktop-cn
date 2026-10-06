# Included by the Rust regression test. Only uniquely named fixture jobs and
# processes created here may be changed; the installed Claude package is mocked.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class ClaudeJobFixture {
    [StructLayout(LayoutKind.Sequential)]
    struct UnicodeString { public ushort Length, MaximumLength; public IntPtr Buffer; }
    [StructLayout(LayoutKind.Sequential)]
    struct ObjectAttributes {
        public int Length; public IntPtr RootDirectory, ObjectName; public uint Attributes;
        public IntPtr SecurityDescriptor, SecurityQualityOfService;
    }
    [DllImport("ntdll.dll")]
    static extern int NtCreateJobObject(out IntPtr job, uint access, ref ObjectAttributes attributes);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern bool TerminateJobObject(IntPtr job, uint code);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr handle);
    public static IntPtr Create(string package, string sid) {
        string name="\\Container_"+package+"-"+sid;
        var text=new UnicodeString { Length=(ushort)(name.Length*2) };
        text.MaximumLength=text.Length; text.Buffer=Marshal.StringToHGlobalUni(name);
        IntPtr pointer=Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UnicodeString)));
        try {
            Marshal.StructureToPtr(text,pointer,false);
            var attributes=new ObjectAttributes { Length=Marshal.SizeOf(typeof(ObjectAttributes)),ObjectName=pointer };
            IntPtr job; int status=NtCreateJobObject(out job,0x1f003f,ref attributes);
            if(status<0) throw new Exception("NtCreateJobObject: "+status.ToString("X8"));
            return job;
        } finally { Marshal.FreeHGlobal(pointer); Marshal.FreeHGlobal(text.Buffer); }
    }
    public static void Assign(IntPtr job, IntPtr process) {
        if(!AssignProcessToJobObject(job,process)) throw new Win32Exception(Marshal.GetLastWin32Error());
    }
}
'@

$children = @()
$jobs = @()
function New-FixtureChild {
    $child = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList '-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 120"' -PassThru -WindowStyle Hidden
    $script:children += $child
    return $child
}
try {
    # Its launching PowerShell exits; only this orphan remains in the fixture Job.
    $launcher = '$child=Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList ''-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 120"'' -PassThru -WindowStyle Hidden; $child.Id'
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($launcher))
    $orphanId = [int](& "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -EncodedCommand $encoded)
    $orphan = Get-Process -Id $orphanId
    $children += $orphan
    $outsider = New-FixtureChild
    $otherPackageChild = New-FixtureChild
    $otherUserChild = New-FixtureChild

    $job = [ClaudeJobFixture]::Create($fixturePackage, $fixtureSid)
    $jobs += $job
    [ClaudeJobFixture]::Assign($job, $orphan.Handle)
    $otherPackage = $fixturePackage + '_other'
    $otherJob = [ClaudeJobFixture]::Create($otherPackage, $fixtureSid)
    $jobs += $otherJob
    [ClaudeJobFixture]::Assign($otherJob, $otherPackageChild.Handle)
    $wrongSid = 'S-1-5-21-111111111-222222222-333333333-4444'
    $wrongOwnerJob = [ClaudeJobFixture]::Create($fixturePackage, $wrongSid)
    $jobs += $wrongOwnerJob
    [ClaudeJobFixture]::Assign($wrongOwnerJob, $otherUserChild.Handle)

    [ClaudeDesktopJob]::Close($fixturePackage, $fixtureSid, ($fixtureSession + 1))
    if ($orphan.HasExited) { throw 'Another session was terminated' }
    [ClaudeDesktopJob]::Close($fixturePackage, $wrongSid, $fixtureSession)
    if ($otherUserChild.HasExited) { throw 'Another user was terminated' }
    [ClaudeDesktopJob]::Close($fixturePackage + '_missing', $fixtureSid, $fixtureSession)

    # A stale/reused PID from enumeration must not terminate a nonmember.
    $stopMember = [ClaudeDesktopJob].GetMethod('StopMember', [Reflection.BindingFlags]'NonPublic,Static')
    $stopped = $stopMember.Invoke($null, @($job, [uint32]$outsider.Id, $fixtureSid, $fixtureSession))
    if ($stopped -or $outsider.HasExited) { throw 'A nonmember PID was terminated' }

    # No Claude.exe exists in the mock, so the old name/path-only flow leaves
    # the orphan alive. Run the complete production close script, not a copy.
    & $legacyCloseFixture | Out-Null
    if ($orphan.HasExited) { throw 'Fixture no longer reproduces the original orphan bug' }
    & $closeFixture | Out-Null
    if (-not $orphan.WaitForExit(5000)) { throw 'Claude Job orphan survived close_desktop_script' }
    foreach ($untouched in @($outsider, $otherPackageChild, $otherUserChild)) {
        if ($untouched.HasExited) { throw 'An unrelated process was terminated' }
    }
    & $closeFixture | Out-Null # Empty job is safe to repeat.
    'PASS'
} finally {
    foreach ($job in $jobs) {
        [ClaudeJobFixture]::TerminateJobObject($job, 1) | Out-Null
        [ClaudeJobFixture]::CloseHandle($job) | Out-Null
    }
    foreach ($child in $children) {
        if (-not $child.HasExited) { $child.Kill(); $child.WaitForExit() }
        $child.Dispose()
    }
}
