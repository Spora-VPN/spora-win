# submit.ps1 — run from the SSH session: hands a job to the desktop agent and
# waits for it. The job body arrives base64-encoded (UTF-8), either as -Script
# or on stdin. Prints the job's output; exits with the job's exit code.
param([int]$Timeout = 120, [string]$Script, [string]$Id)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$root = 'C:\SporaTest'
$q = Join-Path $root 'q'
if (-not $Id) { $Id = (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '-' + (Get-Random -Maximum 9999) }
if (-not $Script) { $Script = [Console]::In.ReadToEnd() }
$body = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Script.Trim()))
$tmp = Join-Path $q "$Id.tmp"
[IO.File]::WriteAllText($tmp, "# timeout=$Timeout`r`n$body", (New-Object Text.UTF8Encoding($true)))
Rename-Item -Path $tmp -NewName "$Id.ps1"

$rcFile = Join-Path $q "$Id.rc"
$outFile = Join-Path $q "$Id.out"
$alive = Join-Path $root 'agent.alive'
$deadline = (Get-Date).AddSeconds($Timeout + 15)
$warned = $false
while (-not (Test-Path $rcFile)) {
    if ((Get-Date) -gt $deadline) { Write-Output "submit: timed out waiting for job $Id"; exit 124 }
    # Only while the job is still unclaimed: the agent refreshes the heartbeat
    # between jobs, not during one.
    if (-not $warned -and (Test-Path (Join-Path $q "$Id.ps1"))) {
        # The agent rewrites "<ISO local time> <pid>" every second, so a read
        # can collide with a write; the file's mtime is the fallback.
        $stamp = [datetime]::MinValue
        try { $beat = [IO.File]::ReadAllText($alive).Trim(); $stamp = [datetime]::ParseExact($beat.Split(' ')[0], 's', $null) } catch {}
        $mt = (Get-Item $alive -ErrorAction SilentlyContinue).LastWriteTime
        if ($mt -and $mt -gt $stamp) { $stamp = $mt }
        if (((Get-Date) - $stamp).TotalSeconds -gt 8) {
            Write-Output "submit: WARNING desktop agent heartbeat is stale or missing; is the SporaTestAgent task running and the user logged on? ('wt setup' re-registers it)"
            $warned = $true
        }
    }
    Start-Sleep -Milliseconds 100
}
Start-Sleep -Milliseconds 50
if (Test-Path $outFile) { [IO.File]::ReadAllText($outFile) | Write-Output }
$rc = [int](Get-Content $rcFile)
Remove-Item $outFile, $rcFile -ErrorAction SilentlyContinue
exit $rc
