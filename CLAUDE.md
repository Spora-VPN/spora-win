# CLAUDE.md — Spora Windows client

Windows desktop client for Spora, the P2P VPN whose core lives in the sibling
`../spora` repo (this repo was extracted from it in July 2026 with history via
`git filter-repo`). Two crates:

- **spora-wincore** — background service. Owns the tunnel: drives
  `spora_core::share()`/`connect()`, creates the TUN device via wintun
  (client TUN address 10.0.85.1), manages routes through the Win32 IP Helper
  API. Exposes a gRPC control API (`proto/spora.proto`, tonic) consumed by the
  UI. The proto's `ShareRequest.key` field carries base64url(identity bytes),
  or empty to generate fresh. Connection log lives under
  `%PROGRAMDATA%\Spora\connlog\<routing-key>` (proto fields control it).
- **spora-winui** — Slint desktop UI. Pure gRPC client of wincore — no
  dependency on spora-core; its `build.rs` compiles
  `../spora-wincore/proto/spora.proto`. Registers/handles the `spora:` URI
  scheme; single-instance.

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

## Relationship to the main repo

`spora-wincore` depends on `spora-core` via a sibling path dependency
(`../../spora/spora-core`), so a checkout of the main spora repo must sit next
to this one. Protocol, transport, and connection-log documentation lives in
that repo's `CLAUDE.md` and `docs/`.
