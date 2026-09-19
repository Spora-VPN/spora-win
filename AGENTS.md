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
  down. The proto's `ShareRequest.key` field carries base64url(identity
  bytes), or empty to generate fresh. Connection log lives under
  `%PROGRAMDATA%\Spora\connlog\<routing-key>` (proto fields control it);
  service log at `%PROGRAMDATA%\Spora\spora-wincore.log`.
- **spora-winui** — Slint desktop UI. Pure gRPC client of wincore — no
  dependency on spora-core; its `build.rs` compiles
  `../spora-wincore/proto/spora.proto`. Runs unelevated; connects to the
  service's named pipe (on Linux dev it spawns wincore itself and uses TCP).
  Registers/handles the `spora:` URI scheme; single-instance.

## Build

```bash
cargo build        # dev build works on Linux (Windows-only deps are cfg(windows)-gated)
./build-winui.sh   # release cross-compile (x86_64-pc-windows-gnu) + NSIS installer
```

`tonic-build` requires `protoc`. `build-winui.sh` additionally needs the
`x86_64-pc-windows-gnu` rustup target, ImageMagick (`convert`, icon from
`assets/logo.svg`), NSIS (`makensis`), and curl/unzip; it downloads
`wintun.dll` verified against a pinned SHA-256 and writes the installer to
`~/winshare/spora-installer.exe`.

## Testing on the Windows box

`tools/wintest/wt` builds the client here, pushes it to the dedicated Windows
test laptop over SSH, launches it on that machine's desktop and brings back
screenshots, the UI Automation tree and logs (`wt deploy`, `wt run`,
`wt shot`, `wt tree`, `wt click`, ...); `wt peer-share` runs a sharer on a
Linux host for real connect flows. See `tools/wintest/README.md`.

## Relationship to the main repo

`spora-wincore` depends on `spora-core` and `spora-vpn` via sibling path
dependencies (`../../spora/spora-core`, `../../spora/spora-vpn`), so a checkout
of the main spora repo must sit next to this one. Protocol, transport, and connection-log documentation lives in
that repo's `CLAUDE.md` and `docs/`.
