# Per-user install, no admin: Start-menu shortcut + "Analyze with Diskscope" on folders,
# folder backgrounds and drives. Safe to run again (it replaces its own entries).
# -Root / -Shortcut exist for testing against a scratch registry key and folder.
param(
    [string]$Root = "HKCU:\Software\Classes",
    [string]$Shortcut = (Join-Path ([Environment]::GetFolderPath("Programs")) "Diskscope.lnk"),
    [switch]$NoShortcut
)

$exe = Join-Path $PSScriptRoot "diskscope.exe"
if (-not (Test-Path $exe)) { Write-Error "diskscope.exe not found next to this script; run build.ps1 first"; exit 1 }

# The app scans its first non-flag argument. Drive roots go unquoted: "C:\" would end in \" ,
# which the argument parser reads as an escaped quote (paths like C:\ never contain spaces).
$entries = @(
    @{ Key = "Directory\shell\Diskscope";            Arg = '"%1"' },
    @{ Key = "Directory\Background\shell\Diskscope"; Arg = '"%V"' },
    @{ Key = "Drive\shell\Diskscope";                Arg = '%1' }
)
foreach ($e in $entries) {
    $key = Join-Path $Root $e.Key
    if (Test-Path $key) { Remove-Item -Path $key -Recurse -Force }
    New-Item -Path "$key\command" -Force | Out-Null
    Set-ItemProperty -Path $key -Name "(default)" -Value "Analyze with Diskscope"
    Set-ItemProperty -Path $key -Name "Icon" -Value "$exe,0"
    Set-ItemProperty -Path "$key\command" -Name "(default)" -Value "`"$exe`" $($e.Arg)"
}

if (-not $NoShortcut) {
    New-Item -ItemType Directory -Force (Split-Path $Shortcut) | Out-Null
    $lnk = (New-Object -ComObject WScript.Shell).CreateShortcut($Shortcut)
    $lnk.TargetPath = $exe
    $lnk.WorkingDirectory = $PSScriptRoot
    $lnk.IconLocation = "$exe,0"
    $lnk.Description = "Disk usage explorer"
    $lnk.Save()
}

Write-Host "Installed. Start menu: Diskscope. Right-click a folder, a folder's background or a drive: 'Analyze with Diskscope'."
Write-Host "On Windows 11 the entry sits under 'Show more options' (or Shift+F10)."
