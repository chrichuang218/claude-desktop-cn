using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Threading;

// Appinfo keeps Desktop descendants in this job even after Claude.exe exits.
// Package identity and executable-path queries do not find all such descendants.
public static class ClaudeDesktopJob
{
    [StructLayout(LayoutKind.Sequential)]
    struct UnicodeString { public ushort Length, MaximumLength; public IntPtr Buffer; }
    [StructLayout(LayoutKind.Sequential)]
    struct ObjectAttributes
    {
        public int Length;
        public IntPtr RootDirectory, ObjectName;
        public uint Attributes;
        public IntPtr SecurityDescriptor, SecurityQualityOfService;
    }

    [DllImport("ntdll.dll")]
    static extern int NtOpenJobObject(out IntPtr job, uint access, ref ObjectAttributes attributes);
    [DllImport("ntdll.dll")]
    static extern uint RtlNtStatusToDosError(int status);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool QueryInformationJobObject(IntPtr job, int info, IntPtr buffer, uint size, out uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool member);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int info, out uint value, uint size, out uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    static void Check(bool success)
    {
        if (!success) throw new Win32Exception(Marshal.GetLastWin32Error());
    }

    static IntPtr Open(string name)
    {
        var text = new UnicodeString { Length = checked((ushort)(name.Length * 2)) };
        text.MaximumLength = text.Length;
        text.Buffer = Marshal.StringToHGlobalUni(name);
        IntPtr pointer = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UnicodeString)));
        try
        {
            Marshal.StructureToPtr(text, pointer, false);
            var attributes = new ObjectAttributes {
                Length = Marshal.SizeOf(typeof(ObjectAttributes)), ObjectName = pointer,
                Attributes = 0x40 // OBJ_CASE_INSENSITIVE; absolute NT path, not BaseNamedObjects.
            };
            IntPtr job;
            int status = NtOpenJobObject(out job, 4 /* JOB_OBJECT_QUERY */, ref attributes);
            if (status == unchecked((int)0xC0000034) || status == unchecked((int)0xC000003A))
                return IntPtr.Zero; // No container job on this Windows/package version.
            if (status < 0) throw new Win32Exception((int)RtlNtStatusToDosError(status));
            return job;
        }
        finally { Marshal.FreeHGlobal(pointer); Marshal.FreeHGlobal(text.Buffer); }
    }

    static uint[] Members(IntPtr job)
    {
        for (int size = 4096; size <= 1048576; size *= 2)
        {
            IntPtr buffer = Marshal.AllocHGlobal(size);
            try
            {
                uint length;
                if (!QueryInformationJobObject(job, 3 /* BasicProcessIdList */, buffer, (uint)size, out length))
                {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 234) continue; // Children can be added during enumeration.
                    throw new Win32Exception(error);
                }
                int count = Marshal.ReadInt32(buffer, 4);
                if (count < 0 || count > (size - 8) / IntPtr.Size)
                    throw new InvalidOperationException("Claude Job 进程列表无效。");
                var ids = new uint[count];
                for (int i = 0; i < count; i++)
                    ids[i] = checked((uint)Marshal.ReadIntPtr(buffer, 8 + i * IntPtr.Size).ToInt64());
                return ids;
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        throw new InvalidOperationException("Claude Job 进程列表过大，无法安全关闭。");
    }

    static bool StopMember(IntPtr job, uint pid, string sid, uint session)
    {
        // Hold one process handle through identity checks and termination: a reused
        // PID must never redirect the termination to an unrelated process.
        IntPtr process = OpenProcess(0x101001 /* SYNCHRONIZE | QUERY_LIMITED_INFORMATION | TERMINATE */, false, pid);
        if (process == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == 87) return false; // Already exited.
            throw new Win32Exception(error, "无法检查 Claude Job 残留进程 PID=" + pid);
        }
        try
        {
            if (WaitForSingleObject(process, 0) == 0) return false;
            bool member;
            Check(IsProcessInJob(process, job, out member));
            if (!member) return false;
            IntPtr token;
            Check(OpenProcessToken(process, 8 /* TOKEN_QUERY */, out token));
            try
            {
                uint tokenSession, length;
                Check(GetTokenInformation(token, 12 /* TokenSessionId */, out tokenSession, 4, out length));
                using (var identity = new WindowsIdentity(token))
                    if (tokenSession != session || identity.User.Value != sid) return false;
            }
            finally { CloseHandle(token); }
            Console.WriteLine("结束官方 Claude Job 残留进程 PID=" + pid);
            if (!TerminateProcess(process, 1))
            {
                int error = Marshal.GetLastWin32Error();
                if (WaitForSingleObject(process, 0) != 0)
                    throw new Win32Exception(error, "无法结束 Claude Job 残留进程 PID=" + pid);
            }
            return true;
        }
        catch (Win32Exception)
        {
            // A child can exit between any two queries. Only a signalled handle
            // proves that this exact process is gone; never swallow live errors.
            if (WaitForSingleObject(process, 0) == 0) return false;
            throw;
        }
        finally { CloseHandle(process); }
    }

    public static void Close(string packageFullName, string sid, uint session)
    {
        IntPtr job = Open("\\Container_" + packageFullName + "-" + sid);
        if (job == IntPtr.Zero) return;
        try
        {
            bool self;
            Check(IsProcessInJob(GetCurrentProcess(), job, out self));
            if (self) throw new InvalidOperationException("助手运行在 Claude Job 内，请退出助手并从开始菜单或资源管理器重新打开。");
            DateTime deadline = DateTime.UtcNow.AddSeconds(5);
            do
            {
                bool stopped = false;
                foreach (uint pid in Members(job)) stopped |= StopMember(job, pid, sid, session);
                if (!stopped) return;
                Thread.Sleep(100);
            } while (DateTime.UtcNow < deadline);
            throw new InvalidOperationException("等待 Claude Job 残留进程退出超时，请重试并查看日志。");
        }
        finally { CloseHandle(job); }
    }
}
