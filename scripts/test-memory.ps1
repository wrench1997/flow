param(
    [ValidateRange(1,1000)][int]$RepeatCount=3,
    [switch]$Release,
    [string]$Cargo='cargo'
)
$ErrorActionPreference='Stop'
$repository=Split-Path $PSScriptRoot
$buildArguments=@('--locked')
if($Release){$buildArguments+='--release'}
Push-Location -LiteralPath $repository
try{
    for($round=1;$round -le $RepeatCount;$round++){
        Write-Output "Memory regression round $round/${RepeatCount}: UI, pause/resume, task deletion, disk snapshots and bounded peer/upload work"
        # Select both targets together so repeated rounds reuse the same feature set.
        & $Cargo test @buildArguments --lib --bin flow-download -p flow-download -p librqbit --no-default-features --features librqbit/rust-tls memory_
        if($LASTEXITCODE -ne 0){throw "Memory regressions failed in round $round"}
    }
}finally{
    Pop-Location
}
Write-Output "Memory regressions passed for $RepeatCount rounds."
