param([string]$Executable = (Join-Path $PSScriptRoot 'Flow.exe'))
$ErrorActionPreference = 'Stop'
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$command = '"' + $Executable + '" --open "%1"'
$icon = '"' + $Executable + '",0'
$user = [Microsoft.Win32.Registry]::CurrentUser
function Set-FlowRegistryValue([string]$Path, [string]$Name, [string]$Value) {
    $key = $user.CreateSubKey($Path)
    try { $key.SetValue($Name, $Value, [Microsoft.Win32.RegistryValueKind]::String) }
    finally { $key.Dispose() }
}
foreach ($progId in @('Flow.Magnet', 'Flow.Torrent')) {
    Set-FlowRegistryValue "Software\Classes\$progId" '' 'Flow'
    Set-FlowRegistryValue "Software\Classes\$progId\DefaultIcon" '' $icon
    Set-FlowRegistryValue "Software\Classes\$progId\shell\open\command" '' $command
}
Set-FlowRegistryValue 'Software\Classes\Flow.Magnet' 'URL Protocol' ''
Set-FlowRegistryValue 'Software\Classes\magnet' '' 'URL:Flow Magnet'
Set-FlowRegistryValue 'Software\Classes\magnet' 'URL Protocol' ''
Set-FlowRegistryValue 'Software\Classes\magnet\DefaultIcon' '' $icon
Set-FlowRegistryValue 'Software\Classes\magnet\shell\open\command' '' $command
Set-FlowRegistryValue 'Software\Classes\.torrent' '' 'Flow.Torrent'
Set-FlowRegistryValue 'Software\Classes\.torrent\OpenWithProgids' 'Flow.Torrent' ''
Set-FlowRegistryValue 'Software\Flow\Capabilities' 'ApplicationName' 'Flow'
Set-FlowRegistryValue 'Software\Flow\Capabilities' 'ApplicationDescription' 'Flow torrent and magnet download manager'
Set-FlowRegistryValue 'Software\Flow\Capabilities' 'ApplicationIcon' $icon
Set-FlowRegistryValue 'Software\Flow\Capabilities\FileAssociations' '.torrent' 'Flow.Torrent'
Set-FlowRegistryValue 'Software\Flow\Capabilities\URLAssociations' 'magnet' 'Flow.Magnet'
Set-FlowRegistryValue 'Software\RegisteredApplications' 'Flow' 'Software\Flow\Capabilities'
# Respect protected UserChoice entries; Windows Settings must change those.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class FlowAssociations {
    [DllImport("shell32.dll")] public static extern void SHChangeNotify(uint e, uint f, IntPtr a, IntPtr b);
}
'@
[FlowAssociations]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
Write-Output "Registered Flow for magnet links and .torrent files: $Executable"
foreach ($path in @('Software\Microsoft\Windows\Shell\Associations\UrlAssociations\magnet\UserChoice','Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\UserChoice')) {
    $key = $user.OpenSubKey($path)
    if ($null -ne $key) {
        try { Write-Output "Windows explicit default: $($key.GetValue('ProgId')); choose Flow in Default apps if needed." }
        finally { $key.Dispose() }
    }
}
