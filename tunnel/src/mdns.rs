//! LAN discovery: the router advertises `_openu60._tcp` so the app finds it without a typed
//! address, in hotspot mode and when the router is a client on the home Wi-Fi. Only real LAN
//! addresses are published: the modem, loopback aliases and link-local interfaces would
//! otherwise be offered to phones too, and a changing address set makes mDNS rename the
//! service ("U60 Pro (2)") after every restart.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use mdns_sd::{ServiceDaemon, ServiceInfo};

pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
    pub ips: Vec<IpAddr>,
}

/// Private IPv4 addresses on interfaces a phone could actually reach: not loopback, not
/// link-local, not the modem's `rmnet` interfaces, not the 192.168.253.0/24 voice aliases.
pub fn lan_ipv4_addresses() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return out };
    for iface in ifaces {
        let IpAddr::V4(ip) = iface.ip() else { continue };
        if iface.is_loopback() || ip.is_loopback() || ip.is_link_local() || !ip.is_private() {
            continue;
        }
        if iface.name.starts_with("rmnet") || iface.name.starts_with("mhi") || iface.name == "lo" {
            continue;
        }
        if ip.octets()[..3] == [192, 168, 253] {
            continue;
        }
        if !out.contains(&IpAddr::V4(ip)) {
            out.push(IpAddr::V4(ip));
        }
    }
    out.sort_by_key(|ip| match ip {
        // Hotspot gateway first, then the home-LAN address.
        IpAddr::V4(v4) if *v4 == Ipv4Addr::new(192, 168, 0, 1) => 0,
        _ => 1,
    });
    out
}

/// `instance` is the human-readable name ("U60 Pro"), `host` the mDNS host name
/// (must end with ".local."), `txt` the TXT record (id, version, ...). With an empty `ips`
/// the daemon publishes every address it can find.
pub fn advertise(instance: &str, host: &str, port: u16, txt: &[(&str, &str)], ips: &[IpAddr]) -> Result<Advertiser, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mdns daemon: {e}"))?;
    let props: HashMap<String, String> = txt.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut info = ServiceInfo::new(crate::MDNS_SERVICE, instance, host, &ips[..], port, props)
        .map_err(|e| format!("mdns service: {e}"))?;
    if ips.is_empty() {
        info = info.enable_addr_auto();
    }
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| format!("mdns register: {e}"))?;
    log::info!("advertising {fullname} on port {port} at {ips:?}");
    Ok(Advertiser { daemon, fullname, ips: ips.to_vec() })
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
