$ErrorActionPreference = 'Stop'
$flow = Join-Path $PSScriptRoot 'Flow.exe'
if (-not (Test-Path -LiteralPath $flow)) { throw 'Flow.exe is missing from this directory.' }
$process = Start-Process -FilePath $flow -ArgumentList '--install-player' -WindowStyle Hidden -PassThru -Wait
$reportPath = Join-Path $PSScriptRoot 'player-install-result.json'
if (-not (Test-Path -LiteralPath $reportPath)) { throw 'Flow did not return an installation result.' }
$report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
if ($process.ExitCode -ne 0 -or -not $report.ok) { throw "Player installation failed: $($report.error)" }
Write-Output "Player ready: $($report.player)"
