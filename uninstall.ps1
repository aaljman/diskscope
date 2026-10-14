# Removes what install.ps1 added: the Start-menu shortcut and the "Analyze with Diskscope" entries.
param(
    [string]$Root = "HKCU:\Software\Classes",
    [string]$Shortcut = (Join-Path ([Environment]::GetFolderPath("Programs")) "Diskscope.lnk")
)

foreach ($k in "Directory\shell\Diskscope", "Directory\Background\shell\Diskscope", "Drive\shell\Diskscope") {
    $key = Join-Path $Root $k
    if (Test-Path $key) { Remove-Item -Path $key -Recurse -Force }
}
if (Test-Path $Shortcut) { Remove-Item -Path $Shortcut -Force }

Write-Host "Removed the Diskscope shortcut and the 'Analyze with Diskscope' context menu entries."
