# agent.ps1 — desktop-session job runner.
# Runs as a scheduled task ("SporaTestAgent") in the interactive session of
# the test user (SSH sessions live in session 0, where no window is visible).
# It watches C:\SporaTest\q for <id>.ps1 job files (written by submit.ps1),
# runs each in a fresh powershell.exe -STA via runjob.ps1, and writes <id>.rc
# when done. Heartbeat: C:\SporaTest\agent.alive. Log: C:\SporaTest\agent.log.

$ErrorActionPreference = 'Continue'
$root  = 'C:\SporaTest'
$q     = Join-Path $root 'q'
$alive = Join-Path $root 'agent.alive'
$log   = Join-Path $root 'agent.log'
New-Item -ItemType Directory -Force -Path $q, (Join-Path $root 'shots'), (Join-Path $root 'bin') | Out-Null

$mutex = New-Object System.Threading.Mutex($false, 'Local\SporaTestAgent')
if (-not $mutex.WaitOne(0)) { "$(Get-Date -Format s) another agent is running; exiting" | Out-File $log -Append; exit 0 }
"$(Get-Date -Format s) agent start pid=$PID session=$((Get-Process -Id $PID).SessionId) user=$env:USERNAME" | Out-File $log -Append

$ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$lastBeat = [datetime]::MinValue
while ($true) {
    $now = Get-Date
    if (($now - $lastBeat).TotalSeconds -ge 1) {
        "$($now.ToString('s')) $PID" | Set-Content $alive -ErrorAction SilentlyContinue
        $lastBeat = $now
    }
    $jobs = Get-ChildItem -Path $q -Filter '*.ps1' -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -notlike '*.running.ps1' } | Sort-Object Name
    foreach ($f in $jobs) {
        $id = $f.BaseName
        $running = Join-Path $q "$id.running.ps1"
        try { Rename-Item -Path $f.FullName -NewName "$id.running.ps1" -ErrorAction Stop } catch { continue }
        $timeout = 120
        $first = Get-Content $running -TotalCount 1 -ErrorAction SilentlyContinue
        if ($first -match '^#\s*timeout=(\d+)') { $timeout = [int]$Matches[1] }
        $out = Join-Path $q "$id.out"
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $ps
        $psi.Arguments = "-NoProfile -NonInteractive -STA -ExecutionPolicy Bypass -File `"$root\runjob.ps1`" -Id $id"
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        $psi.WorkingDirectory = $root
        $rc = -1
        try {
            $p = [System.Diagnostics.Process]::Start($psi)
            if ($p.WaitForExit($timeout * 1000)) {
                $rc = $p.ExitCode
            } else {
                try { $p.Kill() } catch {}
                "AGENT: job timed out after ${timeout}s and was killed" | Out-File $out -Append -Encoding utf8
                $rc = 124
            }
        } catch {
            "AGENT ERROR: $($_.Exception.Message)" | Out-File $out -Append -Encoding utf8
            $rc = 125
        }
        Set-Content -Path (Join-Path $q "$id.rc") -Value $rc
        "$(Get-Date -Format s) job $id rc=$rc" | Out-File $log -Append
        Remove-Item $running -ErrorAction SilentlyContinue
    }
    Start-Sleep -Milliseconds 150
}
