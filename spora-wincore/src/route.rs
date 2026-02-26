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
    destination: String,
    mask: String,
    gateway: String,
    interface_index: Option<u32>,
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
        let status = std::process::Command::new("route")
            .args([
                "ADD", "0.0.0.0", "MASK", "0.0.0.0", "10.0.85.1",
                "METRIC", "5", "IF", &interface_index.to_string(),
            ])
            .status()
            .map_err(|e| format!("route ADD failed: {}", e))?;
        if !status.success() {
            return Err(format!("route ADD exited with {}", status));
        }
        info!("Added default route via TUN IF {}", interface_index);
        self.routes.push(RouteEntry {
            destination: "0.0.0.0".into(),
            mask: "0.0.0.0".into(),
            gateway: "10.0.85.1".into(),
            interface_index: Some(interface_index),
        });
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
        let status = std::process::Command::new("route")
            .args([
                "ADD", ip, "MASK", "255.255.255.255", original_gateway,
                "IF", &original_if.to_string(),
            ])
            .status()
            .map_err(|e| format!("route ADD (exclude) failed: {}", e))?;
        if !status.success() {
            return Err(format!("route ADD (exclude) exited with {}", status));
        }
        info!("Added host route for {} via {} IF {}", ip, original_gateway, original_if);
        self.routes.push(RouteEntry {
            destination: ip.into(),
            mask: "255.255.255.255".into(),
            gateway: original_gateway.into(),
            interface_index: Some(original_if),
        });
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
        for entry in self.routes.drain(..) {
            let mut args = vec![
                "DELETE".to_string(),
                entry.destination,
                "MASK".to_string(),
                entry.mask,
                entry.gateway,
            ];
            if let Some(idx) = entry.interface_index {
                args.push("IF".into());
                args.push(idx.to_string());
            }
            let result = std::process::Command::new("route")
                .args(&args)
                .status();
            match result {
                Ok(status) if status.success() => {
                    info!("Removed route: {}", args.join(" "));
                }
                Ok(status) => {
                    log::warn!("route DELETE exited with {}: {}", status, args.join(" "));
                }
                Err(e) => {
                    log::warn!("route DELETE failed: {}: {}", e, args.join(" "));
                }
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
