param([switch]$BuildOnly)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
Copy-Item -LiteralPath 'target\release\flow-download.exe' -Destination 'Flow.exe' -Force
if (-not $BuildOnly) {
    Start-Process -FilePath "$PSScriptRoot\Flow.exe" -WorkingDirectory $PSScriptRoot
}
