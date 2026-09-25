$ErrorActionPreference = 'Stop'
# Windows build linked from https://mpv.io/installation/. Pin archive and checksum.
$archiveUrl = 'https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20260925/mpv-x86_64-20260925-git-2a4eb8067c.7z'
$expectedHash = 'aef0320478257259c7365087b6d3c87c91c8731b1f3a4e52f1d86d36bac9c998'
$runtimeDirectory = Join-Path $PSScriptRoot 'runtime/mpv'
$archiveDirectory = Join-Path $PSScriptRoot 'build/player-download'
New-Item -ItemType Directory -Force $runtimeDirectory, $archiveDirectory | Out-Null
$archivePath = Join-Path $archiveDirectory 'mpv.7z'
if (!(Test-Path -LiteralPath $archivePath) -or (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedHash) {
    Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath
}
if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedHash) {
    throw 'mpv archive checksum mismatch; extraction cancelled.'
}
& tar -xf $archivePath -C $runtimeDirectory
if ($LASTEXITCODE -ne 0) { throw 'Could not extract mpv archive using Windows tar.' }
Write-Output 'Player ready. Keep runtime/mpv beside Flow.exe. No system installation was performed.'
