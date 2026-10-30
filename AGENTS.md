# CLAUDE.md — Spora Windows client

Windows desktop client for Spora, the P2P VPN whose core lives in the sibling
`../spora` repo (this repo was extracted from it in July 2026 with history via
`git filter-repo`). Two crates:

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
  telemetry and bug reports (see "Relays, shares and telemetry" below).
  Connection log lives under `%PROGRAMDATA%\Spora\connlog\<routing hash>`
  (proto fields control it); human-readable service log at
  `%PROGRAMDATA%\Spora\spora-wincore.log`.
- **spora-winui** — Slint desktop UI. Pure gRPC client of wincore — no
  dependency on spora-core; its `build.rs` compiles
  `../spora-wincore/proto/spora.proto`. Runs unelevated; connects to the
  service's named pipe (on Linux dev it spawns wincore itself and uses TCP).
  Registers/handles the `spora:` URI scheme; single-instance. The UI is the
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

## Relays, shares and telemetry

The core's `spora-service` holds the device identity (one key pair per
machine), the access to Spora's relays and the telemetry switch; wincore
links it (`src/backend.rs`) and the UI reaches it over gRPC. Core docs: `../spora/AGENTS.md`
("Identity, shares, persistence", "Relay access", "Diagnostic log and
telemetry"). The Android client (`../spora-android`) is the reference UI.

- **Shares are blobs.** `MakeShare` makes one on the relays the settings
  name (Spora's advised relays first, then the user's own) and returns it
  with its URL, a pure function of it; the UI stores it base64 in
  `connections.json` (`SharedConnection.share`) and starts it with
  `Share{share}`. Other relays mean a new share, a new URL. A share made
  under another device identity (the service state was lost) fails with
  `SHARE_ERROR_CODE_IDENTITY_MISMATCH`; the UI then offers MAKE A NEW LINK
  (a new share under the same entry and name, through the handoff pane).
- **Relay settings are the service's, machine-wide** (`src/relays.rs`,
  `%ProgramData%\Spora\service\relays.json`): Spora's relays on/off (on by
  default) and own relays (`?r=` form, checked by `relays::parse` with a
  reason per error; an optional token sent as a static credential, DPAPI-
  sealed on disk and never returned to the UI). The service keeps the relay
  token fresh while Spora's relays are on or a running share uses them
  (`set_relay_access`); running shares register their credentials in the
  session store, so renewed tokens reach them. `MakeShare` fetches advice
  first only when none is cached.
- **State and secrets** (`src/paths.rs`, `src/secret.rs`): everything under
  `%ProgramData%\Spora`; `service\`, `diag\`, `records\` and `reports\` get a
  protected DACL (SYSTEM + Administrators). `service.json`'s secrets are
  sealed with a 32-byte state key kept in `service\state.key` as a DPAPI blob
  (LocalSystem, purpose entropy): written before first use; one that does
  not open (another machine, a restored backup) is replaced, which is a
  fresh device by design. Linux dev builds keep the same layout under
  `$SPORA_WINCORE_DATA` (else `$XDG_STATE_HOME/spora-wincore`), in the clear.
- **Diagnostics**: wincore's logger is spora-service's composite one — the
  diagnostic log file (`diag\`, JSON Lines, secrets redacted, crash files
  from the panic hook) plus the human-readable log. Every share and connect
  keeps a connection record in `records\`. The uploader runs in the service
  (uploads off by default, per machine); a stop makes one last pass (≤ 3 s).
  `ExportBugReport` zips the last days in the service and streams it; the UI
  saves it to the user's Downloads (Settings › Telemetry › SAVE REPORT). The
  account's force flag refuses a switch-off (`refused_required`) and the UI
  shows the agreed notice (also once when the service says it is pending,
  then acknowledges it). Never log a share URL: the human-readable log is
  not redacted.
- **Never contact production from development.** The API is `SPORA_API`
  (comma-separated, as the CLI reads it) from the service's environment,
  else the built-in production API on Windows; Linux dev builds without it
  use `http://127.0.0.1:9` (answers nothing). On the test laptop, pin it in
  the service's registry `Environment` (`HKLM\SYSTEM\CurrentControlSet\
  Services\Spora`, REG_MULTI_SZ `SPORA_API=...`) before deploying a build
  that will register.

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

## Release

The version lives in one place, `[workspace.package]` in `Cargo.toml`; it is
stamped into both exes' version resources, the installer's file properties
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
`wt peer-share` runs a sharer on a Linux host for real connect flows.
`wt vnc` mirrors the box's console desktop over an SSH-tunnelled, loopback-only
TightVNC for hands-on testing -- VNC rather than RDP because RDP would detach
the console session the whole agent depends on (and the box is Windows Home).
See `tools/wintest/README.md`.

## Relationship to the main repo

`spora-wincore` depends on `spora-core` and `spora-vpn` via sibling path
dependencies (`../../spora/spora-core`, `../../spora/spora-vpn`), so a checkout
of the main spora repo must sit next to this one. Protocol, transport, and connection-log documentation lives in
that repo's `CLAUDE.md` and `docs/`.
