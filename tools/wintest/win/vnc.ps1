# vnc.ps1 — install/configure TightVNC Server on the test box, loopback-only.
# Runs in the SSH session (elevated, session 0): an MSI install and a service,
# nothing visual, so session 0 is fine. See README.md "Remote desktop access".
param(
  [Parameter(Mandatory=$true)][string]$Password,   # VNC auth password (TightVNC truncates at 8 chars)
  [string]$Msi = 'C:\SporaTest\bin\tightvnc.msi',
  [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'

if ($Uninstall) {
  $p = Start-Process msiexec -ArgumentList @('/x', $Msi, '/quiet', '/norestart') -Wait -PassThru
  "uninstall exit code: $($p.ExitCode)"
  exit $p.ExitCode
}

if (-not (Test-Path $Msi)) { throw "MSI not found at $Msi (wt vnc install copies it over)" }

# Only the Server feature. No firewall exception and LOOPBACKONLY: the server
# listens on 127.0.0.1 alone, so the SSH tunnel is the only way in and the weak
# VNC auth never faces the LAN.
# BLOCKLOCALINPUT=0 matters: wt injects input into this same desktop session
# through SendKeys/UIA, and blocking local input while a viewer is attached
# would break every wt click and keystroke.
# DISCONNECTACTION=0 (do nothing) keeps the console session logged on when the
# viewer goes away -- a lock or logoff would kill the SporaTestAgent desktop.
# REMOVEWALLPAPER=0 so attaching a viewer does not change what screenshots see.
$msiArgs = @(
  '/i', $Msi, '/quiet', '/norestart', 'ADDLOCAL=Server',
  'SERVER_REGISTER_AS_SERVICE=1', 'SERVER_ADD_FIREWALL_EXCEPTION=0', 'SERVER_ALLOW_SAS=1',
  'SET_USEVNCAUTHENTICATION=1', 'VALUE_OF_USEVNCAUTHENTICATION=1',
  "SET_PASSWORD=1", "VALUE_OF_PASSWORD=$Password",
  'SET_ALLOWLOOPBACK=1',   'VALUE_OF_ALLOWLOOPBACK=1',
  'SET_LOOPBACKONLY=1',    'VALUE_OF_LOOPBACKONLY=1',
  'SET_DISCONNECTACTION=1','VALUE_OF_DISCONNECTACTION=0',
  'SET_BLOCKLOCALINPUT=1', 'VALUE_OF_BLOCKLOCALINPUT=0',
  'SET_BLOCKREMOTEINPUT=1','VALUE_OF_BLOCKREMOTEINPUT=0',
  'SET_REMOVEWALLPAPER=1', 'VALUE_OF_REMOVEWALLPAPER=0',
  'SET_ACCEPTHTTPCONNECTIONS=1', 'VALUE_OF_ACCEPTHTTPCONNECTIONS=0'
)
$p = Start-Process msiexec -ArgumentList $msiArgs -Wait -PassThru
if ($p.ExitCode -ne 0) { throw "msiexec failed with exit code $($p.ExitCode)" }

# UseD3D=0 is not optional on this box: with TightVNC's default D3D capture the
# framebuffer comes through solid black (measured mean brightness 0.15 vs 182
# with GDI) even though authentication, framebuffer geometry and input
# injection all work and wt's own screenshots show a normal desktop.
# This has to be a registry write: the MSI declares SET_USED3D/VALUE_OF_USED3D
# in its Property table but 2.8.88 never writes the value (verified -- a fresh
# install with those properties leaves UseD3D absent and the screen black).
New-ItemProperty -Path 'HKLM:\SOFTWARE\TightVNC\Server' -Name 'UseD3D' -Value 0 -PropertyType DWord -Force | Out-Null

Restart-Service tvnserver
Start-Sleep 2
$svc = Get-Service tvnserver -ErrorAction SilentlyContinue
if (-not $svc) { throw 'tvnserver service was not registered' }
if ($svc.Status -ne 'Running') { Start-Service tvnserver; Start-Sleep 2 }
"service: $((Get-Service tvnserver).Status), UseD3D=$((Get-ItemProperty 'HKLM:\SOFTWARE\TightVNC\Server').UseD3D)"
