# runjob.ps1 — wrapper the agent starts for each job (fresh powershell -STA in
# the desktop session). Dot-sources lib.ps1 then the job, captures all output
# streams to <id>.out. A job reports failure by setting $global:WT_RC (do not
# `exit` from a job: it would cut the output short).
param([Parameter(Mandatory)][string]$Id)
$ErrorActionPreference = 'Continue'
$q   = 'C:\SporaTest\q'
$job = Join-Path $q "$Id.running.ps1"
$out = Join-Path $q "$Id.out"
$global:WT_RC = 0
try {
    & { . 'C:\SporaTest\lib.ps1'; . $job } *>&1 | ForEach-Object {
        if ($_ -is [System.Management.Automation.ErrorRecord]) {
            $global:WT_RC = 1
            "ERROR: $($_.Exception.Message)`n   at $($_.InvocationInfo.PositionMessage)"
        } else { $_ }
    } | Out-String -Width 220 -Stream | Out-File -FilePath $out -Encoding utf8 -Append
} catch {
    "FATAL: $($_.Exception.Message)`n$($_.ScriptStackTrace)" | Out-File -FilePath $out -Encoding utf8 -Append
    $global:WT_RC = 1
}
exit $global:WT_RC
