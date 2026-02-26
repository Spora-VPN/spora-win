#[cfg(windows)]
use log::info;

/// Detected default gateway info: gateway IP and its interface index.
#[allow(dead_code)]
pub struct DefaultGateway {
    pub gateway: String,
    pub interface_index: u32,
}

/// Detect the current default gateway using the Win32 IP Helper API.
/// Calls `GetBestRoute2` for destination 0.0.0.0 to find the route the OS would
/// use for internet traffic, returning the next-hop gateway and interface index.
#[cfg(windows)]
pub fn detect_default_gateway() -> Result<DefaultGateway, String> {
    use std::net::Ipv4Addr;
    use windows_sys::Win32::NetworkManagement::IpHelper::GetBestRoute2;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN, SOCKADDR_INET};

    unsafe {
        // Destination: 0.0.0.0 (default route)
        let mut dest: SOCKADDR_INET = std::mem::zeroed();
        dest.Ipv4.sin_family = AF_INET;
        // sin_addr left zeroed = 0.0.0.0

        let mut best_route: windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_ROW2 =
            std::mem::zeroed();
        let mut best_src: SOCKADDR_INET = std::mem::zeroed();

        let ret = GetBestRoute2(
            std::ptr::null(),  // no specific interface LUID
            0,                 // no specific interface index
            std::ptr::null(),  // no source address constraint
            &dest,
            0,                 // no sort options
            &mut best_route,
            &mut best_src,
        );

        if ret != 0 {
            return Err(format!(
                "GetBestRoute2 failed with error code {}",
                ret
            ));
        }

        // Extract next-hop gateway IP
        let next_hop: &SOCKADDR_IN = &best_route.NextHop.Ipv4;
        let gw_bytes = next_hop.sin_addr.S_un.S_addr.to_ne_bytes();
        let gw_ip = Ipv4Addr::new(gw_bytes[0], gw_bytes[1], gw_bytes[2], gw_bytes[3]);

        if gw_ip.is_unspecified() {
            return Err("GetBestRoute2 returned on-link (0.0.0.0) gateway".into());
        }

        Ok(DefaultGateway {
            gateway: gw_ip.to_string(),
            interface_index: best_route.InterfaceIndex,
        })
    }
}

#[cfg(not(windows))]
pub fn detect_default_gateway() -> Result<DefaultGateway, String> {
    Err("detect_default_gateway not available on this platform".into())
}

/// Manages Windows route table entries. On Drop, removes all added routes.
pub struct RouteManager {
    #[cfg(windows)]
    routes: Vec<RouteEntry>,
}

#[cfg(windows)]
struct RouteEntry {
    row: windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_ROW2,
}

/// Parse an IPv4 dotted-decimal string into a `u32` in network byte order.
#[cfg(windows)]
fn parse_ipv4(s: &str) -> Result<u32, String> {
    let addr: std::net::Ipv4Addr = s.parse().map_err(|e| format!("invalid IPv4 '{}': {}", s, e))?;
    Ok(u32::from_ne_bytes(addr.octets()))
}

#[allow(dead_code)]
impl RouteManager {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            routes: Vec::new(),
        }
    }

    /// Add default route (0.0.0.0/0) via the TUN adapter.
    #[cfg(windows)]
    pub fn add_default_route(&mut self, interface_index: u32) -> Result<(), String> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            CreateIpForwardEntry2, InitializeIpForwardEntry, MIB_IPFORWARD_ROW2,
        };
        use windows_sys::Win32::Foundation::ERROR_OBJECT_ALREADY_EXISTS;
        use windows_sys::Win32::Networking::WinSock::AF_INET;

        unsafe {
            let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
            InitializeIpForwardEntry(&mut row);
            row.InterfaceIndex = interface_index;
            // DestinationPrefix: 0.0.0.0/0
            row.DestinationPrefix.Prefix.Ipv4.sin_family = AF_INET;
            row.DestinationPrefix.PrefixLength = 0;
            // NextHop: 10.0.85.1 (TUN gateway)
            row.NextHop.Ipv4.sin_family = AF_INET;
            row.NextHop.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes([10, 0, 85, 1]);
            row.Metric = 5;
            let ret = CreateIpForwardEntry2(&row);
            if ret != 0 && ret != ERROR_OBJECT_ALREADY_EXISTS {
                return Err(format!("CreateIpForwardEntry2 (default route) failed: error {}", ret));
            }
            info!("Added default route via TUN IF {}", interface_index);
            self.routes.push(RouteEntry { row });
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn add_default_route(&mut self, _interface_index: u32) -> Result<(), String> {
        Ok(())
    }

    /// Add a host route for the relay IP via the original gateway to prevent routing loops.
    /// This is the Windows equivalent of Android's VpnService.protect().
    #[cfg(windows)]
    pub fn exclude_host(
        &mut self,
        ip: &str,
        original_gateway: &str,
        original_if: u32,
    ) -> Result<(), String> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            CreateIpForwardEntry2, InitializeIpForwardEntry, MIB_IPFORWARD_ROW2,
        };
        use windows_sys::Win32::Foundation::ERROR_OBJECT_ALREADY_EXISTS;
        use windows_sys::Win32::Networking::WinSock::AF_INET;

        let dest_addr = parse_ipv4(ip)?;
        let gw_addr = parse_ipv4(original_gateway)?;

        unsafe {
            let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
            InitializeIpForwardEntry(&mut row);
            row.InterfaceIndex = original_if;
            // DestinationPrefix: <ip>/32
            row.DestinationPrefix.Prefix.Ipv4.sin_family = AF_INET;
            row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr = dest_addr;
            row.DestinationPrefix.PrefixLength = 32;
            // NextHop: original gateway
            row.NextHop.Ipv4.sin_family = AF_INET;
            row.NextHop.Ipv4.sin_addr.S_un.S_addr = gw_addr;
            let ret = CreateIpForwardEntry2(&row);
            if ret != 0 && ret != ERROR_OBJECT_ALREADY_EXISTS {
                return Err(format!(
                    "CreateIpForwardEntry2 (exclude {}) failed: error {}",
                    ip, ret
                ));
            }
            info!("Added host route for {} via {} IF {}", ip, original_gateway, original_if);
            self.routes.push(RouteEntry { row });
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn exclude_host(
        &mut self,
        _ip: &str,
        _original_gateway: &str,
        _original_if: u32,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Remove all routes added by this manager.
    #[cfg(windows)]
    pub fn remove_all(&mut self) {
        use windows_sys::Win32::NetworkManagement::IpHelper::DeleteIpForwardEntry2;

        for entry in self.routes.drain(..) {
            let ret = unsafe { DeleteIpForwardEntry2(&entry.row) };
            let dest_bytes = unsafe {
                entry.row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes()
            };
            let prefix_len = entry.row.DestinationPrefix.PrefixLength;
            let dest = format!(
                "{}.{}.{}.{}/{}",
                dest_bytes[0], dest_bytes[1], dest_bytes[2], dest_bytes[3], prefix_len
            );
            if ret == 0 {
                info!("Removed route: {}", dest);
            } else {
                log::warn!("DeleteIpForwardEntry2 failed for {}: error {}", dest, ret);
            }
        }
    }

    #[cfg(not(windows))]
    pub fn remove_all(&mut self) {}
}

impl Drop for RouteManager {
    fn drop(&mut self) {
        self.remove_all();
    }
}
