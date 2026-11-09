# CLAUDE.md — Spora Windows client

Windows desktop client for Spora, the P2P VPN whose core lives in the sibling
`../spora` repo (this repo was extracted from it in July 2026 with history via
`git filter-repo`). The repo is public, GPL-3.0 like the core: keep hosts, IP
addresses, credentials and anything from private repos or notes out of its
code, docs and commit messages. Three crates:

- **spora-wincore** — Windows service (`Spora`, auto-start, installed by the
  NSIS installer; `--service` runs the SCM entry point, without it wincore
  runs in console mode for dev). Owns the tunnel: drives
  `spora_core::share()`/`connect()`; the client's host integration is the
  shared `spora-vpn` crate from the sibling repo — the same code as
  `spora use`: wintun adapter (10.0.85.1/24 + fd00:5350::2/64), half-default
  routes, resolver = the sharer's DNS forwarder (100.64.0.53), MTU following
  the path, every outer socket bound to the uplink (`IP_UNICAST_IF`) so the
  punched peer never loops into the tunnel, DAD off on the adapter, all of
  it undone when the session drops. Exposes a gRPC control API (`proto/spora.proto`, tonic) consumed by the
  UI — served on Windows over the named pipe `\\.\pipe\spora-wincore`
  (DACL: SYSTEM/Admins full, interactive users connect-only), on Linux dev
  builds over `[::1]:50051` TCP. Sessions are tied to their event stream:
  when the client drops the stream, the session (and its routes) is torn
  down. It links the core's `spora-service` like the CLI does: the device
  identity, Spora-managed relays, the diagnostic log, connection records,
  telemetry and bug reports, all of it per Windows user, one user served at
  a time (see "Relays, shares and telemetry" below). Each user's connection
  logs live under `%PROGRAMDATA%\Spora\users\<SID>\connlog\<routing hash>`
  (proto fields control it), and a running share also sweeps the logs of
  the user's earlier shares there by retention (the core's
  `ConnLogConfig.sweep_siblings`); human-readable service log at
  `%PROGRAMDATA%\Spora\spora-wincore.log` (through `spora-winlog`). All of
  `%PROGRAMDATA%\Spora` is SYSTEM and Administrators only.
- **spora-winui** — Slint desktop UI. Pure gRPC client of wincore — no
  dependency on spora-core; its `build.rs` compiles
  `../spora-wincore/proto/spora.proto`. Runs unelevated; connects to the
  service's named pipe (on Linux dev it spawns wincore itself and uses TCP).
  Registers/handles the `spora:` URI scheme; single-instance. Its own files
  are the user's: the saved connections in
  `%APPDATA%\spora\config\connections.json` and its log,
  `%LOCALAPPDATA%\Spora\spora-winui.log` (through `spora-winlog`). The UI is the
  "master-detail instrument" from the Windows design canvas (claude.ai design
  project `Spora Windows.dc.html`, built on the Spora design system): the
  sidebar is the connection list (shape-coded status: ● active, ○ pending,
  ◆ error), the detail pane is the status surface, creating a share is an
  in-place handoff (busy pane → link ready + copy + naming), connecting goes
  through a centered dialog. The window is frameless (`no-frame`): the
  caption strip and the three window controls are ours (`ui/chrome.slint`;
  drag/menu/maximise go through the winit accessor in main.rs), edge
  resizing is Slint's own `resize-border-width`, and the DWM shadow plus
  Windows 11 corner rounding are applied to the winit window after show.
  Design tokens live in `ui/theme.slint`; IBM Plex
  Sans/Mono are bundled from `ui/fonts/` (nothing may be fetched at runtime).
  The client keeps running in the notification area when its window is
  closed: `src/tray.rs` renders the tray icon (mono mark + state shape,
  theme-aware) and its native menu, `ui/flyout.slint` is the left-click
  flyout, `src/toast.rs` posts Windows toasts (AUMID `Spora.Client`). All
  UI state is pushed from Rust in one `render()` (main.rs); sessions are
  tokio tasks that own their gRPC stream, and stopping one aborts the task
  (dropping the stream is what tears the service session down).
  Settings (the sidebar's foot) is a pane with two tabs, Relays and
  Telemetry (`ui/settings.slint`, `src/settings.rs`); it was designed in the
  window's vocabulary after the Android app's Settings, not on the design
  canvas.
  `SPORA_UI_DEMO=states|share|empty|new|creating|banner|error|linklost|settings|relaydialog|telemetry|required|norelays`
  seeds every UI state without touching the service or the saved
  connections, and `SPORA_UI_DEMO_FLYOUT=1|only` opens the flyout, for
  screenshot matrices.
- **spora-winlog** — the human-readable log files of both executables
  (`spora-wincore.log`, `spora-winui.log`), std only so the UI can link it:
  env_logger writes through `spora_winlog::open`, which redacts share-link
  secrets from every line (the rule of spora-service's diagnostic log, whose
  function is private to that crate: `/s/` plus 40 or more base64url
  characters, either case, JSON-escaped or percent-encoded slashes, and
  here `spora:s/` too; keep the two in step) and rotates the file at 1 MiB
  to `<name>.1` .. `.3`. A file left by an older version is cut to its last
  MiB and redacted when opened. wincore's console-mode stderr is redacted
  too.

## Relays, shares and telemetry

The core's `spora-service` holds the device identity (one key pair per
Windows user: each user is a device of their own), the access to Spora's
relays and the telemetry switch; wincore links it (`src/backend.rs`) and the
UI reaches it over gRPC. Core docs: `../spora/AGENTS.md`
("Identity, shares, persistence", "Relay access", "Diagnostic log and
telemetry"). The Android client (`../spora-android`) is the reference UI.

- **One Windows user at a time** (`src/users.rs`). Each pipe connection is
  tagged with the Windows user on its other end (`src/pipe.rs`: the client
  process's token user, as a SID); on Linux dev builds every caller is the
  one user `local`. Every RPC goes through `Users::enter` first: the first
  user to call is served, with their own `Backend` (a `Telemetry` instance
  opened on `users\<SID>\service`, their relay settings, records and
  connection logs), until their last connection closes. Calls from anyone
  else meanwhile fail with `PERMISSION_DENIED` and `users::IN_USE`, which
  the UI shows as its banner (the settings poll asks again every 10 s).
  Letting a user go waits for their sessions to end (they end with their
  streams), makes one last upload pass and closes their state; the
  diagnostic log file moves with the user served
  (`spora_service::diag::switch_file`), and there is none while nobody is,
  so one user's uploads and reports never carry another's activity. The UI
  holds its connection while it runs (in the tray too): a user is served
  until they quit Spora or sign out.
- **Shares are blobs.** `MakeShare` makes one on the relays the settings
  name (Spora's advised relays first, then the user's own) and returns it
  with its URL, a pure function of it; the UI stores it base64 in
  `connections.json` (`SharedConnection.share`) and starts it with
  `Share{share}`. Other relays mean a new share, a new URL. A share made
  under another device identity (the service state was lost) fails with
  `SHARE_ERROR_CODE_IDENTITY_MISMATCH`; the UI then offers MAKE A NEW LINK
  (a new share under the same entry and name, through the handoff pane).
- **Relay settings are the user's** (`src/relays.rs`,
  `users\<SID>\service\relays.json`): Spora's relays on/off (on by
  default) and own relays (`?r=` form, checked by `relays::parse` with a
  reason per error; an optional token sent as a static credential, DPAPI-
  sealed on disk and never returned to the UI). The service keeps the relay
  token fresh while Spora's relays are on or a running share uses them
  (`set_relay_access`); running shares register their credentials in the
  session store, so renewed tokens reach them. `MakeShare` fetches advice
  first only when none is cached.
- **State and secrets** (`src/paths.rs`, `src/secret.rs`): everything under
  `%ProgramData%\Spora`, which gets a protected DACL (SYSTEM +
  Administrators) at every start, and per user under `users\<SID>\`
  (`service\`, `diag\`, `records\`, `reports\`, `connlog\`). `service.json`'s
  secrets are sealed with a 32-byte state key kept in `service\state.key` as
  a DPAPI blob (LocalSystem, purpose entropy): written before first use; one
  that does not open (another machine, a restored backup) is replaced, which
  is a fresh device by design. Linux dev builds keep the same layout under
  `$SPORA_WINCORE_DATA` (else `$XDG_STATE_HOME/spora-wincore`), in the clear.
- **Diagnostics**: wincore's logger is spora-service's composite one — the
  diagnostic log file (the served user's `diag\`, JSON Lines, secrets
  redacted, crash files from the panic hook) plus the human-readable log
  (every user's). Every share and connect keeps a connection record in the
  user's `records\`. The uploader runs in the service for the user served
  (uploads off by default, per user); letting the user go, or a stop, makes
  one last pass (≤ 3 s). `ExportBugReport` zips the user's last days in the
  service and streams it; the UI
  saves it to the user's Downloads (Settings › Telemetry › SAVE REPORT). The
  account's force flag refuses a switch-off (`refused_required`) and the UI
  shows the agreed notice (also once when the service says it is pending,
  then acknowledges it). Never log a share URL: the human-readable logs
  redact share-link secrets (`spora-winlog`), but that is a safety net, not
  leave to log one.
- **Never contact production from development.** The API is `SPORA_API`
  (comma-separated, as the CLI reads it) from the service's environment,
  else the built-in production API on Windows; Linux dev builds without it
  use `http://127.0.0.1:9` (answers nothing). On the test laptop, pin it in
  the service's registry `Environment` (`HKLM\SYSTEM\CurrentControlSet\
  Services\Spora`, REG_MULTI_SZ `SPORA_API=...`) before deploying a build
  that will register.

## Data and uninstall

Everything Spora keeps on a Windows machine:

- The service's, for every Windows user: `%ProgramData%\Spora` — per user
  under `users\<SID>\` (device identity and `service.json`, `state.key`,
  `relays.json`, the diagnostic log and crash files, connection records,
  report zips in transit, the connection logs), plus `spora-wincore.log*`.
  Versions before the per-user layout kept `service\`, `diag\`,
  `records\`, `reports\` and `connlog\` directly under it.
- The UI's, per Windows user: `%APPDATA%\spora\config\connections.json`
  (`.bak` after a parse failure; it holds the share blobs, secrets
  included) and `%LOCALAPPDATA%\Spora\spora-winui.log*` (and
  `spora-wincore.log*` from versions before the service, when the UI ran
  wincore itself). The toast registration
  `HKCU\Software\Classes\AppUserModelId\Spora.Client`.
- Not ours: the spora CLI keeps its share in `%APPDATA%\spora\share.bin`
  and its state in `%LOCALAPPDATA%\spora\` — the same two folders — and
  reports the user saved (`Downloads\spora-report-*.zip`) are theirs.

The installer (`installer/spora.nsi`) upgrades and reinstalls in place: it
stops the service, replaces the files and writes a new uninstaller; it never
runs the old uninstaller and never deletes data. The uninstaller always
removes the program, the service, the shortcuts, the registry entries and
the running user's toast registration. Spora's data goes only when asked:
an interactive uninstall asks when its confirm page is left (Yes, the
default, deletes; No keeps; Cancel goes back), saying what the data
includes, the connection logs a sharer may need for an abuse report among
it; a silent one (`/S`) keeps it unless run with `/DELETEDATA`, which also
skips the question in an interactive one. Deleting removes all of
`%ProgramData%\Spora` and, for the elevating user (their `$APPDATA` /
`$LOCALAPPDATA`) and every profile in `ProfileList` (at the default
`AppData` paths), only the UI's files above, then their folders if empty,
so the CLI's files survive. Out of reach: a roaming profile's server copy,
other users' AppData redirected elsewhere, and other users' toast
registration (their hives are not loaded). A future installer that runs the
old uninstaller on upgrade must run it silently and without `/DELETEDATA`.

## Build

```bash
cargo build        # dev build works on Linux (Windows-only deps are cfg(windows)-gated)
./build-winui.sh   # release cross-compile (x86_64-pc-windows-gnu) + NSIS installer
./scripts/release.sh  # the release ceremony (below)
```

`tonic-build` requires `protoc`. `build-winui.sh` additionally needs the
`x86_64-pc-windows-gnu` rustup target, the mingw `windres` (the exes' icon and
version resources), NSIS (`makensis`), and curl/unzip; it downloads
`wintun.dll` verified against a pinned SHA-256 and writes the installer to
`~/winshare/spora-installer.exe` (`--out` elsewhere).

Licences: `LICENSE` is GPL-3.0; `NOTICE` adds the GPLv3 section 7 permission
for Wintun (the core's `NOTICE` grants the same) and lists what the installer
bundles. The installer puts `LICENSE`, `NOTICE`, Wintun's licence (taken from
the same verified zip as the DLL) and IBM Plex's OFL (`ui/fonts/OFL.txt`) in
`$INSTDIR\licenses`. Anything new the installer or the exes bundle outside
Cargo gets its licence added there and a line in `NOTICE`. CI runs
`cargo deny` (`deny.toml`, the core's list plus GPL-3.0 for Slint and the
core crates).

## Release

The version lives in one place, `[workspace.package]` in `Cargo.toml`
(`Cargo.lock` repeats it: run `cargo check` after a bump and commit both,
or `release.sh` refuses); it is stamped into both exes' version resources, the installer's file properties
and its Add/Remove Programs entry, and the service log's first line
(`spora-wincore <version> (release, core <version> <commit>)`).
`./scripts/release.sh` is the release ceremony, the counterpart of
spora-android's: it refuses unless `../spora` sits exactly at the ref in
`.github/core-ref`, confirms the version, builds with `--locked` into
`~/winshare/spora-<version>-setup.exe`, verifies the version stamp, and on
clean trees offers an annotated `v<version>` tag recording the core commit,
toolchain and installer SHA-256. Then `wt install <that file>` on the test
laptop, and upload through the website admin. No code signing yet.

## Testing on the Windows box

`tools/wintest/wt` builds the client here, pushes it to the dedicated Windows
test laptop over SSH, launches it on that machine's desktop and brings back
screenshots, the UI Automation tree and logs (`wt deploy`, `wt run`,
`wt shot`, `wt tree`, `wt click`, `wt tray`, `wt shot --flyout`, ...);
`wt peer-share` runs a sharer on a Linux peer for real connect flows. The
hosts (`WT_HOST`, `WT_PEER`) come from the environment or
`~/.config/spora-wintest/env`, never from the repo.
`wt vnc` mirrors the box's console desktop over an SSH-tunnelled, loopback-only
TightVNC for hands-on testing -- VNC rather than RDP because RDP would detach
the console session the whole agent depends on (and the box is Windows Home).
See `tools/wintest/README.md`.

## Relationship to the main repo

`spora-wincore` depends on `spora-core` and `spora-vpn` via sibling path
dependencies (`../../spora/spora-core`, `../../spora/spora-vpn`), so a checkout
of the main spora repo must sit next to this one. Protocol, transport, and connection-log documentation lives in
that repo's `CLAUDE.md` and `docs/`.
