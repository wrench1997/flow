$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'start.ps1') -BuildOnly
Write-Output 'Ready: Flow.exe. Native Rust + librqbit; no Python or extra runtime required.'
