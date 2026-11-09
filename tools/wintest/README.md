# wintest — run and look at the Windows client from Linux

`tools/wintest/wt` builds the client here, pushes it to the dedicated Windows
test laptop over SSH, launches it on that machine's desktop, and brings back
screenshots, the UI Automation tree and logs. It can also start a sharer peer
on a Linux host so connect flows are real. It exists so UI changes can be
checked by whoever (or whatever) is making them, without touching the laptop.

```
wt setup                  # once per box, and after editing win/agent.ps1 (wt push for the other scripts)
wt install [installer]    # first install (silent NSIS), also for installer/upgrade tests;
                          # given a file (a release artifact), installs it without building
wt deploy                 # cargo build (release, gnu target) + swap exes into the install
wt run [spora-url]        # launch the UI on the desktop, parked top-left
wt shot [name]            # -> target/wintest/shots/<name>.png
wt tree                   # AccessKit tree with window-relative bounds
wt click 'Share your internet' | wt click 244 237 | wt type text | wt keys '{ENTER}' | wt paste text
wt drag 400 16 500 116    # press, move, release (window-relative): caption drag, edge resize
wt tray                   # click the tray icon (UI Automation) -> opens/closes the flyout
wt shot name --flyout     # PNG of the flyout window
wt toasts                 # toasts the client delivered (Action Center history)
wt log [--ui|--core]      # service log (%PROGRAMDATA%\Spora) and UI log (%LOCALAPPDATA%\Spora)
wt vnc                    # mirror the box's desktop over VNC (SSH-tunnelled); wt vnc install sets it up
wt peer-share             # sharer on the peer, prints the spora URL to feed `wt run`
wt help                   # everything else
```

Two hosts, with no defaults: `WT_HOST`, the Windows box (`user@host`; Windows
11 Home here, PowerShell 5.1 as the SSH shell, the user in Administrators), and
`WT_PEER`, the sharer peer (`root@host`, any Linux machine with systemd). Set
them in the environment or as `WT_HOST=...` and `WT_PEER=...` lines in
`~/.config/spora-wintest/env` (`WT_CONFIG` moves that file); the environment
wins. Screenshots land under `target/wintest/shots/`.

## How it works

SSH into Windows lands in **session 0**: the token is elevated (Administrators
get a full token over Windows OpenSSH, so services and Program Files are
directly writable, no UAC), but no window created there is visible and
screen capture returns nothing. Everything visual therefore goes through a
small agent running in the logged-on user's desktop session:

- `win/agent.ps1` runs as the scheduled task **SporaTestAgent** (at logon of
  the test user, "run only when logged on", limited token, no time limit). It
  polls `C:\SporaTest\q\` for job files and runs each in a fresh
  `powershell.exe -STA` via `win/runjob.ps1`, which dot-sources `win/lib.ps1`
  first. Output goes to `<id>.out`, the exit code to `<id>.rc`. Heartbeat in
  `C:\SporaTest\agent.alive`, log in `C:\SporaTest\agent.log`.
- `win/submit.ps1` is what `wt` calls over SSH: it drops the base64-encoded
  job in the queue, waits for the `.rc`, prints the output. One SSH round trip
  per job (about 3 s, connection reused via ControlMaster).
- `win/lib.ps1` is the toolbox jobs see: `Start-Ui`, `Stop-Ui`,
  `Save-Screenshot`, `Get-UiaTree`/`Format-UiaTree`/`Find-UiaElement`,
  `Invoke-Click`, `Invoke-ClickElement`, `Send-Text`, `Send-Keys`,
  `Send-Paste`, `Show-Ui`, `Move-Ui`. `wt ps '<script>'` runs anything with
  these in scope.
- `win/deploy.ps1` and `win/setup.ps1` run in the SSH session (elevated):
  service stop/copy/start and task registration respectively. Scripts are
  invoked with `-ExecutionPolicy Bypass` because the box's policy is the
  default Restricted; nothing machine-wide is changed for that.

The UI is launched unelevated from the desktop session, exactly as a user
would, and talks to the installed `Spora` service over its named pipe. The
install directory is read from the installer's registry key (32-bit view) or
the service's binary path, not assumed.

### Seeded UI states

The client seeds every screen from `SPORA_UI_DEMO` (`states`, `share`,
`empty`, `new`, `creating`, `banner`) without touching the service or the
saved connections, and `SPORA_UI_DEMO_FLYOUT=1|only` opens the flyout
(`only` hides the main window so `wt shot` finds the flyout). `wt run`
sets no environment, so launch through the desktop helper:

```
wt ps '$env:SPORA_UI_DEMO="states"; Start-Ui'; wt shot win-states; wt kill
```

### The tray icon and the flyout

Windows parks a new notification-area icon in the hidden-icons overflow.
`wt tray` (`Invoke-TrayIcon`) finds it through UI Automation — its name is
"Spora" plus the tooltip, e.g. `Spora Spora — OFFLINE` — opening the
overflow ("Show Hidden Icons") when needed, and invokes it, which toggles
the flyout. `wt shot NAME --flyout` captures the flyout window (the
smallest titled Spora window) with PrintWindow. While the icon sits in the
overflow, `wt tray` always ends with the flyout open: opening the overflow
takes focus (which closes the flyout) seconds before the icon is invoked, so
the client rightly treats the invoke as a fresh open. A click on a visible
icon toggles, because its button-down closes the flyout and its button-up
arrives within the client's 400 ms guard. To test the toggle, promote the
icon to the visible tray once (Settings › Personalization › Taskbar › Other
system tray icons, or `IsPromoted=1` under
`HKCU\Control Panel\NotifyIconSettings\<id>` for `spora-winui.exe`). The
taskbar's UI Automation tree also goes stale for a while after a window
hides, so `Invoke-TrayIcon` remembers the icon's last screen rectangle in
`C:\SporaTest\tray-rect.txt` and falls back to a real mouse click there. Closing the main window
hides it to the tray; "Open Spora" in the flyout or the tray menu brings it
back. The tray icon's tooltip is the aggregate status, so `wt tray` also
prints it.

Toast popups cannot be captured reliably here either, but the notification
platform keeps what was delivered: `wt toasts` lists the client's toasts
(`ToastNotificationManager.History` for the `Spora.Client` AppUserModelId,
which the client registers under `HKCU\Software\Classes\AppUserModelId`
on start) with their text.

### Coordinates and the accessibility tree

`spora-winui` is built with Slint's `accessibility` feature, so AccessKit
exposes the element tree over UI Automation. `wt tree` prints it with bounds
relative to the window's visible frame, the same coordinate space as a
`wt shot` image and as `wt click X Y`. Only elements Slint marks accessible
appear: plain `Text` does automatically; custom `TouchArea` buttons need
`accessible-role: button` (and ideally `accessible-label`) in the `.slint`
to be clickable by name. Until then, click the text inside them.

Clicks refuse to fire unless the top-level window under the pointer belongs
to `spora-winui` (a buried Spora window must never turn into clicks on
whatever is in front of it). `wt run` minimises other windows before a fresh
launch and parks Spora at the top-left corner, because the 680px-tall window
otherwise opens partly below the laptop's 768px screen.

### Screenshots: window captures only

`wt shot` renders the Spora window with `PrintWindow(PW_RENDERFULLCONTENT)`,
i.e. the window's own surface, cropped to its visible frame. That is the
reliable path on this laptop: the composited screen that `wt shot --screen`
copies has been observed frozen for long stretches (old windows at old
positions, a live taskbar clock), while the window-manager state, input and
UI Automation were all correct. Treat `--screen` as best effort. The cause
was not pinned down; a closed lid or the panel's power state is the likely
suspect, so if full-screen captures matter, keep the lid open or attach a
display (a dummy HDMI plug works).

Never minimise the Spora window from a job. One instance that was minimised
by an early version of `wt run` while a connect was in flight stopped
repainting and stopped applying its async status updates (it stayed on
"Connecting..." with the tunnel up); a deliberate minimise/restore on a
fresh instance did not reproduce it, so the trigger is unknown.

### Watching the desktop over VNC

`wt vnc` mirrors the box's console desktop so you can drive it by hand
without fetching the laptop. `wt vnc` alone opens the tunnel and a viewer;
`wt vnc tunnel` / `wt vnc down` manage just the tunnel, `wt vnc status`
reports service, listener and tunnel, and `wt vnc install` sets it up (it is
idempotent, so it doubles as a repair).

VNC and not RDP, for two reasons. The box is Windows 11 **Home**, which does
not host RDP at all; and RDP would take over the console session and leave it
detached on disconnect, which is precisely what kills `SporaTestAgent` and
turns every screenshot black. VNC mirrors the existing session instead, so
watching costs nothing: `wt shot`, `wt tree` and `wt click` keep working with
a viewer attached, and the agent heartbeat is unaffected (verified).

`wt vnc install` puts TightVNC 2.8.88 on the box (`win/vnc.ps1`, MSI verified
against a pinned SHA-256) configured so that:

- it listens on **127.0.0.1 only** (`LoopbackOnly`), with no firewall
  exception, so the SSH tunnel is the only way in and VNC's weak 8-character
  auth never faces the LAN. The password is generated on first install into
  `~/.config/spora-wintest/vnc-password` (override with `WT_VNC_PWFILE`).
- `DisconnectAction=0` — closing the viewer does not lock or log off, which
  would take the agent's desktop with it.
- `BlockLocalInput=0` — the agent injects input into this same session
  through SendKeys/UIA, and blocking local input would break every `wt click`.
- `RemoveWallpaper=0`, so attaching a viewer does not change what screenshots
  see.
- **`UseD3D=0`** — see below.

The local end of the tunnel defaults to port **5901**, not 5900: this
workstation already runs an `x11vnc` on 5900 for display `:99`. The forward
uses `ExitOnForwardFailure=yes`, so a busy local port fails loudly instead of
quietly leaving you looking at the wrong machine's desktop. `WT_VNC_PORT`
moves it.

#### UseD3D=0 is required on this box

With TightVNC's default D3D capture the framebuffer arrives **solid black**,
while authentication, the framebuffer geometry (1366x768) and input injection
all work normally — and while `wt shot --screen`, taken from the desktop
session at the same moment, shows a perfectly normal desktop. Measured mean
framebuffer brightness was 0.15 with D3D on and 182 with it off, confirmed by
toggling the setting back and forth. `win/vnc.ps1` therefore forces GDI
capture by writing `HKLM\SOFTWARE\TightVNC\Server\UseD3D = 0` and restarting
the service. It has to be a registry write: the MSI declares `SET_USED3D` and
`VALUE_OF_USED3D` in its Property table, but 2.8.88 never writes the value --
a fresh install passing those properties leaves `UseD3D` absent and the screen
black. If a reinstall ever shows black again, check that value first.

Note this is a different failure from the frozen `wt shot --screen` described
above: that one returns a stale but real desktop, this one returns black.


## What the first run showed

Recorded here so the next person does not rediscover it:

- The installer reused an `InstallDir` left by a February helper-era install
  (under `HKLM\SOFTWARE\WOW6432Node\Spora`, the 32-bit view NSIS writes), so
  Spora lives in `C:\Program Files (x86)\Spora` on this box. Legitimate
  upgrade behaviour; the scripts follow the registry rather than assume.
- The UI window is taller than the laptop's work area (707px frame on a 768px
  screen with a 48px taskbar); the tab bar is off-screen at winit's default
  placement.
- (Fixed by the redesign) On the old Share card only the "+" was
  interactive, and every imported `spora:` URL became a new "Imported link"
  entry even when already saved; the new UI opens the connect dialog
  prefilled for unknown links and selects/connects known ones.
- Connect flow against the sharer peer: relay session, direct upgrade
  within a second, MTU 1414, and `curl https://ifconfig.co` from the box
  answers with the peer's address.

## Box requirements and what `wt setup` changes

- OpenSSH server with PowerShell as the default shell; the SSH user in
  Administrators; this machine's key authorised.
- The test user logged on at the physical console and staying logged on.
  A locked or logged-off desktop means no agent and black screenshots. Do not
  RDP into the box while testing (a disconnected RDP session detaches the
  desktop). It is a Windows Home edition, so RDP hosting is off anyway --
  use `wt vnc`, which mirrors the console session instead of taking it over.
- `wt setup` registers the task and, unless `-NoPower` is passed through,
  sets display/standby/hibernate timeouts to never, lid close to "do nothing"
  and the screensaver off, so the desktop stays capturable.

## Sharer peer

`wt peer-push` copies the musl `spora-cli` from `../spora`'s target directory
to `/root/wintest/` on the peer (`--build` rebuilds it first).
`wt peer-share [share args]` runs `spora-cli share --json` there as the
transient systemd unit `spora-wintest-share`, with its own identity file, and
prints the share URL; `wt peer-url`, `wt peer-log` and `wt peer-stop` do what
they say. The peer is a lab machine: it runs the sharer with a dedicated
identity and process, never a production user's share.

## Layout

```
tools/wintest/
  wt                 the CLI (bash)
  README.md
  win/agent.ps1      desktop-session job runner (scheduled task)
  win/runjob.ps1     per-job wrapper: lib.ps1 + job, output capture
  win/submit.ps1     SSH-side: enqueue a job, wait, print
  win/lib.ps1        helpers available to jobs
  win/setup.ps1      task registration + power settings (elevated)
  win/deploy.ps1     swap exes into the install, cycle the service (elevated)
  win/vnc.ps1        install/configure TightVNC, loopback-only (elevated)
  peer/share.sh      runs on the peer: start the sharer unit, print the URL
```

On the box: `C:\SporaTest\{*.ps1,q\,shots\,bin\,agent.log,agent.alive}`.
