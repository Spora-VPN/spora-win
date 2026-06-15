#![cfg_attr(windows, windows_subsystem = "windows")]

mod grpc;
mod store;

use grpc::proto::*;
use grpc::GrpcClient;
use store::Store;

use slint::Model;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

slint::include_modules!();

/// Convert a `spora:` URI or `https://spora.to/` URL to an `https://spora.to/` URL.
fn convert_spora_url(input: &str) -> Option<String> {
    if input.starts_with("https://spora.to/") {
        Some(input.to_string())
    } else if let Some(rest) = input.strip_prefix("spora:") {
        Some(format!("https://spora.to/{rest}"))
    } else {
        None
    }
}

const SINGLE_INSTANCE_PORT: u16 = 21453;

/// Active session handle returned by the gRPC streaming RPCs.
struct ActiveHandles {
    /// connection-id → gRPC handle
    share_handles: HashMap<String, i32>,
    use_handles: HashMap<String, i32>,
}

fn main() {
    // Parse URL from command-line args (e.g. "spora:s/abc?r=host:443")
    let initial_url = std::env::args()
        .skip(1)
        .find_map(|arg| convert_spora_url(&arg));

    // Single-instance enforcement: try to bind a known port.
    // If we can't bind, another instance is running — send it the URL and exit.
    let instance_listener = match std::net::TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT))
    {
        Ok(listener) => {
            listener.set_nonblocking(true).ok();
            Some(listener)
        }
        Err(_) => {
            // Another instance is running — forward the URL
            if let Some(url) = &initial_url {
                use std::io::Write;
                if let Ok(mut stream) =
                    std::net::TcpStream::connect(("127.0.0.1", SINGLE_INSTANCE_PORT))
                {
                    let _ = writeln!(stream, "{url}");
                }
            }
            std::process::exit(0);
        }
    };

    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("spora-winui.log"))
        .expect("failed to open log file");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    // Launch spora-wincore (elevated on Windows, plain on Linux for dev)
    launch_wincore();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let app = AppWindow::new().unwrap();

    let store = Arc::new(Mutex::new(Store::load()));
    let client: Arc<Mutex<Option<GrpcClient>>> = Arc::new(Mutex::new(None));
    let handles = Arc::new(Mutex::new(ActiveHandles {
        share_handles: HashMap::new(),
        use_handles: HashMap::new(),
    }));

    // Initial UI sync from store
    sync_share_list(&app, &store, &handles);
    sync_use_list(&app, &store, &handles);

    // Connect to gRPC in background
    {
        let weak = app.as_weak();
        let client = client.clone();
        rt.spawn(async move {
            loop {
                match GrpcClient::connect().await {
                    Ok(c) => {
                        *client.lock().unwrap() = Some(c);
                        let weak = weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(app) = weak.upgrade() {
                                app.set_core_connected(true);
                                app.set_status_text("Connected".into());
                            }
                        })
                        .ok();
                        break;
                    }
                    Err(e) => {
                        log::warn!("gRPC connect failed, retrying in 2s: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }

    // ── URL channel + incoming URL handling ─────────────────────────────
    {
        let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Send initial URL from command-line args
        if let Some(url) = initial_url {
            let _ = url_tx.send(url);
        }

        // Spawn TCP listener task to accept URLs from subsequent instances
        if let Some(std_listener) = instance_listener {
            let url_tx = url_tx.clone();
            rt.spawn(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener)
                    .expect("failed to convert TcpListener");
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let url_tx = url_tx.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut reader = tokio::io::BufReader::new(stream);
                        use tokio::io::AsyncBufReadExt;
                        if reader.read_until(b'\n', &mut buf).await.is_ok() {
                            let line = String::from_utf8_lossy(&buf).trim().to_string();
                            if let Some(url) = convert_spora_url(&line) {
                                let _ = url_tx.send(url);
                            }
                        }
                    });
                }
            });
        }

        // Spawn URL processor task
        let client_for_url = client.clone();
        let store_for_url = store.clone();
        let handles_for_url = handles.clone();
        let weak_for_url = app.as_weak();
        let rt_handle = rt.handle().clone();
        rt.spawn(async move {
            while let Some(url) = url_rx.recv().await {
                log::info!("Handling incoming URL: {url}");

                // Wait for gRPC client to be ready
                let grpc = loop {
                    let lock = client_for_url.lock().unwrap().clone();
                    if let Some(g) = lock {
                        break g;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                };

                // Add to store
                let url_clone = url.clone();
                let id = store_for_url
                    .lock()
                    .unwrap()
                    .add_use("Imported link".to_string(), url_clone);

                // Update UI: switch to Use tab, sync list, raise window
                let weak = weak_for_url.clone();
                let store2 = store_for_url.clone();
                let handles2 = handles_for_url.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        app.set_current_tab(1);
                        sync_use_list(&app, &store2, &handles2);
                        app.window().request_redraw();
                    }
                })
                .ok();

                // Start the connection
                let id_clone = id.clone();
                let store3 = store_for_url.clone();
                let handles3 = handles_for_url.clone();
                let weak3 = weak_for_url.clone();
                rt_handle.spawn(async move {
                    start_use(grpc, id_clone, url, store3, handles3, weak3).await;
                });
            }
        });
    }

    // ── Save share callback ──────────────────────────────────────────
    {
        let store = store.clone();
        let client = client.clone();
        let handles = handles.clone();
        let weak = app.as_weak();
        let rt_handle = rt.handle().clone();
        app.on_save_share(move |label| {
            let label = label.to_string();
            let client_lock = client.lock().unwrap().clone();
            let Some(grpc) = client_lock else { return };

            // Generate a key, save to store, start sharing
            let store = store.clone();
            let handles = handles.clone();
            let weak = weak.clone();
            rt_handle.spawn(async move {
                let key = match grpc.make_secret_key().await {
                    Ok(k) => k,
                    Err(e) => {
                        log::error!("MakeSecretKey failed: {e}");
                        return;
                    }
                };

                let id = store.lock().unwrap().add_shared(label, key.clone());

                // Update UI
                let weak2 = weak.clone();
                let store2 = store.clone();
                let handles2 = handles.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak2.upgrade() {
                        sync_share_list(&app, &store2, &handles2);
                    }
                })
                .ok();

                // Start sharing
                start_share(grpc, id, key, store, handles, weak).await;
            });
        });
    }

    // ── Save use callback ────────────────────────────────────────────
    {
        let store = store.clone();
        let client = client.clone();
        let weak = app.as_weak();
        let handles = handles.clone();
        let rt_handle = rt.handle().clone();
        app.on_save_use(move |label, url| {
            let url = url.to_string();
            let id = store.lock().unwrap().add_use(label.to_string(), url.clone());
            if let Some(app) = weak.upgrade() {
                sync_use_list(&app, &store, &handles);
            }

            let client_lock = client.lock().unwrap().clone();
            let Some(grpc) = client_lock else { return };
            let store = store.clone();
            let handles = handles.clone();
            let weak = weak.clone();
            rt_handle.spawn(async move {
                start_use(grpc, id, url, store, handles, weak).await;
            });
        });
    }

    // ── Toggle share callback ────────────────────────────────────────
    {
        let store = store.clone();
        let client = client.clone();
        let handles = handles.clone();
        let weak = app.as_weak();
        let rt_handle = rt.handle().clone();
        app.on_toggle_share(move |id| {
            let id = id.to_string();
            let client_lock = client.lock().unwrap().clone();
            let Some(grpc) = client_lock else { return };

            let is_active = handles
                .lock()
                .unwrap()
                .share_handles
                .contains_key(&id);

            if is_active {
                // Stop
                let handle_val = handles.lock().unwrap().share_handles.remove(&id);
                if let Some(h) = handle_val {
                    let grpc = grpc.clone();
                    rt_handle.spawn(async move {
                        if let Err(e) = grpc.stop_share(h).await {
                            log::error!("StopShare failed: {e}");
                        }
                    });
                }
                if let Some(app) = weak.upgrade() {
                    sync_share_list(&app, &store, &handles);
                }
            } else {
                // Start
                let key = {
                    let s = store.lock().unwrap();
                    s.shared.iter().find(|c| c.id == id).map(|c| c.secret_key.clone())
                };
                if let Some(key) = key {
                    let store = store.clone();
                    let handles = handles.clone();
                    let weak = weak.clone();
                    rt_handle.spawn(async move {
                        start_share(grpc, id, key, store, handles, weak).await;
                    });
                }
            }
        });
    }

    // ── Toggle use callback ──────────────────────────────────────────
    {
        let store = store.clone();
        let client = client.clone();
        let handles = handles.clone();
        let weak = app.as_weak();
        let rt_handle = rt.handle().clone();
        app.on_toggle_use(move |id| {
            let id = id.to_string();
            let client_lock = client.lock().unwrap().clone();
            let Some(grpc) = client_lock else { return };

            let is_active = handles
                .lock()
                .unwrap()
                .use_handles
                .contains_key(&id);

            if is_active {
                let handle_val = handles.lock().unwrap().use_handles.remove(&id);
                if let Some(h) = handle_val {
                    let grpc = grpc.clone();
                    rt_handle.spawn(async move {
                        if let Err(e) = grpc.disconnect(h).await {
                            log::error!("Disconnect failed: {e}");
                        }
                    });
                }
                if let Some(app) = weak.upgrade() {
                    sync_use_list(&app, &store, &handles);
                }
            } else {
                let url = {
                    let s = store.lock().unwrap();
                    s.uses.iter().find(|c| c.id == id).map(|c| c.url.clone())
                };
                if let Some(url) = url {
                    let store = store.clone();
                    let handles = handles.clone();
                    let weak = weak.clone();
                    rt_handle.spawn(async move {
                        start_use(grpc, id, url, store, handles, weak).await;
                    });
                }
            }
        });
    }

    // ── Delete callback ──────────────────────────────────────────────
    {
        let store = store.clone();
        let handles = handles.clone();
        let client = client.clone();
        let weak = app.as_weak();
        let rt_handle = rt.handle().clone();
        app.on_delete_connection(move |id, is_share| {
            let id = id.to_string();
            let client_lock = client.lock().unwrap().clone();

            // Stop active session if any
            if is_share {
                if let Some(h) = handles.lock().unwrap().share_handles.remove(&id)
                    && let Some(grpc) = client_lock.clone()
                {
                    rt_handle.spawn(async move {
                        let _ = grpc.stop_share(h).await;
                    });
                }
                store.lock().unwrap().remove_shared(&id);
            } else {
                if let Some(h) = handles.lock().unwrap().use_handles.remove(&id)
                    && let Some(grpc) = client_lock
                {
                    rt_handle.spawn(async move {
                        let _ = grpc.disconnect(h).await;
                    });
                }
                store.lock().unwrap().remove_use(&id);
            }

            if let Some(app) = weak.upgrade() {
                sync_share_list(&app, &store, &handles);
                sync_use_list(&app, &store, &handles);
            }
        });
    }

    // ── Rename callback ──────────────────────────────────────────────
    {
        let store = store.clone();
        let handles = handles.clone();
        let weak = app.as_weak();
        app.on_rename_connection(move |id, is_share, new_label| {
            let id = id.to_string();
            let new_label = new_label.to_string();
            let mut s = store.lock().unwrap();
            if is_share {
                s.rename_shared(&id, new_label);
            } else {
                s.rename_use(&id, new_label);
            }
            drop(s);
            if let Some(app) = weak.upgrade() {
                sync_share_list(&app, &store, &handles);
                sync_use_list(&app, &store, &handles);
            }
        });
    }

    // ── Copy to clipboard callback ───────────────────────────────────
    app.on_copy_to_clipboard(move |text| {
        let text = text.to_string();
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if let Err(e) = clipboard.set_text(&text) {
                log::error!("Clipboard set_text failed: {e}");
            }
        } else {
            log::error!("Failed to access clipboard");
        }
    });

    app.run().unwrap();

    // UI closed — shut down wincore
    rt.block_on(async {
        let grpc = client.lock().unwrap().clone();
        if let Some(grpc) = grpc {
            if let Err(e) = grpc.shutdown().await {
                log::warn!("Shutdown RPC failed: {e}");
            }
        }
    });
}

// ── Helper: sync share list from store → UI ──────────────────────────

fn sync_share_list(
    app: &AppWindow,
    store: &Arc<Mutex<Store>>,
    handles: &Arc<Mutex<ActiveHandles>>,
) {
    let s = store.lock().unwrap();
    let h = handles.lock().unwrap();
    let items: Vec<ConnectionItem> = s
        .shared
        .iter()
        .map(|c| {
            let active = h.share_handles.contains_key(&c.id);
            ConnectionItem {
                id: c.id.clone().into(),
                label: c.label.clone().into(),
                detail: Default::default(),
                status: if active { "sharing" } else { "inactive" }.into(),
                active,
            }
        })
        .collect();
    let model = std::rc::Rc::new(slint::VecModel::from(items));
    app.set_share_connections(model.into());
}

fn sync_use_list(
    app: &AppWindow,
    store: &Arc<Mutex<Store>>,
    handles: &Arc<Mutex<ActiveHandles>>,
) {
    let s = store.lock().unwrap();
    let h = handles.lock().unwrap();
    let items: Vec<ConnectionItem> = s
        .uses
        .iter()
        .map(|c| {
            let active = h.use_handles.contains_key(&c.id);
            ConnectionItem {
                id: c.id.clone().into(),
                label: c.label.clone().into(),
                detail: c.url.clone().into(),
                status: if active { "connected" } else { "inactive" }.into(),
                active,
            }
        })
        .collect();
    let model = std::rc::Rc::new(slint::VecModel::from(items));
    app.set_use_connections(model.into());
}

// ── Helper: start a share session ────────────────────────────────────

async fn start_share(
    grpc: GrpcClient,
    id: String,
    key: String,
    store: Arc<Mutex<Store>>,
    handles: Arc<Mutex<ActiveHandles>>,
    weak: slint::Weak<AppWindow>,
) {
    // Set starting status
    {
        let weak = weak.clone();
        let id = id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                set_share_status(&app, &id, "starting", false);
            }
        })
        .ok();
    }

    let mut stream = match grpc.share(key).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Share RPC failed: {e}");
            let weak = weak.clone();
            let id = id.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    set_share_status(&app, &id, "inactive", false);
                }
            })
            .ok();
            return;
        }
    };

    while let Ok(Some(event)) = stream.message().await {
        let Some(event) = event.event else { continue };
        match event {
            share_event::Event::Ready(ready) => {
                handles
                    .lock()
                    .unwrap()
                    .share_handles
                    .insert(id.clone(), ready.handle);
                let url = ready.url.clone();
                let weak = weak.clone();
                let store = store.clone();
                let handles = handles.clone();
                let id2 = id.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        sync_share_list(&app, &store, &handles);
                        set_share_detail(&app, &id2, &url);
                    }
                })
                .ok();
            }
            share_event::Event::Connected(_) => {
                let weak = weak.clone();
                let id2 = id.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        set_share_status(&app, &id2, "sharing", true);
                    }
                })
                .ok();
            }
            share_event::Event::Error(err) => {
                log::error!("Share error: {}", err.message);
                break;
            }
            share_event::Event::SessionEnded(ended) => {
                log::info!("Share session ended: {}", ended.reason);
            }
            share_event::Event::ConnLogDegraded(d) => {
                // Surfacing this in the UI is a follow-up; at minimum it must
                // not be silent.
                log::error!("Connection log degraded: {}", d.detail);
            }
        }
    }

    // Stream ended — mark inactive
    handles.lock().unwrap().share_handles.remove(&id);
    let weak = weak.clone();
    let store = store.clone();
    let handles = handles.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            sync_share_list(&app, &store, &handles);
        }
    })
    .ok();
}

// ── Helper: start a use/connect session ──────────────────────────────

async fn start_use(
    grpc: GrpcClient,
    id: String,
    url: String,
    store: Arc<Mutex<Store>>,
    handles: Arc<Mutex<ActiveHandles>>,
    weak: slint::Weak<AppWindow>,
) {
    // Set connecting status
    {
        let weak = weak.clone();
        let id = id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                set_use_status(&app, &id, "connecting", false);
            }
        })
        .ok();
    }

    let mut stream = match grpc.connect_vpn(url).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Connect RPC failed: {e}");
            let weak = weak.clone();
            let id = id.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    set_use_status(&app, &id, "inactive", false);
                }
            })
            .ok();
            return;
        }
    };

    while let Ok(Some(event)) = stream.message().await {
        let Some(event) = event.event else { continue };
        match event {
            connect_event::Event::Connected(conn) => {
                handles
                    .lock()
                    .unwrap()
                    .use_handles
                    .insert(id.clone(), conn.handle);
                let weak = weak.clone();
                let store = store.clone();
                let handles = handles.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        sync_use_list(&app, &store, &handles);
                    }
                })
                .ok();
            }
            connect_event::Event::Disconnected(d) => {
                log::info!("Disconnected: {}", d.reason);
                break;
            }
            connect_event::Event::Error(err) => {
                log::error!("Connect error: {}", err.message);
                break;
            }
        }
    }

    // Stream ended — mark inactive
    handles.lock().unwrap().use_handles.remove(&id);
    let weak = weak.clone();
    let store = store.clone();
    let handles = handles.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            sync_use_list(&app, &store, &handles);
        }
    })
    .ok();
}

// ── Helper: update a single item's status in the share model ─────────

fn set_share_status(app: &AppWindow, id: &str, status: &str, active: bool) {
    let model = app.get_share_connections();
    for i in 0..model.row_count() {
        if let Some(mut item) = model.row_data(i)
            && item.id == id
        {
            item.status = status.into();
            item.active = active;
            model.set_row_data(i, item);
            return;
        }
    }
}

fn set_share_detail(app: &AppWindow, id: &str, detail: &str) {
    let model = app.get_share_connections();
    for i in 0..model.row_count() {
        if let Some(mut item) = model.row_data(i)
            && item.id == id
        {
            item.detail = detail.into();
            model.set_row_data(i, item);
            return;
        }
    }
}

fn set_use_status(app: &AppWindow, id: &str, status: &str, active: bool) {
    let model = app.get_use_connections();
    for i in 0..model.row_count() {
        if let Some(mut item) = model.row_data(i)
            && item.id == id
        {
            item.status = status.into();
            item.active = active;
            model.set_row_data(i, item);
            return;
        }
    }
}

// ── Launch spora-wincore ─────────────────────────────────────────────

fn log_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        // %LOCALAPPDATA%\Spora
        std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()))
            .join("Spora")
    }
    #[cfg(not(windows))]
    {
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }
}

fn launch_wincore() {
    let exe_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let log_path = log_dir().join("spora-wincore.log");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // On Windows, launch elevated via PowerShell Start-Process -Verb RunAs.
        // CREATE_NO_WINDOW hides the intermediate PowerShell console.
        let exe = exe_dir.join("spora-wincore.exe");
        let _ = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-Command",
                &format!(
                    "Start-Process -FilePath '{}' -Verb RunAs -WindowStyle Hidden -ArgumentList '--log-file','{}'",
                    exe.display(),
                    log_path.display(),
                ),
            ])
            .spawn();
    }

    #[cfg(not(windows))]
    {
        // On Linux (dev), launch from same target directory
        let exe = exe_dir.join("spora-wincore");
        if exe.exists() {
            let _ = Command::new(&exe)
                .args(["--log-file", &log_path.to_string_lossy()])
                .spawn();
        } else {
            log::warn!("spora-wincore not found at {}, skipping launch", exe.display());
        }
    }

    log::info!("Wincore log file: {}", log_path.display());
}
