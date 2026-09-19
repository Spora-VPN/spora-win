# lib.ps1 — helpers available to every job the desktop-session agent runs.
# Dot-sourced by runjob.ps1 in a fresh powershell.exe (-STA) inside the
# interactive desktop session, so everything here can see the real desktop:
# windows, pixels, input, UI Automation. Runs unelevated as the desktop user.

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Drawing, System.Windows.Forms, UIAutomationClient, UIAutomationTypes

$WT_ROOT    = 'C:\SporaTest'
$WT_SHOTS   = Join-Path $WT_ROOT 'shots'
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
$WT_INSTALL = Get-SporaInstallDir
$WT_UI_EXE  = Join-Path $WT_INSTALL 'spora-winui.exe'

Add-Type -Namespace Wt -Name Native -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
[DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, int attr, out RECT rect, int size);
[DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
[DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
[DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, System.Text.StringBuilder s, int n);
[DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
public delegate bool EnumProc(IntPtr h, IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
[StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
'@
[Wt.Native]::SetProcessDPIAware() | Out-Null

# ── Process / window ──────────────────────────────────────────────────

function Get-UiProcess { Get-Process spora-winui -ErrorAction SilentlyContinue }

# Visible, titled top-level windows belonging to spora-winui. (Process
# .MainWindowHandle is not usable: it can land on a 6px helper window.)
function Get-UiWindows {
    $pids = @(Get-UiProcess | ForEach-Object { [uint32]$_.Id })
    $found = New-Object System.Collections.Generic.List[object]
    if (-not $pids) { return $found }
    $cb = [Wt.Native+EnumProc]{
        param($h, $l)
        if ([Wt.Native]::IsWindowVisible($h)) {
            $p = [uint32]0; [Wt.Native]::GetWindowThreadProcessId($h, [ref]$p) | Out-Null
            if ($pids -contains $p) {
                $t = Get-WindowTitle $h
                if ($t) { $found.Add([pscustomobject]@{ Hwnd = $h; Title = $t; ProcessId = $p }) }
            }
        }
        $true
    }
    [Wt.Native]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $found
}

# HWND of the Spora main window: the largest titled window of the process,
# restored if minimised, waiting up to -TimeoutSec for it to appear and be
# laid out (winit creates it tiny before Slint sizes it).
function Get-UiWindow([int]$TimeoutSec = 10) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $best = $null
    do {
        $best = $null; $bestArea = 0
        foreach ($w in Get-UiWindows) {
            if ([Wt.Native]::IsIconic($w.Hwnd)) { [Wt.Native]::ShowWindow($w.Hwnd, 9) | Out-Null; Start-Sleep -Milliseconds 150 }
            $r = Get-WindowRect $w.Hwnd
            $a = $r.Width * $r.Height
            if ($a -gt $bestArea) { $best = $w.Hwnd; $bestArea = $a }
        }
        if ($best -and $bestArea -gt 2500) { return [IntPtr]$best }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    if ($best) { return [IntPtr]$best }
    return [IntPtr]::Zero
}

# Visible frame of a window (DWM extended frame bounds, i.e. without the
# invisible resize/shadow border), as a System.Drawing.Rectangle in screen px.
function Get-WindowRect([IntPtr]$Hwnd) {
    $r = New-Object Wt.Native+RECT
    $hr = [Wt.Native]::DwmGetWindowAttribute($Hwnd, 9, [ref]$r, [Runtime.InteropServices.Marshal]::SizeOf($r))
    if ($hr -ne 0) { [Wt.Native]::GetWindowRect($Hwnd, [ref]$r) | Out-Null }
    return New-Object System.Drawing.Rectangle $r.Left, $r.Top, ($r.Right - $r.Left), ($r.Bottom - $r.Top)
}

# Restore + raise the Spora window so screenshots and input reach it.
# Windows refuses SetForegroundWindow from a background process, so this
# escalates: attach to the foreground thread's input queue, minimise/restore,
# AppActivate. Returns $true only when Spora really is the foreground window.
function Show-Ui([IntPtr]$Hwnd = (Get-UiWindow -TimeoutSec 2)) {
    if ($Hwnd -eq [IntPtr]::Zero) { return $false }
    if ([Wt.Native]::IsIconic($Hwnd)) { [Wt.Native]::ShowWindow($Hwnd, 9) | Out-Null; Start-Sleep -Milliseconds 200 }   # SW_RESTORE
    if ([Wt.Native]::GetForegroundWindow() -eq $Hwnd) { return $true }
    [Wt.Native]::SetForegroundWindow($Hwnd) | Out-Null; Start-Sleep -Milliseconds 100
    if ([Wt.Native]::GetForegroundWindow() -eq $Hwnd) { return $true }
    $fg = [Wt.Native]::GetForegroundWindow(); $procId = [uint32]0
    $fgTid = [Wt.Native]::GetWindowThreadProcessId($fg, [ref]$procId)
    $me = [Wt.Native]::GetCurrentThreadId()
    if ($fgTid -ne 0 -and $fgTid -ne $me) {
        [Wt.Native]::AttachThreadInput($me, $fgTid, $true) | Out-Null
        [Wt.Native]::BringWindowToTop($Hwnd) | Out-Null
        [Wt.Native]::SetForegroundWindow($Hwnd) | Out-Null
        [Wt.Native]::AttachThreadInput($me, $fgTid, $false) | Out-Null
        Start-Sleep -Milliseconds 100
        if ([Wt.Native]::GetForegroundWindow() -eq $Hwnd) { return $true }
    }
    [Wt.Native]::ShowWindow($Hwnd, 6) | Out-Null; Start-Sleep -Milliseconds 150   # SW_MINIMIZE
    [Wt.Native]::ShowWindow($Hwnd, 9) | Out-Null; Start-Sleep -Milliseconds 250   # SW_RESTORE activates
    if ([Wt.Native]::GetForegroundWindow() -eq $Hwnd) { return $true }
    try {
        Add-Type -AssemblyName Microsoft.VisualBasic
        [Microsoft.VisualBasic.Interaction]::AppActivate((Get-UiProcess | Select-Object -First 1).Id)
        Start-Sleep -Milliseconds 150
    } catch {}
    return ([Wt.Native]::GetForegroundWindow() -eq $Hwnd)
}

function Get-WindowTitle([IntPtr]$Hwnd) {
    $sb = New-Object System.Text.StringBuilder 256
    [Wt.Native]::GetWindowTextW($Hwnd, $sb, 256) | Out-Null
    $sb.ToString()
}

# Is the top-level window under screen point X,Y one of spora-winui's?
function Test-PointOnUi([int]$X, [int]$Y) {
    $pt = New-Object Wt.Native+POINT; $pt.X = $X; $pt.Y = $Y
    $w = [Wt.Native]::WindowFromPoint($pt)
    if ($w -eq [IntPtr]::Zero) { return $false }
    $top = [Wt.Native]::GetAncestor($w, 2)   # GA_ROOT
    $procId = [uint32]0
    [Wt.Native]::GetWindowThreadProcessId($top, [ref]$procId) | Out-Null
    return [bool](Get-UiProcess | Where-Object { $_.Id -eq $procId })
}

# Move the Spora window (its outer rect) to X,Y without resizing.
function Move-Ui([int]$X = 0, [int]$Y = 0, [IntPtr]$Hwnd = (Get-UiWindow -TimeoutSec 2)) {
    if ($Hwnd -eq [IntPtr]::Zero) { return }
    [Wt.Native]::SetWindowPos($Hwnd, [IntPtr]::Zero, $X, $Y, 0, 0, 0x0001 -bor 0x0004 -bor 0x0010) | Out-Null   # NOSIZE|NOZORDER|NOACTIVATE
}

# Launch the installed UI (unelevated, like a user would), wait for its
# window, park it at the top-left corner (the 680px-tall window otherwise
# opens partly below a 768px screen) and raise it. Other windows on the test
# desktop are minimised first unless -KeepWindows. Extra args are passed
# through (e.g. a spora: URL). $env:WT_RUST_LOG or -RustLog sets RUST_LOG for
# the UI's log at %LOCALAPPDATA%\Spora\spora-winui.log.
function Start-Ui {
    param([string[]]$Arguments = @(), [string]$RustLog = $env:WT_RUST_LOG, [int]$TimeoutSec = 15,
          [switch]$KeepWindows, [int]$PosX = 0, [int]$PosY = 0)
    if (-not (Test-Path $WT_UI_EXE)) { throw "UI not installed: $WT_UI_EXE (run 'wt install')" }
    if ($RustLog) { $env:RUST_LOG = $RustLog }
    if (-not $KeepWindows -and -not (Get-UiProcess)) { try { (New-Object -ComObject Shell.Application).MinimizeAll() } catch {}; Start-Sleep -Milliseconds 300 }
    if ($Arguments.Count -gt 0) {
        Start-Process -FilePath $WT_UI_EXE -ArgumentList $Arguments -WorkingDirectory $WT_INSTALL
    } else {
        Start-Process -FilePath $WT_UI_EXE -WorkingDirectory $WT_INSTALL
    }
    $h = Get-UiWindow -TimeoutSec $TimeoutSec
    if ($h -eq [IntPtr]::Zero) {
        $p = Get-UiProcess
        if ($p) { Write-Output "spora-winui running (pid $($p.Id -join ',')) but no window within ${TimeoutSec}s" }
        else    { Write-Output "spora-winui exited without showing a window (see 'wt log --ui')" }
        return
    }
    Start-Sleep -Milliseconds 400
    Move-Ui -X $PosX -Y $PosY -Hwnd $h
    $fg = Show-Ui $h
    $r = Get-WindowRect $h
    Write-Output ("window hwnd=0x{0:X} pid={1} at {2},{3} size {4}x{5} foreground={6}" -f $h.ToInt64(), (Get-UiProcess | Select-Object -First 1).Id, $r.X, $r.Y, $r.Width, $r.Height, $fg)
}

function Stop-Ui {
    $p = Get-UiProcess
    if (-not $p) { Write-Output 'spora-winui not running'; return }
    $p | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    Write-Output "stopped spora-winui (pid $($p.Id -join ','))"
}

# ── Screenshots ───────────────────────────────────────────────────────

# PNG of the Spora window (default) or the whole screen (-Screen) into
# C:\SporaTest\shots. Prints "SHOT <path> ..." which `wt shot` parses.
# The window capture uses PrintWindow(PW_RENDERFULLCONTENT): it renders the
# window's own surface, so it is immune to overlapping windows and to a
# display that is off or composing late (both seen on this laptop). Pixels
# are cropped to the DWM frame so image coordinates match `wt tree`/`wt click`.
# -Composited captures the window region from the screen instead.
function Save-Screenshot {
    param([string]$Name, [switch]$Screen, [switch]$NoRaise, [switch]$Composited)
    if (-not $Name) { $Name = Get-Date -Format 'yyyyMMdd-HHmmss' }
    if ($Name -notmatch '\.png$') { $Name += '.png' }
    New-Item -ItemType Directory -Force -Path $WT_SHOTS | Out-Null
    $path = Join-Path $WT_SHOTS $Name
    $bmp = $null; $rect = $null; $how = 'screen'
    if (-not $Screen) {
        $h = Get-UiWindow -TimeoutSec 2
        if ($h -ne [IntPtr]::Zero) {
            if (-not $NoRaise) {
                if (-not (Show-Ui $h)) { Write-Output "WARNING: could not bring Spora to the foreground; foreground is '$(Get-WindowTitle ([Wt.Native]::GetForegroundWindow()))'" }
                Start-Sleep -Milliseconds 300
            }
            $rect = Get-WindowRect $h
            if (-not $Composited) {
                $wr = New-Object Wt.Native+RECT; [Wt.Native]::GetWindowRect($h, [ref]$wr) | Out-Null
                $full = New-Object System.Drawing.Bitmap ($wr.Right - $wr.Left), ($wr.Bottom - $wr.Top)
                $g = [System.Drawing.Graphics]::FromImage($full); $hdc = $g.GetHdc()
                $ok = [Wt.Native]::PrintWindow($h, $hdc, 2)
                $g.ReleaseHdc($hdc); $g.Dispose()
                if ($ok) {
                    $crop = New-Object System.Drawing.Rectangle ($rect.X - $wr.Left), ($rect.Y - $wr.Top), $rect.Width, $rect.Height
                    $crop.Intersect((New-Object System.Drawing.Rectangle 0, 0, $full.Width, $full.Height))
                    $bmp = $full.Clone($crop, $full.PixelFormat); $full.Dispose(); $how = 'printwindow'
                } else { $full.Dispose(); Write-Output 'PrintWindow failed; falling back to screen capture' }
            }
        } else {
            Write-Output 'no Spora window; capturing the whole screen'
        }
    }
    if (-not $bmp) {
        if (-not $rect) { $rect = [System.Windows.Forms.SystemInformation]::VirtualScreen }
        $bmp = New-Object System.Drawing.Bitmap $rect.Width, $rect.Height
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($rect.Location, [System.Drawing.Point]::Empty, $rect.Size, [System.Drawing.CopyPixelOperation]::SourceCopy)
        $g.Dispose()
    }
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $w = $bmp.Width; $hh = $bmp.Height; $bmp.Dispose()
    Write-Output "SHOT $path (${w}x$hh $how at $($rect.X),$($rect.Y))"
}

# ── UI Automation ─────────────────────────────────────────────────────
# Slint exposes its element tree through AccessKit when the crate is built
# with the "accessibility" feature (it is). Coordinates below are relative
# to the window's visible frame (same as a `wt shot` image); AbsX/AbsY are
# screen coordinates.

function Get-UiaTree {
    param([IntPtr]$Hwnd = (Get-UiWindow -TimeoutSec 2), [int]$MaxDepth = 16, [switch]$Raw)
    if ($Hwnd -eq [IntPtr]::Zero) { throw 'no Spora window' }
    $root = [System.Windows.Automation.AutomationElement]::FromHandle($Hwnd)
    $walker = if ($Raw) { [System.Windows.Automation.TreeWalker]::RawViewWalker } else { [System.Windows.Automation.TreeWalker]::ControlViewWalker }
    $win = Get-WindowRect $Hwnd
    $list = New-Object System.Collections.Generic.List[object]
    $visit = $null
    $visit = {
        param($e, $d)
        try { $rb = $e.Current.BoundingRectangle } catch { $rb = [System.Windows.Rect]::Empty }
        $has = -not $rb.IsEmpty
        $list.Add([pscustomobject]@{
            Depth = $d
            Type  = ($e.Current.ControlType.ProgrammaticName -replace '^ControlType\.', '')
            Name  = $e.Current.Name
            Id    = $e.Current.AutomationId
            X     = if ($has) { [int]($rb.X - $win.X) } else { $null }
            Y     = if ($has) { [int]($rb.Y - $win.Y) } else { $null }
            W     = if ($has) { [int]$rb.Width } else { $null }
            H     = if ($has) { [int]$rb.Height } else { $null }
            AbsX  = if ($has) { [int]$rb.X } else { $null }
            AbsY  = if ($has) { [int]$rb.Y } else { $null }
            Element = $e
        })
        if ($d -ge $MaxDepth) { return }
        $c = $walker.GetFirstChild($e)
        while ($c) { & $visit $c ($d + 1); $c = $walker.GetNextSibling($c) }
    }
    & $visit $root 0
    return $list
}

function Format-UiaTree($Tree) {
    foreach ($n in $Tree) {
        $line = ('  ' * $n.Depth) + $n.Type
        if ($n.Name) { $line += " '" + $n.Name + "'" }
        if ($n.Id)   { $line += ' #' + $n.Id }
        if ($n.W -ne $null) { $line += " @$($n.X),$($n.Y) $($n.W)x$($n.H)" }
        $line
    }
}

# First element whose Name equals (then contains) -Name, optionally of -Type
# (e.g. Button, Text, Edit). Only elements with a bounding box count.
function Find-UiaElement {
    param([string]$Name, [string]$Type, [switch]$All)
    $tree = Get-UiaTree | Where-Object { $_.W -ne $null -and $_.Depth -gt 0 }
    if ($Type) { $tree = $tree | Where-Object { $_.Type -eq $Type } }
    $hits = @($tree | Where-Object { $_.Name -eq $Name })
    if (-not $hits) { $hits = @($tree | Where-Object { $_.Name -like "*$Name*" }) }
    if ($All) { return $hits }
    return ($hits | Select-Object -First 1)
}

# ── Input ─────────────────────────────────────────────────────────────

# Click at X,Y relative to the Spora window frame (or the screen with
# -Screen). Refuses unless the top-level window under the point belongs to
# spora-winui, so a buried Spora window never turns into clicks on whatever
# is in front of it; -Force skips that check (clicking outside Spora on purpose).
function Invoke-Click {
    param([int]$X, [int]$Y, [switch]$Screen, [switch]$Right, [switch]$Double, [switch]$Force)
    $h = Get-UiWindow -TimeoutSec 2
    if ($h -ne [IntPtr]::Zero) {
        $raised = Show-Ui $h
        if (-not $Screen) { $r = Get-WindowRect $h; $X += $r.X; $Y += $r.Y }
    } elseif (-not $Screen) { throw 'no Spora window for window-relative click' }
    if (-not $Force -and -not (Test-PointOnUi $X $Y)) {
        throw "refusing to click screen $X,${Y}: not over the Spora window (foreground: '$(Get-WindowTitle ([Wt.Native]::GetForegroundWindow()))'); use -Force to click anyway"
    }
    [Wt.Native]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 80
    if ($Right) { $down = 0x08; $up = 0x10 } else { $down = 0x02; $up = 0x04 }
    $n = if ($Double) { 2 } else { 1 }
    for ($i = 0; $i -lt $n; $i++) {
        [Wt.Native]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
        [Wt.Native]::mouse_event($up,   0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 80
    }
    Write-Output "clicked screen $X,$Y"
}

# Click the centre of the UIA element named -Name.
function Invoke-ClickElement {
    param([string]$Name, [string]$Type, [switch]$Right, [switch]$Double)
    $el = Find-UiaElement -Name $Name -Type $Type
    if (-not $el) { throw "no UIA element matching '$Name'$(if ($Type) { " of type $Type" }) (see 'wt tree')" }
    Invoke-Click -X ([int]($el.AbsX + $el.W / 2)) -Y ([int]($el.AbsY + $el.H / 2)) -Screen -Right:$Right -Double:$Double
    Write-Output "  -> $($el.Type) '$($el.Name)' @$($el.X),$($el.Y) $($el.W)x$($el.H)"
}

# Type literal text into the focused control (SendKeys metacharacters escaped).
function Send-Text([string]$Text) {
    Show-Ui | Out-Null
    $esc = [regex]::Replace($Text, '[+^%~(){}\[\]]', { param($m) '{' + $m.Value + '}' })
    [System.Windows.Forms.SendKeys]::SendWait($esc)
    Write-Output "typed $($Text.Length) chars"
}

# Raw SendKeys syntax: '{ENTER}', '{TAB}', '^a', '{BACKSPACE 5}' ...
function Send-Keys([string]$Keys) {
    Show-Ui | Out-Null
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
    Write-Output "sent keys $Keys"
}

# Put text on the clipboard and Ctrl+V it (robust for long URLs).
function Send-Paste([string]$Text) {
    Set-Clipboard -Value $Text
    Show-Ui | Out-Null
    [System.Windows.Forms.SendKeys]::SendWait('^v')
    Write-Output "pasted $($Text.Length) chars"
}

function Get-ClipboardText { Get-Clipboard -Raw }
