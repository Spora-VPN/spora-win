# Spora for Windows

Windows client for [Spora](https://github.com/Spora-VPN/spora), a P2P VPN: share your internet connection with a peer, or connect through someone else's. All networking lives in the Rust core; this repo is the Windows service around it and the desktop UI.

- **`spora-wincore`** is a Windows service (`Spora`). It runs the tunnels with the core's `spora-core`, `spora-vpn` (the wintun adapter, routes and DNS, the same code as the CLI's `spora use`) and `spora-service` (device identity, relay access, the diagnostic log and the opt-in telemetry), and serves the UI over a gRPC API on a named pipe ([`proto/spora.proto`](spora-wincore/proto/spora.proto)).
- **`spora-winui`** is the desktop UI, in [Slint](https://slint.dev): a window with the list of connections, a notification-area icon with its flyout, and toasts. It runs unelevated and talks only to the service.

## Build

The core is a sibling checkout: `spora-wincore` depends on its crates by path (`../spora`), at the commit named in [`.github/core-ref`](.github/core-ref).

```bash
git clone https://github.com/Spora-VPN/spora
git clone https://github.com/Spora-VPN/spora-win
git -C spora checkout "$(grep -v '^#' spora-win/.github/core-ref)"
cd spora-win
cargo build
```

You need Rust and `protoc`. The workspace also builds on Linux, where the Windows-only parts are left out: the UI starts the service itself and talks to it over TCP on `[::1]:50051`. That is the quickest way to work on the UI. `SPORA_UI_DEMO=states` (and the other values listed in [`AGENTS.md`](AGENTS.md)) fills it with sample connections, without a service.

The installer is cross-compiled from Linux:

```bash
rustup target add x86_64-pc-windows-gnu
./build-winui.sh --out spora-setup.exe
```

This needs the mingw toolchain (`x86_64-w64-mingw32-windres`), NSIS (`makensis`), curl and unzip. The script downloads [Wintun](https://www.wintun.net/) and checks it against a pinned SHA-256 before putting it in the installer.

A build on Windows uses Spora's production API unless the service's environment sets `SPORA_API`, for example to `http://127.0.0.1:9` (answers nothing).

## License

[GPL-3.0](LICENSE), with an additional permission for combining Spora with Wintun: see [NOTICE](NOTICE), which also lists what the installer bundles.
