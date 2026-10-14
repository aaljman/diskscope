# Runs a program (diskscope, for tests) with a real, fully rendered window on a separate Windows
# desktop that is never shown. It can't take your screen, focus or mouse: the desktop is
# created with CreateDesktop and the process is started on it through STARTUPINFO.lpDesktop;
# SwitchDesktop is never called. The process is killed after -Seconds.
#
# Usage (pass the program's arguments as a list after -Args; no argument may contain a comma):
#   powershell -File tools\run_hidden.ps1 -Exe target\debug\diskscope.exe `
#       -Args '--demo','--shot','C:\tmp\a.png','--after','2' -Seconds 30
#   -Cwd <dir>     working directory (default: the exe's folder)
# Exit code: the program's exit code, 124 when it was killed at the time limit, negative for a
# launch failure (-Win32 error).
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string[]]$Args = @(),
    [int]$Seconds = 60,
    [string]$Cwd = ""
)

$Exe = (Resolve-Path $Exe).Path
if ($Cwd -eq "") { $Cwd = Split-Path $Exe -Parent }

# A cursor clip is system-wide even from another desktop. diskscope never clips, but be safe:
# release any clip that appears while the child runs.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class HiddenRun {
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateDesktop(string name, IntPtr device, IntPtr devmode, int flags, uint access, IntPtr sa);
    [DllImport("user32.dll", SetLastError = true)] static extern bool CloseDesktop(IntPtr desktop);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2; public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcess(string app, System.Text.StringBuilder cmd, IntPtr pa, IntPtr ta, bool inherit,
        uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr h, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] static extern bool GetClipCursor(out RECT r);
    [DllImport("user32.dll")] static extern bool ClipCursor(IntPtr r);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);

    // Whole-screen clip is the normal state; anything smaller that appears while our child runs
    // and was not there before is released.
    static bool Clipped(out RECT r) {
        if (!GetClipCursor(out r)) return false;
        int vw = GetSystemMetrics(78), vh = GetSystemMetrics(79); // virtual screen
        return (r.R - r.L) < vw || (r.B - r.T) < vh;
    }

    public static int Run(string desktop, string app, string cmd, string cwd, uint timeoutMs) {
        // GENERIC_ALL on the new desktop; it exists only while a handle is open
        IntPtr desk = CreateDesktop(desktop, IntPtr.Zero, IntPtr.Zero, 0, 0x10000000, IntPtr.Zero);
        if (desk == IntPtr.Zero) return -Marshal.GetLastWin32Error() - 100000;
        try {
            RECT before;
            bool clippedBefore = Clipped(out before);
            STARTUPINFO si = new STARTUPINFO();
            si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
            si.lpDesktop = desktop;
            PROCESS_INFORMATION pi;
            if (!CreateProcess(app, new System.Text.StringBuilder(cmd), IntPtr.Zero, IntPtr.Zero, false, 0,
                    IntPtr.Zero, cwd, ref si, out pi))
                return -Marshal.GetLastWin32Error();
            int result = -1;
            long waited = 0;
            while (true) {
                if (WaitForSingleObject(pi.hProcess, 50) == 0) break;
                waited += 50;
                RECT r;
                if (!clippedBefore && Clipped(out r)) ClipCursor(IntPtr.Zero);
                if (waited >= timeoutMs) { TerminateProcess(pi.hProcess, 124); result = 124; break; }
            }
            WaitForSingleObject(pi.hProcess, 5000);
            uint code;
            GetExitCodeProcess(pi.hProcess, out code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return result >= 0 ? result : (int)code;
        } finally {
            CloseDesktop(desk);
        }
    }
}
"@

function Quote([string]$s) { if ($s -eq "" -or $s -match '[\s"]') { '"' + ($s -replace '"', '\"') + '"' } else { $s } }
# With `powershell -File`, a list like '--demo','--shot','a.png' arrives as one string joined by
# commas, so split on commas (program arguments here never contain one).
$list = @($Args | Where-Object { $_ -ne $null } | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne "" })
$parts = @(Quote $Exe)
$parts += @($list | ForEach-Object { Quote $_ })
$cmd = $parts -join " "

Write-Host "run_hidden: $cmd"
$name = [IO.Path]::GetFileNameWithoutExtension($Exe)
Write-Host "run_hidden: $name for up to $Seconds s on a hidden desktop"
$code = [HiddenRun]::Run("run_hidden_$PID", $Exe, $cmd, $Cwd, [uint32]($Seconds * 1000))
if ($code -eq 124) { Write-Host "run_hidden: ended at the $Seconds s limit" }
Write-Host "run_hidden: exit $code"
exit $code
