# stopui.ps1 - run over SSH by `wt kill` and `wt reset`: stops every
# spora-winui and says whether it worked. The SSH session is elevated, so it
# also ends a UI that runs elevated (one started by the installer's "Launch
# Spora" page, which inherits the installer's admin token); Stop-Ui in the
# desktop agent's limited-token session cannot, and says so.
$ErrorActionPreference = 'Continue'
$p = @(Get-Process spora-winui -ErrorAction SilentlyContinue)
if (-not $p) { Write-Output 'spora-winui not running'; exit 0 }
$p | Stop-Process -Force -ErrorAction SilentlyContinue
$deadline = (Get-Date).AddSeconds(5)
while ((Get-Process spora-winui -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 100
}
$left = @(Get-Process spora-winui -ErrorAction SilentlyContinue)
if ($left) {
    Write-Output "could not stop spora-winui (pid $($left.Id -join ','))"
    exit 1
}
Write-Output "stopped spora-winui (pid $($p.Id -join ','))"
