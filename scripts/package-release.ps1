param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [Parameter(Mandatory=$true)][string]$Version,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [Parameter(Mandatory=$true)][string]$SigningKey
)
$ErrorActionPreference='Stop'
$repository=Split-Path $PSScriptRoot
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
$OutputDirectory=(Resolve-Path -LiteralPath $OutputDirectory).Path
if((Resolve-Path -LiteralPath $Executable).Path -ne (Join-Path $OutputDirectory 'Flow.exe')){Copy-Item -LiteralPath $Executable -Destination (Join-Path $OutputDirectory 'Flow.exe')}
$toolTarget=if($env:CARGO_TARGET_DIR){$env:CARGO_TARGET_DIR}else{Join-Path $repository 'target'}
& cargo build --release --locked --manifest-path (Join-Path $repository 'tools/uninstaller/Cargo.toml') --target-dir $toolTarget
if($LASTEXITCODE -ne 0){throw 'Uninstaller build failed'}
$oldPayload=$env:FLOW_MAIN_PAYLOAD
$oldUninstaller=$env:FLOW_UNINSTALLER_PATH
try{
    $env:FLOW_MAIN_PAYLOAD=(Resolve-Path -LiteralPath $Executable).Path
    $env:FLOW_UNINSTALLER_PATH=Join-Path $toolTarget 'release/flow-uninstaller.exe'
    & cargo build --release --locked --manifest-path (Join-Path $repository 'tools/installer/Cargo.toml') --target-dir $toolTarget
    if($LASTEXITCODE -ne 0){throw 'Installer build failed'}
}finally{
    $env:FLOW_MAIN_PAYLOAD=$oldPayload
    $env:FLOW_UNINSTALLER_PATH=$oldUninstaller
}
Copy-Item -LiteralPath (Join-Path $toolTarget 'release/flow-installer.exe') -Destination (Join-Path $OutputDirectory "Flow-Setup-$Version-x64.exe")
$portable=Join-Path $OutputDirectory "Flow-$Version-portable"
New-Item -ItemType Directory -Force $portable | Out-Null
Copy-Item -LiteralPath $Executable -Destination (Join-Path $portable 'Flow.exe')
foreach($name in @('README.md','README.zh-CN.md')){Copy-Item -LiteralPath (Join-Path $repository $name) -Destination $portable}
Copy-Item -LiteralPath (Join-Path $repository 'vendor/librqbit/LICENSE') -Destination (Join-Path $portable 'librqbit-LICENSE.txt')
$assets=Join-Path $portable 'assets'
New-Item -ItemType Directory -Force $assets | Out-Null
Copy-Item -LiteralPath (Join-Path $repository 'assets/flow-icon.png') -Destination $assets
Compress-Archive -LiteralPath $portable -DestinationPath (Join-Path $OutputDirectory "Flow-$Version-windows-x64-portable.zip") -Force
python (Join-Path $PSScriptRoot 'sign-update.py') --exe (Join-Path $OutputDirectory 'Flow.exe') --version $Version --key $SigningKey --out (Join-Path $OutputDirectory 'update.json')
if($LASTEXITCODE -ne 0){throw 'Update signing failed'}
foreach($name in @('setup-player.ps1','register-defaults.ps1')){Copy-Item -LiteralPath (Join-Path $repository $name) -Destination $OutputDirectory}
$names=@('Flow.exe',"Flow-Setup-$Version-x64.exe","Flow-$Version-windows-x64-portable.zip",'update.json','setup-player.ps1','register-defaults.ps1')
$lines=foreach($name in $names){$h=Get-FileHash -LiteralPath (Join-Path $OutputDirectory $name); "$($h.Hash.ToLower())  $name"}
$lines | Set-Content -LiteralPath (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding ascii
Write-Output "Release packages ready in $OutputDirectory"
