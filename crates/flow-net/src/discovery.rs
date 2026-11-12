//! mDNS 服务发现：广播本机，发现局域网中的其他流界设备。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};

use flow_core::layout::DeviceId;
use flow_core::protocol::clean_name;
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc::UnboundedSender;

use crate::{NetError, SERVICE_TYPE};

/// 本机广播的信息。
#[derive(Debug, Clone)]
pub struct Advert {
    pub device: DeviceId,
    pub name: String,
    /// "macos" / "windows" / ...
    pub os: String,
    pub port: u16,
}

/// 发现的一台设备。
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub device: DeviceId,
    pub name: String,
    pub os: String,
    /// IPv4 地址优先。
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscoveryEvent {
    Found(Found),
    Lost(DeviceId),
}

pub struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
    advert: Advert,
}

/// 同时记住的 mDNS 服务名上限，远多于一个局域网里会有的流界设备数。
const MAX_TRACKED_SERVICES: usize = 256;

fn instance_name(device: &DeviceId) -> String {
    let end = device
        .0
        .char_indices()
        .nth(16)
        .map_or(device.0.len(), |(i, _)| i);
    format!("flowacross-{}", &device.0[..end])
}

fn service_info(a: &Advert) -> Result<ServiceInfo, NetError> {
    let instance = instance_name(&a.device);
    let props = HashMap::from([
        ("id".to_owned(), a.device.0.clone()),
        ("name".to_owned(), a.name.clone()),
        ("os".to_owned(), a.os.clone()),
    ]);
    Ok(ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{instance}.local."),
        "",
        a.port,
        props,
    )?
    .enable_addr_auto())
}

fn scoped_socket_addr(ip: IpAddr, port: u16, scope_id: u32) -> Option<SocketAddr> {
    if ip.is_unspecified() || ip.is_multicast() {
        return None;
    }
    match ip {
        // fe80::/10 的网卡编号来自本机收到 mDNS 的接口，不能丢弃或使用对端的编号。
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => {
            (scope_id != 0).then(|| SocketAddrV6::new(v6, port, 0, scope_id).into())
        }
        _ => Some(SocketAddr::new(ip, port)),
    }
}

fn resolved_addrs(ips: impl Iterator<Item = ScopedIp>, port: u16) -> Vec<SocketAddr> {
    let mut addrs: Vec<_> = ips
        .filter_map(|ip| {
            let scope_id = match &ip {
                ScopedIp::V6(v6) => v6.scope_id().index,
                _ => 0,
            };
            scoped_socket_addr(ip.to_ip_addr(), port, scope_id)
        })
        .collect();
    // IPv4 优先，其次非链路本地的 IPv6；相同优先级按地址排序，日志顺序稳定。
    addrs.sort_by_key(|a| {
        let priority = match a.ip() {
            IpAddr::V4(_) => 0,
            IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => 2,
            IpAddr::V6(_) => 1,
        };
        (priority, *a)
    });
    addrs.dedup();
    addrs
}

impl Discovery {
    /// 开始广播并浏览。发现/消失事件发到 `tx`（不包含本机）。
    pub fn start(advert: Advert, tx: UnboundedSender<DiscoveryEvent>) -> Result<Self, NetError> {
        let daemon = ServiceDaemon::new()?;
        let info = service_info(&advert)?;
        let fullname = info.get_fullname().to_owned();
        daemon.register(info)?;

        let rx = daemon.browse(SERVICE_TYPE)?;
        let me = advert.device.clone();
        std::thread::Builder::new()
            .name("flowacross-mdns".into())
            .spawn(move || {
                let mut by_fullname: HashMap<String, DeviceId> = HashMap::new();
                while let Ok(ev) = rx.recv() {
                    let out = match ev {
                        ServiceEvent::ServiceResolved(s) => {
                            let Some(id) = s.txt_properties.get_property_val_str("id") else {
                                continue;
                            };
                            let device = DeviceId(id.to_owned());
                            // mDNS 可以被任何人伪造：ID 不是合法指纹的直接丢弃。
                            if device == me || !device.is_well_formed() {
                                continue;
                            }
                            // 局域网里任何人都能广播任意多个服务名：记录数封顶，超出的新名字不理会。
                            if by_fullname.len() >= MAX_TRACKED_SERVICES
                                && !by_fullname.contains_key(&s.fullname)
                            {
                                continue;
                            }
                            let addrs = resolved_addrs(s.addresses.into_iter(), s.port);
                            by_fullname.insert(s.fullname.clone(), device.clone());
                            DiscoveryEvent::Found(Found {
                                device,
                                name: Some(clean_name(
                                    s.txt_properties.get_property_val_str("name").unwrap_or(""),
                                ))
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| "未命名设备".to_owned()),
                                os: clean_name(
                                    s.txt_properties.get_property_val_str("os").unwrap_or(""),
                                ),
                                addrs,
                            })
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            match by_fullname.remove(&fullname) {
                                Some(d) => DiscoveryEvent::Lost(d),
                                None => continue,
                            }
                        }
                        _ => continue,
                    };
                    if tx.send(out).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            daemon,
            fullname,
            advert,
        })
    }

    /// 设备名称变化时重新广播。
    pub fn update_name(&mut self, name: &str) -> Result<(), NetError> {
        self.advert.name = name.to_owned();
        self.daemon.register(service_info(&self.advert)?)?;
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_addresses_keep_the_local_interface() {
        let ip = "fe80::1839:cbe9:1dc2:fd28".parse().unwrap();
        assert_eq!(scoped_socket_addr(ip, 24830, 0), None);
        let wlan = scoped_socket_addr(ip, 24830, 7).unwrap();
        assert_eq!(wlan, "[fe80::1839:cbe9:1dc2:fd28%7]:24830".parse().unwrap());
        assert_ne!(wlan, scoped_socket_addr(ip, 24830, 12).unwrap());
    }

    #[test]
    fn discovery_prefers_ipv4_and_discards_unusable_addresses() {
        let ips = [
            "::",
            "0.0.0.0",
            "ff02::fb",
            "224.0.0.251",
            "fe80::1",
            "2001:db8::2",
            "192.168.5.81",
            "192.168.5.81",
        ];
        let addrs = resolved_addrs(
            ips.into_iter()
                .map(|ip| ScopedIp::from(ip.parse::<IpAddr>().unwrap())),
            24830,
        );
        assert_eq!(
            addrs,
            vec![
                "192.168.5.81:24830".parse().unwrap(),
                "[2001:db8::2]:24830".parse().unwrap()
            ]
        );
    }
}
