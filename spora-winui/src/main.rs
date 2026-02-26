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

/// Active session handle returned by the gRPC streaming RPCs.
struct ActiveHandles {
    /// connection-id → gRPC handle
    share_handles: HashMap<String, i32>,
    use_handles: HashMap<String, i32>,
}

fn main() {
    env_logger::init();

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
        let weak = app.as_weak();
        let handles = handles.clone();
        app.on_save_use(move |label, url| {
            let mut s = store.lock().unwrap();
            s.add_use(label.to_string(), url.to_string());
            if let Some(app) = weak.upgrade() {
                sync_use_list(&app, &store, &handles);
            }
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

fn launch_wincore() {
    let exe_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let log_path = exe_dir.join("spora-wincore.log");

    #[cfg(windows)]
    {
        // On Windows, launch elevated via PowerShell Start-Process -Verb RunAs
        // Redirect stdout+stderr to a log file for debugging.
        let exe = exe_dir.join("spora-wincore.exe");
        let _ = Command::new("powershell")
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
