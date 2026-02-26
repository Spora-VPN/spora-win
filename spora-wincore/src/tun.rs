#[cfg(windows)]
use log::{error, info};
use spora_core::IpTransport;

/// WinTUN adapter wrapper. On Windows, manages a wintun adapter session.
/// On non-Windows, all methods are stubs that return errors.
pub struct TunDevice {
    #[cfg(windows)]
    session: std::sync::Arc<wintun::Session>,
    #[cfg(windows)]
    _adapter: std::sync::Arc<wintun::Adapter>,
    #[cfg(windows)]
    interface_index: u32,
}

#[allow(dead_code)]
impl TunDevice {
    /// Create a WinTUN adapter with the given name and MTU.
    /// Configures the adapter with IP 10.0.85.1/24 via Win32 API.
    #[cfg(windows)]
    pub fn create(name: &str, mtu: u32) -> Result<Self, String> {
        let dll_path = std::env::current_exe()
            .map_err(|e| format!("failed to get exe path: {}", e))?
            .parent()
            .ok_or("exe has no parent directory")?
            .join("wintun.dll");
        let wintun = unsafe { wintun::load_from_path(&dll_path) }
            .map_err(|e| format!("failed to load {}: {}", dll_path.display(), e))?;
        let adapter = wintun::Adapter::create(&wintun, name, "Spora", None)
            .map_err(|e| format!("failed to create adapter: {}", e))?;

        let interface_index = adapter
            .get_adapter_index()
            .map_err(|e| format!("failed to get adapter index: {}", e))?;

        // Configure IP address via Win32 API
        unsafe {
            use windows_sys::Win32::NetworkManagement::IpHelper::{
                CreateUnicastIpAddressEntry, InitializeUnicastIpAddressEntry,
                MIB_UNICASTIPADDRESS_ROW,
            };
            use windows_sys::Win32::Networking::WinSock::AF_INET;

            let mut row: MIB_UNICASTIPADDRESS_ROW = std::mem::zeroed();
            InitializeUnicastIpAddressEntry(&mut row);
            row.InterfaceIndex = interface_index;
            row.Address.Ipv4.sin_family = AF_INET;
            row.Address.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes([10, 0, 85, 1]);
            row.OnLinkPrefixLength = 24;
            let ret = CreateUnicastIpAddressEntry(&row);
            if ret != 0 {
                return Err(format!("CreateUnicastIpAddressEntry failed: error {}", ret));
            }
        }

        // Set MTU via Win32 API
        set_mtu(interface_index, mtu)?;

        let session = std::sync::Arc::new(
            adapter
                .start_session(wintun::MAX_RING_CAPACITY)
                .map_err(|e| format!("failed to start session: {}", e))?,
        );

        info!("WinTUN adapter '{}' created with MTU {}, IF index {}", name, mtu, interface_index);
        Ok(Self { session, _adapter: adapter, interface_index })
    }

    #[cfg(not(windows))]
    pub fn create(name: &str, _mtu: u32) -> Result<Self, String> {
        Err(format!("TUN device '{}' not available on this platform", name))
    }

    /// Returns the Win32 interface index for this TUN adapter.
    #[cfg(windows)]
    pub fn interface_index(&self) -> u32 {
        self.interface_index
    }

    #[cfg(not(windows))]
    pub fn interface_index(&self) -> u32 {
        0
    }
}

/// Update an adapter's MTU via Win32 IP Helper API. Can be called independently of TunDevice
/// (e.g. from an MTU callback closure).
#[cfg(windows)]
pub fn set_mtu(interface_index: u32, mtu: u32) -> Result<(), String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetIpInterfaceEntry, MIB_IPINTERFACE_ROW, SetIpInterfaceEntry,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    unsafe {
        let mut row: MIB_IPINTERFACE_ROW = std::mem::zeroed();
        row.Family = AF_INET;
        row.InterfaceIndex = interface_index;
        let ret = GetIpInterfaceEntry(&mut row);
        if ret != 0 {
            return Err(format!("GetIpInterfaceEntry failed: error {}", ret));
        }
        row.NlMtu = mtu;
        row.SitePrefixLength = 0; // required for IPv4, see SetIpInterfaceEntry docs
        let ret = SetIpInterfaceEntry(&mut row);
        if ret != 0 {
            return Err(format!("SetIpInterfaceEntry failed: error {}", ret));
        }
    }
    info!("TUN MTU updated to {}", mtu);
    Ok(())
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn set_mtu(_interface_index: u32, _mtu: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
impl Drop for TunDevice {
    fn drop(&mut self) {
        info!("Shutting down WinTUN session");
        let _ = self.session.shutdown();
    }
}

/// Bridge packets between a spora-core transport and a WinTUN device.
///
/// Uses two OS threads for blocking wintun read/write, bridged to the async
/// transport via mpsc channels — same pattern as tun_util::start_fd.
#[cfg(windows)]
pub async fn run_tunnel(transport: IpTransport, tun: &TunDevice) -> Result<(), String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::mpsc;

    let (tun_tx, mut tun_rx) = mpsc::channel::<Vec<u8>>(256);
    let (transport_tx, mut transport_rx) = mpsc::channel::<Vec<u8>>(256);

    // OS thread: read from TUN → tun_tx
    let reader_session: std::sync::Arc<wintun::Session> = tun.session.clone();
    let read_handle = std::thread::spawn(move || {
        loop {
            match reader_session.receive_blocking() {
                Ok(packet) => {
                    let data: Vec<u8> = packet.bytes().to_vec();
                    if tun_tx.blocking_send(data).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("TUN read error: {}", e);
                    break;
                }
            }
        }
    });

    // OS thread: write transport packets → TUN
    let writer_session: std::sync::Arc<wintun::Session> = tun.session.clone();
    let write_handle = std::thread::spawn(move || {
        while let Some(data) = transport_rx.blocking_recv() {
            match writer_session.allocate_send_packet(data.len() as u16) {
                Ok(mut packet) => {
                    let buf: &mut [u8] = packet.bytes_mut();
                    buf.copy_from_slice(&data);
                    writer_session.send_packet(packet);
                }
                Err(e) => {
                    error!("TUN write error: {}", e);
                    break;
                }
            }
        }
    });

    // Async bridge: transport ↔ mpsc channels
    let (mut sink, mut stream) = transport.split();

    tokio::select! {
        // TUN → transport
        _ = async {
            while let Some(pkt) = tun_rx.recv().await {
                if sink.send(pkt).await.is_err() {
                    break;
                }
            }
        } => {}
        // transport → TUN
        _ = async {
            while let Some(Ok(pkt)) = stream.next().await {
                if transport_tx.send(pkt).await.is_err() {
                    break;
                }
            }
        } => {}
    }

    drop(tun_rx);
    drop(transport_tx);
    let _ = read_handle.join();
    let _ = write_handle.join();

    Ok(())
}

#[cfg(not(windows))]
pub async fn run_tunnel(_transport: IpTransport, _tun: &TunDevice) -> Result<(), String> {
    Err("TUN tunnel not available on this platform".into())
}
