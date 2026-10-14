$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
Set-Location $PSScriptRoot
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# honour CARGO_TARGET_DIR / .cargo/config target-dir
$targetDir = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
$src = Join-Path $targetDir "release\diskscope.exe"
$dst = Join-Path $PSScriptRoot "diskscope.exe"

if (Test-Path $dst) {
    try {
        $fs = [System.IO.File]::Open($dst, 'Open', 'ReadWrite', 'None')
        $fs.Close()
    } catch {
        $running = Get-Process diskscope -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $dst }
        $pids = ($running | ForEach-Object { $_.Id }) -join ", "
        Write-Error ("diskscope.exe is in use" + $(if ($pids) { " (running, PID $pids)" } else { "" }) +
            ". Close Diskscope and run build.ps1 again. The new build is at $src")
        exit 1
    }
}

Copy-Item $src $dst -Force
Write-Host "Built diskscope.exe"
