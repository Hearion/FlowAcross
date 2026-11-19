//! 连接问题的诊断（纯逻辑）：把"连不上"的现象换成原因和处理办法，以及系统睡眠唤醒的判断。
//!
//! 引擎收集事实（现在还能不能在局域网里发现它、它的地址和本机的网段、是超时还是被拒绝），
//! 这里据此给出最可能的原因和按顺序排查的步骤。文案直接显示给用户。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::protocol::Os;

/// 本机的一个网络地址和它的子网前缀长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

/// 连接尝试是怎么失败的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 一个能用的地址都没有。
    NoAddress,
    /// 发出去的包没有回应，或者系统当场报网络不可达。
    Unreachable,
    /// 连上了，但对方不是要找的那台设备。
    Identity,
    /// 连上了，握手没走完。
    Handshake,
}

/// 诊断出的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// 本机没有连上任何网络。
    NoNetwork,
    /// 局域网里找不到对方：没开流界、睡眠、关机，或者网络把设备隔开了。
    NotFound,
    /// 两台电脑不在同一个网段。
    OtherNetwork,
    /// 找得到对方但连不上：防火墙、VPN / 代理的虚拟网卡。
    Blocked,
    /// 地址上的不是那台设备。
    Identity,
    /// 握手没走完：版本差得太远，或对方正忙。
    Handshake,
}

/// 给用户看的诊断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub cause: Cause,
    /// 一句话的原因。
    pub summary: String,
    /// 按顺序排查的步骤。
    pub steps: Vec<String>,
}

/// 诊断用到的事实。
#[derive(Debug, Clone)]
pub struct ConnectFacts<'a> {
    /// 对方的名称。
    pub name: &'a str,
    /// 对方的系统（不知道时为 `Os::Other`）。
    pub peer_os: Os,
    pub local_os: Os,
    pub fault: Fault,
    /// 现在还能在局域网里发现它（mDNS）。
    pub discovered: bool,
    /// 这次尝试连接的地址。
    pub peer_addrs: &'a [IpAddr],
    /// 本机的网络地址（不含回环）。
    pub local_nets: &'a [LocalNet],
    /// 对方监听的端口。
    pub port: u16,
}

fn mask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        p if p >= 32 => u32::MAX,
        p => u32::MAX << (32 - p),
    }
}

/// `peer` 是否和本机的某个 IPv4 地址在同一个子网里。
pub fn same_subnet(peer: Ipv4Addr, nets: &[LocalNet]) -> bool {
    nets.iter().any(|n| match n.addr {
        IpAddr::V4(local) => {
            let m = mask(n.prefix);
            u32::from(local) & m == u32::from(peer) & m
        }
        IpAddr::V6(_) => false,
    })
}

/// 把双栈端点给出的 `::ffff:a.b.c.d` 还原成 IPv4。
fn as_v4(addr: &IpAddr) -> Option<Ipv4Addr> {
    match addr {
        IpAddr::V4(v4) => Some(*v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    }
}

/// 对方有 IPv4 地址、本机也有 IPv4 地址，但没有一个落在同一个子网里时，返回（本机地址，对方地址）。
/// 只有 IPv6 地址时判断不了（链路本地地址不带网段信息），返回 `None`。
fn other_network(facts: &ConnectFacts) -> Option<(Ipv4Addr, Ipv4Addr)> {
    let peers: Vec<Ipv4Addr> = facts
        .peer_addrs
        .iter()
        .filter_map(as_v4)
        .filter(|a| !a.is_loopback())
        .collect();
    let local = facts.local_nets.iter().find_map(|n| match n.addr {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(_) => None,
    })?;
    let first = *peers.first()?;
    if peers.iter().any(|p| same_subnet(*p, facts.local_nets)) {
        return None;
    }
    Some((local, first))
}

/// 一次最多尝试这么多个地址（每个地址连不上要等好几秒，而自动重连总共只有约 30 秒）。
pub const MAX_CONNECT_ADDRS: usize = 8;

/// 整理要尝试连接的地址：和本机在同一个子网的排最前面，其余保持原来的顺序；去掉自动分配的
/// 169.254.x.x（没连上网络时才会有），最多留 [`MAX_CONNECT_ADDRS`] 个。
///
/// 对方常常同时广播着 VPN、虚拟机、容器网卡的地址（10.x、100.x、172.x），按数值排在
/// 192.168.x 前面：逐个去连的话，每个都要白等到超时。
pub fn order_addrs(addrs: &[SocketAddr], nets: &[LocalNet]) -> Vec<SocketAddr> {
    let v4 = |a: &SocketAddr| as_v4(&a.ip());
    let usable = |a: &&SocketAddr| !v4(a).is_some_and(|ip| ip.is_link_local());
    let near = |a: &&SocketAddr| v4(a).is_some_and(|ip| same_subnet(ip, nets));
    let mut out: Vec<SocketAddr> = addrs.iter().filter(usable).filter(near).copied().collect();
    out.extend(addrs.iter().filter(usable).filter(|a| !near(a)));
    out.truncate(MAX_CONNECT_ADDRS);
    out
}

fn firewall_step(os: Os, whose: &str, port: u16) -> String {
    match os {
        Os::Windows => format!(
            "在{whose}上打开「Windows 安全中心 › 防火墙和网络保护 › 允许应用通过防火墙」，把 FlowAcross 的「专用」和「公用」都勾上。装了第三方安全软件的话，也在里面放行流界。"
        ),
        Os::MacOs => format!(
            "在{whose}上打开「系统设置 › 网络 › 防火墙 › 选项」，关掉「阻止所有传入连接」，并把 FlowAcross 设为「允许传入连接」。"
        ),
        Os::Linux => format!(
            "在{whose}上放行流界的端口（UDP {port}）：用 ufw 的话是 sudo ufw allow {port}/udp，用 firewalld 的话是 sudo firewall-cmd --add-port={port}/udp --permanent 再 sudo firewall-cmd --reload。"
        ),
        Os::Other => format!("在{whose}的防火墙里放行流界（UDP 端口 {port}）。"),
    }
}

const VPN_STEP: &str = "两台电脑上如果开着 VPN 或代理软件，先退出它，或关掉它的「虚拟网卡 / TUN 模式」、打开「允许局域网连接」。这类软件会把发往局域网的流量也接管走。";
const ISOLATION_STEP: &str = "连的是访客 Wi‑Fi、公司或公共网络时，路由器常常不让设备之间互相访问（AP 隔离）。换到家里的主 Wi‑Fi，或者两台都接网线试试。";

/// 根据事实给出最可能的原因和排查步骤。
pub fn diagnose(facts: &ConnectFacts) -> Diagnosis {
    let name = facts.name;
    let peer = format!("「{name}」");
    match facts.fault {
        Fault::Identity => {
            return Diagnosis {
                cause: Cause::Identity,
                summary: format!("这个地址上的不是{peer}，已停止连接。"),
                steps: vec![
                    format!(
                        "{peer}的网络地址可能被别的设备占用了。重新打开两台电脑上的流界，等几秒再连接。"
                    ),
                    format!(
                        "如果{peer}重装过系统或清空过流界的数据，它的身份已经变了：在这里移除它，再重新配对。"
                    ),
                ],
            };
        }
        Fault::Handshake => {
            return Diagnosis {
                cause: Cause::Handshake,
                summary: format!("已经连上{peer}，但没能完成握手。"),
                steps: vec![
                    "把两台电脑的流界都更新到最新版本（设置 › 软件更新）。".to_owned(),
                    "重新打开两台电脑上的流界后再试。".to_owned(),
                ],
            };
        }
        Fault::NoAddress | Fault::Unreachable => {}
    }
    if facts.local_nets.is_empty() {
        return Diagnosis {
            cause: Cause::NoNetwork,
            summary: "这台电脑现在没有连上网络。".to_owned(),
            steps: vec![
                "连上 Wi‑Fi 或插好网线，流界会自动重新连接。".to_owned(),
                format!("两台电脑要在同一个局域网里，{peer}那边也看一下网络。"),
            ],
        };
    }
    if let Some((local, remote)) = other_network(facts) {
        return Diagnosis {
            cause: Cause::OtherNetwork,
            summary: format!(
                "两台电脑不在同一个网络里：这台电脑的地址是 {local}，{peer}是 {remote}。"
            ),
            steps: vec![
                "把两台电脑连到同一个路由器的同一个 Wi‑Fi（或同一个交换机）。一台连 2.4G、一台连 5G 没关系，但不能一台连主网络、一台连访客网络。".to_owned(),
                format!(
                    "{peer}换过网络的话，它的地址已经变了：确认它的流界开着，等几秒让本机重新发现它。"
                ),
                VPN_STEP.to_owned(),
            ],
        };
    }
    if facts.discovered && facts.fault == Fault::Unreachable {
        let mut steps = vec![firewall_step(facts.peer_os, &peer, facts.port)];
        steps.push(firewall_step(facts.local_os, "这台电脑", facts.port));
        steps.push(VPN_STEP.to_owned());
        steps.push(ISOLATION_STEP.to_owned());
        return Diagnosis {
            cause: Cause::Blocked,
            summary: format!(
                "能在网络里看到{peer}，但连接被拦住了，多半是防火墙或 VPN / 代理软件。"
            ),
            steps,
        };
    }
    let mut steps = vec![
        format!("确认{peer}开着机、没有睡眠，而且流界正在运行（托盘 / 菜单栏里有流界的图标）。"),
        "确认两台电脑连的是同一个 Wi‑Fi 或同一个局域网。".to_owned(),
        ISOLATION_STEP.to_owned(),
    ];
    if facts.local_os == Os::MacOs || facts.peer_os == Os::MacOs {
        steps.push(
            "Mac 上打开「系统设置 › 隐私与安全性 › 本地网络」，确认 FlowAcross 的开关开着，否则它找不到也连不上局域网里的其他电脑。".to_owned(),
        );
    }
    steps.push(VPN_STEP.to_owned());
    Diagnosis {
        cause: Cause::NotFound,
        summary: format!("在网络里找不到{peer}。"),
        steps,
    }
}

/// 版本不兼容时给用户看的说明。`peer_version` 为空时不写版本号。
pub fn version_mismatch(name: &str, peer_version: &str, local_version: &str) -> Diagnosis {
    let versions = if peer_version.is_empty() {
        String::new()
    } else {
        format!("（对方 {peer_version}，本机 {local_version}）")
    };
    Diagnosis {
        cause: Cause::Handshake,
        summary: format!("「{name}」的流界版本和这台电脑的不兼容{versions}。"),
        steps: vec![
            "把两台电脑的流界都更新到最新版本（设置 › 软件更新），更新后会自动连上。".to_owned(),
        ],
    }
}

/// 一台连着的设备为什么不在线了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// 对方的流界正常退出（或电脑关机）。
    Quit,
    /// 对方突然没了回应：睡眠、断网、断电。
    Silent,
    /// 这台电脑自己刚从睡眠中醒来。
    LocalResume,
}

/// 设备离线原因的说明（显示在设备详情里）。
pub fn offline_note(gone: Gone, name: &str) -> String {
    match gone {
        Gone::Quit => format!("「{name}」的流界已经退出。重新打开它的流界后会自动连上。"),
        Gone::Silent => format!(
            "「{name}」突然没有回应，可能睡眠了、断网了或关机了。它醒来或网络恢复后会自动连回来。"
        ),
        Gone::LocalResume => "这台电脑刚从睡眠中醒来，正在重新连接。".to_owned(),
    }
}

/// 判断系统是不是刚从睡眠中醒来：定时器本该每隔 `interval_ms` 触发一次，两次之间的
/// 墙上时钟却过去了很久，说明中间整台电脑停过。时间由调用方传入（墙上时钟的毫秒数）。
#[derive(Debug, Clone)]
pub struct ResumeDetector {
    last_ms: Option<u64>,
    threshold_ms: u64,
}

impl ResumeDetector {
    /// `interval_ms`：定时器的间隔；`slack_ms`：比间隔多出这么久才算睡过（系统忙时定时器也会晚）。
    pub fn new(interval_ms: u64, slack_ms: u64) -> Self {
        Self {
            last_ms: None,
            threshold_ms: interval_ms + slack_ms,
        }
    }

    /// 每次定时器触发时调用。刚从睡眠中醒来时返回停了多久（毫秒）。
    /// 时钟被往回拨时不算，只重新开始计时。
    pub fn tick(&mut self, wall_ms: u64) -> Option<u64> {
        let gap = self.last_ms.and_then(|last| wall_ms.checked_sub(last));
        self.last_ms = Some(wall_ms);
        gap.filter(|g| *g > self.threshold_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn home() -> Vec<LocalNet> {
        vec![LocalNet {
            addr: v4(192, 168, 1, 23),
            prefix: 24,
        }]
    }

    fn facts<'a>(
        fault: Fault,
        discovered: bool,
        peer: &'a [IpAddr],
        nets: &'a [LocalNet],
    ) -> ConnectFacts<'a> {
        ConnectFacts {
            name: "书房台式机",
            peer_os: Os::Windows,
            local_os: Os::MacOs,
            fault,
            discovered,
            peer_addrs: peer,
            local_nets: nets,
            port: 24830,
        }
    }

    #[test]
    fn subnet_matching() {
        let nets = home();
        assert!(same_subnet(Ipv4Addr::new(192, 168, 1, 200), &nets));
        assert!(!same_subnet(Ipv4Addr::new(192, 168, 31, 5), &nets));
        // /16 的网络里第三段不同也算同一个子网。
        let wide = [LocalNet {
            addr: v4(10, 0, 3, 4),
            prefix: 16,
        }];
        assert!(same_subnet(Ipv4Addr::new(10, 0, 200, 9), &wide));
        assert!(!same_subnet(Ipv4Addr::new(10, 1, 3, 4), &wide));
        // 前缀 0 和 32 不会移位溢出。
        assert_eq!(mask(0), 0);
        assert_eq!(mask(32), u32::MAX);
        assert_eq!(mask(24), 0xFFFF_FF00);
    }

    #[test]
    fn same_subnet_addresses_are_tried_first() {
        let nets = home();
        let sock = |a: &str| -> SocketAddr { a.parse().unwrap() };
        // 对方广播了 VPN、容器和自动分配的地址，按数值都排在 192.168.1.x 前面。
        let addrs = [
            sock("10.8.0.2:24830"),
            sock("169.254.3.4:24830"),
            sock("172.17.0.1:24830"),
            sock("192.168.1.50:24830"),
            sock("[fe80::1%4]:24830"),
        ];
        assert_eq!(
            order_addrs(&addrs, &nets),
            vec![
                sock("192.168.1.50:24830"),
                sock("10.8.0.2:24830"),
                sock("172.17.0.1:24830"),
                sock("[fe80::1%4]:24830"),
            ]
        );
        // 数量封顶。
        let many: Vec<SocketAddr> = (0..50).map(|i| sock(&format!("10.0.0.{i}:1"))).collect();
        assert_eq!(order_addrs(&many, &nets).len(), MAX_CONNECT_ADDRS);
        assert!(order_addrs(&[], &nets).is_empty());
    }

    #[test]
    fn discovered_but_unreachable_is_a_firewall_problem() {
        let nets = home();
        let peer = [v4(192, 168, 1, 50)];
        let d = diagnose(&facts(Fault::Unreachable, true, &peer, &nets));
        assert_eq!(d.cause, Cause::Blocked);
        // 先说对方（Windows）的防火墙怎么放行，再说本机（Mac）的。
        assert!(d.steps[0].contains("Windows 安全中心") && d.steps[0].contains("书房台式机"));
        assert!(d.steps[1].contains("系统设置 › 网络 › 防火墙") && d.steps[1].contains("这台电脑"));
        assert!(d.steps.iter().any(|s| s.contains("TUN")));
    }

    #[test]
    fn linux_firewall_step_names_the_port() {
        let nets = home();
        let peer = [v4(192, 168, 1, 50)];
        let mut f = facts(Fault::Unreachable, true, &peer, &nets);
        f.peer_os = Os::Linux;
        f.port = 40000;
        let d = diagnose(&f);
        assert!(d.steps[0].contains("sudo ufw allow 40000/udp"));
    }

    #[test]
    fn different_subnets_are_reported_with_both_addresses() {
        let nets = home();
        // 上次连接时它在 192.168.31.x，现在本机在 192.168.1.x。
        let peer = [v4(192, 168, 31, 8)];
        let d = diagnose(&facts(Fault::Unreachable, false, &peer, &nets));
        assert_eq!(d.cause, Cause::OtherNetwork);
        assert!(d.summary.contains("192.168.1.23") && d.summary.contains("192.168.31.8"));
        // 只要有一个地址在同一子网，就不算"不在同一网络"。
        let both = [v4(192, 168, 31, 8), v4(192, 168, 1, 9)];
        assert_eq!(
            diagnose(&facts(Fault::Unreachable, false, &both, &nets)).cause,
            Cause::NotFound
        );
        // 双栈端点给的 ::ffff:a.b.c.d 当 IPv4 看。
        let mapped = [IpAddr::V6(Ipv4Addr::new(192, 168, 31, 8).to_ipv6_mapped())];
        assert_eq!(
            diagnose(&facts(Fault::Unreachable, false, &mapped, &nets)).cause,
            Cause::OtherNetwork
        );
        // 只有 IPv6 链路本地地址：判断不了网段，按找不到处理。
        let v6 = ["fe80::1".parse().unwrap()];
        assert_eq!(
            diagnose(&facts(Fault::Unreachable, false, &v6, &nets)).cause,
            Cause::NotFound
        );
    }

    #[test]
    fn not_discovered_means_not_found_and_mentions_mac_permission() {
        let nets = home();
        let peer = [v4(192, 168, 1, 50)];
        let d = diagnose(&facts(Fault::Unreachable, false, &peer, &nets));
        assert_eq!(d.cause, Cause::NotFound);
        assert!(d.steps.iter().any(|s| s.contains("本地网络")));
        // 两边都不是 Mac 时不提 Mac 的权限。
        let mut f = facts(Fault::NoAddress, false, &[], &nets);
        f.local_os = Os::Windows;
        let d = diagnose(&f);
        assert_eq!(d.cause, Cause::NotFound);
        assert!(!d.steps.iter().any(|s| s.contains("本地网络")));
    }

    #[test]
    fn no_local_network_wins_over_everything_else() {
        let peer = [v4(192, 168, 1, 50)];
        let d = diagnose(&facts(Fault::Unreachable, true, &peer, &[]));
        assert_eq!(d.cause, Cause::NoNetwork);
    }

    #[test]
    fn identity_and_handshake_faults_keep_their_own_advice() {
        let nets = home();
        let peer = [v4(192, 168, 1, 50)];
        let d = diagnose(&facts(Fault::Identity, true, &peer, &nets));
        assert_eq!(d.cause, Cause::Identity);
        assert!(d.steps.iter().any(|s| s.contains("重新配对")));
        let d = diagnose(&facts(Fault::Handshake, true, &peer, &nets));
        assert_eq!(d.cause, Cause::Handshake);
        assert!(d.steps[0].contains("更新到最新版本"));
    }

    #[test]
    fn version_mismatch_shows_both_versions_when_known() {
        let d = version_mismatch("书房台式机", "0.1.2", "0.2.4");
        assert!(d.summary.contains("对方 0.1.2，本机 0.2.4"));
        assert!(
            !version_mismatch("书房台式机", "", "0.2.4")
                .summary
                .contains("对方")
        );
    }

    #[test]
    fn offline_notes_say_what_happens_next() {
        assert!(offline_note(Gone::Quit, "A").contains("已经退出"));
        assert!(offline_note(Gone::Silent, "A").contains("自动连回来"));
        assert!(offline_note(Gone::LocalResume, "A").contains("正在重新连接"));
    }

    #[test]
    fn resume_is_detected_from_a_long_wall_clock_gap() {
        let mut d = ResumeDetector::new(2_000, 13_000);
        assert_eq!(d.tick(1_000_000), None);
        assert_eq!(d.tick(1_002_000), None);
        // 系统忙，定时器晚了几秒：不算。
        assert_eq!(d.tick(1_010_000), None);
        // 睡了十分钟。
        assert_eq!(d.tick(1_610_000), Some(600_000));
        assert_eq!(d.tick(1_612_000), None);
        // 时钟被往回拨：不算，从新的时间重新开始。
        assert_eq!(d.tick(500_000), None);
        assert_eq!(d.tick(502_000), None);
    }
}
