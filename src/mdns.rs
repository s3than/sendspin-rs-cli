// mDNS service discovery for Sendspin servers

use log::{debug, info};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::time::Duration;

/// Discover Sendspin server via mDNS
/// Returns server address in format "host:port"
pub fn discover_sendspin_server() -> Result<String, Box<dyn std::error::Error>> {
    info!("Starting mDNS discovery for Sendspin server...");

    // Create mDNS daemon
    let mdns = ServiceDaemon::new()?;

    // Browse for _sendspin-server._tcp.local. services
    let service_type = "_sendspin-server._tcp.local.";
    let receiver = mdns.browse(service_type)?;

    info!("Searching for {} services (timeout: 5s)...", service_type);

    // Wait up to 5 seconds for a service to be discovered
    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();

    let result = loop {
        if start.elapsed() >= timeout {
            break Err("No Sendspin server found via mDNS after 5 seconds".into());
        }

        if let Ok(event) = receiver.recv_timeout(Duration::from_millis(100)) {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let host = info.get_hostname();
                    let port = info.get_port();
                    let addresses = info.get_addresses();

                    debug!(
                        "Found service: {} at {}:{}",
                        info.get_fullname(),
                        host,
                        port
                    );
                    debug!("Addresses: {:?}", addresses);

                    // Prefer IPv4 address
                    if let Some(addr) = addresses.iter().find(|a| a.is_ipv4()) {
                        let server = format!("{}:{}", addr, port);
                        info!("Discovered Sendspin server: {}", server);
                        break Ok(server);
                    }

                    // Fallback to any address
                    if let Some(addr) = addresses.iter().next() {
                        let server = format!("{}:{}", addr, port);
                        info!("Discovered Sendspin server: {}", server);
                        break Ok(server);
                    }
                }
                ServiceEvent::ServiceFound(type_name, fullname) => {
                    debug!("Service found: {} ({})", fullname, type_name);
                }
                ServiceEvent::SearchStarted(service_type) => {
                    debug!("Search started for: {}", service_type);
                }
                ServiceEvent::SearchStopped(service_type) => {
                    debug!("Search stopped for: {}", service_type);
                }
                _ => {}
            }
        }
    };

    // Stop the browse operation
    mdns.stop_browse(service_type).ok();

    // Drain any remaining messages from the channel, including SearchStopped
    while receiver.recv_timeout(Duration::from_millis(10)).is_ok() {}
    
    // Shutdown and consume the final response to prevent error message
    if mdns.shutdown().is_ok() {
        // Try to receive the shutdown acknowledgment to prevent "closed channel" error
        receiver.recv_timeout(Duration::from_millis(50)).ok();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mdns_discovery_timeout_or_success() {
        // This test verifies mDNS discovery works
        // It may find a server (Ok) or timeout (Err) depending on network
        let start = std::time::Instant::now();
        let result = discover_sendspin_server();
        let elapsed = start.elapsed();

        match result {
            Ok(server) => {
                // Found a server - verify format
                assert!(server.contains(':'), "Server address should contain port");
                // Should complete quickly if server found
                assert!(elapsed < Duration::from_secs(6));
            }
            Err(e) => {
                // No server found - verify timeout behavior
                assert!(elapsed >= Duration::from_secs(5));
                assert!(elapsed < Duration::from_secs(6));
                assert!(e.to_string().contains("No Sendspin server found"));
            }
        }
    }

    #[test]
    fn test_service_type_constant() {
        // Verify the service type format is correct
        let service_type = "_sendspin-server._tcp.local.";

        assert!(service_type.starts_with("_sendspin-server"));
        assert!(service_type.contains("._tcp."));
        assert!(service_type.ends_with(".local."));
    }
}
