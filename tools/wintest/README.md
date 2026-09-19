# wintest — run and look at the Windows client from Linux

`tools/wintest/wt` builds the client here, pushes it to the dedicated Windows
test laptop over SSH, launches it on that machine's desktop, and brings back
screenshots, the UI Automation tree and logs. It can also start a sharer peer
on a Linux host so connect flows are real. It exists so UI changes can be
checked by whoever (or whatever) is making them, without touching the laptop.

```
wt setup                  # once per box, and after editing win/agent.ps1 (wt push for the other scripts)
wt install                # first install (silent NSIS), also for installer/upgrade tests
wt deploy                 # cargo build (release, gnu target) + swap exes into the install
wt run [spora-url]        # launch the UI on the desktop, parked top-left
wt shot [name]            # -> target/wintest/shots/<name>.png
wt tree                   # AccessKit tree with window-relative bounds
wt click 'Share Your Internet' | wt click 244 237 | wt type text | wt keys '{ENTER}' | wt paste text
wt log [--ui|--core]      # service log (%PROGRAMDATA%\Spora) and UI log (%LOCALAPPDATA%\Spora)
wt peer-share             # sharer on the lab node, prints the spora URL to feed `wt run`
wt help                   # everything else
```

Hosts come from `WT_HOST` (default `user@windows-box`, Windows 11 Home,
PowerShell 5.1 as the SSH shell, user in Administrators) and `WT_PEER`
(default `root@sharer-peer`). Screenshots land under `target/wintest/shots/`.

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

## What the first run showed

Recorded here so the next person does not rediscover it:

- The installer reused an `InstallDir` left by a February helper-era install
  (under `HKLM\SOFTWARE\WOW6432Node\Spora`, the 32-bit view NSIS writes), so
  Spora lives in `C:\Program Files (x86)\Spora` on this box. Legitimate
  upgrade behaviour; the scripts follow the registry rather than assume.
- The UI window is taller than the laptop's work area (707px frame on a 768px
  screen with a 48px taskbar); the tab bar is off-screen at winit's default
  placement.
- On the Share card only the "+" is interactive, not the card body; the
  same holds for "Connect via Friend".
- Every imported `spora:` URL becomes a new "Imported link" entry, even for
  a URL that is already saved.
- Connect flow against the sharer peer: relay session, direct upgrade
  within a second, MTU 1414, and `curl https://ifconfig.co` from the box
  answers with the peer's address.

## Box requirements and what `wt setup` changes

- OpenSSH server with PowerShell as the default shell; the SSH user in
  Administrators; this machine's key authorised.
- The test user logged on at the physical console and staying logged on.
  A locked or logged-off desktop means no agent and black screenshots. Do not
  RDP into the box while testing (a disconnected RDP session detaches the
  desktop). It is a Windows Home edition, so RDP hosting is off anyway.
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
  peer/share.sh      runs on the peer: start the sharer unit, print the URL
```

On the box: `C:\SporaTest\{*.ps1,q\,shots\,bin\,agent.log,agent.alive}`.
