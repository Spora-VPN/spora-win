# deploy.ps1 — run elevated over SSH by `wt deploy` after the fresh exes were
# copied to C:\SporaTest\bin: swaps them into the installed location, cycling
# the Spora service for wincore and killing the UI for winui. Needs an
# existing install (`wt install` does the first one via the NSIS installer).
param([switch]$Ui, [switch]$Core)
$ErrorActionPreference = 'Stop'
if (-not $Ui -and -not $Core) { $Ui = $true; $Core = $true }
$bin = 'C:\SporaTest\bin'
# Where the installer put Spora: the NSIS installer is 32-bit, so its
# InstallDir key lives under WOW6432Node; fall back to the service's binary
# path, then the conventional locations.
function Get-SporaInstallDir {
    foreach ($k in 'HKLM:\SOFTWARE\WOW6432Node\Spora', 'HKLM:\SOFTWARE\Spora') {
        $d = (Get-ItemProperty $k -ErrorAction SilentlyContinue).InstallDir
        if ($d -and (Test-Path (Join-Path $d 'spora-winui.exe'))) { return $d }
    }
    $svc = Get-CimInstance Win32_Service -Filter "Name='Spora'" -ErrorAction SilentlyContinue
    if ($svc -and $svc.PathName -match '^"([^"]+)\\spora-wincore\.exe"') { return $Matches[1] }
    foreach ($d in "$env:ProgramFiles\Spora", "${env:ProgramFiles(x86)}\Spora") {
        if (Test-Path (Join-Path $d 'spora-winui.exe')) { return $d }
    }
    return "$env:ProgramFiles\Spora"
}
$inst = Get-SporaInstallDir
if (-not (Test-Path "$inst\spora-winui.exe")) { throw "Spora is not installed (looked in $inst); run 'wt install' first" }

if ($Core) {
    $svc = Get-Service Spora -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -ne 'Stopped') {
        Stop-Service Spora -Force
        $svc.WaitForStatus('Stopped', '00:00:30')
    }
    Get-Process spora-wincore -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    Copy-Item "$bin\spora-wincore.exe" "$inst\spora-wincore.exe" -Force
    if ($svc) { Start-Service Spora }
    Write-Output "wincore: $((Get-Item "$inst\spora-wincore.exe").LastWriteTime.ToString('s')) service=$((Get-Service Spora -ErrorAction SilentlyContinue).Status)"
}
if ($Ui) {
    Get-Process spora-winui -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    Copy-Item "$bin\spora-winui.exe" "$inst\spora-winui.exe" -Force
    Write-Output "winui:   $((Get-Item "$inst\spora-winui.exe").LastWriteTime.ToString('s'))"
}
