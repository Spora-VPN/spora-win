# setup.ps1 — one-time (re)setup of the test box; run elevated over SSH by
# `wt setup` after the win/ scripts were copied to C:\SporaTest.
#  * registers the SporaTestAgent scheduled task (runs agent.ps1 in the test
#    user's interactive session at logon, unelevated) and starts it now
#  * keeps the desktop capturable: no display/standby timeout, no screensaver,
#    lid close does nothing (it is a laptop). Skip with -NoPower.
param([string]$User = $env:USERNAME, [switch]$NoPower)
$ErrorActionPreference = 'Stop'
$root = 'C:\SporaTest'
New-Item -ItemType Directory -Force -Path "$root\q", "$root\shots", "$root\bin" | Out-Null
Get-ChildItem "$root\*.ps1" | Unblock-File

$taskName = 'SporaTestAgent'
$ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$action    = New-ScheduledTaskAction -Execute $ps -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$root\agent.ps1`"" -WorkingDirectory $root
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User $User
$principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -StartWhenAvailable `
    -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1)

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
}
# Stray agents from an earlier registration hold the single-instance mutex.
Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
    Where-Object { $_.CommandLine -like '*SporaTest\agent.ps1*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 500
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings `
    -Description 'Spora UI test agent: runs test jobs inside the interactive desktop session (tools/wintest in spora-win)' | Out-Null
Start-ScheduledTask -TaskName $taskName
Start-Sleep -Seconds 3
$info = Get-ScheduledTaskInfo -TaskName $taskName
$beat = Get-Item "$root\agent.alive" -ErrorAction SilentlyContinue
$fresh = $beat -and ((Get-Date) - $beat.LastWriteTime).TotalSeconds -lt 5
Write-Output "task $taskName state=$((Get-ScheduledTask -TaskName $taskName).State) lastResult=$($info.LastTaskResult) heartbeat=$(if ($fresh) { 'ok' } else { 'MISSING (is ' + $User + ' logged on at the desktop?)' })"

if (-not $NoPower) {
    foreach ($s in 'monitor-timeout-ac', 'monitor-timeout-dc', 'standby-timeout-ac', 'standby-timeout-dc', 'hibernate-timeout-ac', 'hibernate-timeout-dc') {
        powercfg /change $s 0 | Out-Null
    }
    $btn = '4f971e89-eebd-4455-a8de-9e59040e7347'; $lid = '5ca83367-6e45-459f-a27b-476b1d01c936'
    powercfg /setacvalueindex SCHEME_CURRENT $btn $lid 0 | Out-Null
    powercfg /setdcvalueindex SCHEME_CURRENT $btn $lid 0 | Out-Null
    powercfg /setactive SCHEME_CURRENT | Out-Null
    Set-ItemProperty 'HKCU:\Control Panel\Desktop' -Name ScreenSaveActive -Value '0'
    Set-ItemProperty 'HKCU:\Control Panel\Desktop' -Name ScreenSaverIsSecure -Value '0' -ErrorAction SilentlyContinue
    Write-Output 'power: display/standby/hibernate timeouts off, lid close does nothing, screensaver off'
}
Write-Output "setup done (session of this shell: $((Get-Process -Id $PID).SessionId); desktop user: $((Get-CimInstance Win32_ComputerSystem).UserName))"
