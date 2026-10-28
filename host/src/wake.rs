//! Wake-on-LAN (слайс W): сетевые карты этого хоста для `HostRegister.ifaces`
//! и рассылка magic packet по команде сервера (`SendWake`).
//!
//! Модуль общий и о платформе не знает: перечисление интерфейсов даёт
//! `platform::netifs::raw_ifaces` (сырьё `RawIface`), а здесь живёт чистый
//! фильтр `select_ifaces` -- он решает, какие карты вообще годятся, чтобы
//! разбудить ими соседа (физические, с глобальным MAC, в нормальной подсети).

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::thread;
use std::time::Duration;

use proto::signal::{NetIface, WakeTarget};

use crate::platform;

/// Порт UDP «discard» -- стандартное назначение magic packet.
const WOL_PORT: u16 = 9;
/// Сколько раз повторять рассылку (пакет UDP без подтверждения).
const SEND_ROUNDS: usize = 3;
/// Пауза между повторами.
const ROUND_PAUSE: Duration = Duration::from_millis(100);
/// Не больше стольких карт уходит на сервер.
const MAX_IFACES: usize = 8;

/// Подстроки (в нижнем регистре) имён и описаний виртуальных интерфейсов.
const VIRTUAL_MARKERS: &[&str] = &[
    "vethernet",
    "hyper-v",
    "virtualbox",
    "vmware",
    "virtual",
    "tap-",
    "wireguard",
    "tailscale",
    "zerotier",
    "docker",
    "veth",
    "virbr",
    "br-",
    "bridge",
    "utun",
    "awdl",
    "llw",
    "vmenet",
    "anpi",
];

/// Тип канального уровня интерфейса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfaceKind {
    Ethernet,
    Wifi,
    Other,
}

/// Интерфейс в платформенно-независимом виде, как его отдаёт ОС.
#[derive(Debug, Clone)]
pub struct RawIface {
    pub name: String,
    pub mac: [u8; 6],
    pub ipv4: Ipv4Addr,
    pub prefix: u8,
    pub is_up: bool,
    pub is_loopback: bool,
    pub is_point_to_point: bool,
    pub kind: IfaceKind,
    pub description: String,
}

/// Magic packet: 6 байт 0xFF и 16 повторов MAC.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for i in 0..16 {
        packet[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    packet
}

/// Разбирает MAC вида `aa:bb:cc:dd:ee:ff` (как шлёт сервер).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for byte in mac.iter_mut() {
        let part = parts.next()?;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(mac)
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn is_virtual(name: &str, description: &str) -> bool {
    let name = name.to_lowercase();
    let description = description.to_lowercase();
    VIRTUAL_MARKERS
        .iter()
        .any(|m| name.contains(m) || description.contains(m))
}

fn iface_suits(raw: &RawIface) -> bool {
    if !raw.is_up || raw.is_loopback || raw.is_point_to_point {
        return false;
    }
    if raw.kind == IfaceKind::Other {
        return false;
    }
    // Нулевой, multicast (бит 0) и локально-администрируемый (бит 1) MAC:
    // docker, рандомизированный Wi-Fi, многие виртуальные.
    if raw.mac == [0u8; 6] || raw.mac[0] & 0x01 != 0 || raw.mac[0] & 0x02 != 0 {
        return false;
    }
    if raw.ipv4.is_loopback() || raw.ipv4.is_link_local() || raw.ipv4.is_unspecified() {
        return false;
    }
    if !(8..=30).contains(&raw.prefix) {
        return false;
    }
    !is_virtual(&raw.name, &raw.description)
}

/// Оставляет только годные для побудки карты: Ethernet раньше Wi-Fi, без
/// дублей `(mac, ipv4)`, не больше `MAX_IFACES`.
pub fn select_ifaces(raw: Vec<RawIface>) -> Vec<NetIface> {
    let mut suitable: Vec<RawIface> = raw.into_iter().filter(iface_suits).collect();
    // Сортировка стабильная: внутри типа порядок ОС сохраняется.
    suitable.sort_by_key(|r| match r.kind {
        IfaceKind::Ethernet => 0,
        _ => 1,
    });
    let mut seen: Vec<([u8; 6], Ipv4Addr)> = Vec::new();
    let mut out = Vec::new();
    for r in suitable {
        if seen.contains(&(r.mac, r.ipv4)) {
            continue;
        }
        seen.push((r.mac, r.ipv4));
        out.push(NetIface {
            mac: format_mac(r.mac),
            ipv4: r.ipv4.to_string(),
            prefix: r.prefix,
        });
        if out.len() == MAX_IFACES {
            break;
        }
    }
    out
}

/// Сетевые карты этого хоста для `HostRegister`. При ошибке перечисления --
/// пустой список (сервер тогда не трогает сохранённое).
pub fn local_ifaces() -> Vec<NetIface> {
    match platform::netifs::raw_ifaces() {
        Ok(raw) => select_ifaces(raw),
        Err(err) => {
            tracing::warn!(?err, "failed to enumerate network interfaces");
            Vec::new()
        }
    }
}

/// Рассылает magic packet по каждой цели (блокирующе; звать из
/// `spawn_blocking`). Порт -- стандартный 9.
pub fn send_wake(targets: &[WakeTarget]) {
    send_wake_to(targets, WOL_PORT);
}

/// То же с произвольным портом (для тестов). Возвращает число удавшихся
/// отправок `send_to`.
pub fn send_wake_to(targets: &[WakeTarget], port: u16) -> usize {
    let mut sent = 0usize;
    let mut valid = Vec::new();
    for target in targets {
        let Some(mac) = parse_mac(&target.mac) else {
            tracing::warn!(mac = %target.mac, "wake target has an invalid MAC, skipped");
            continue;
        };
        let Ok(broadcast) = target.broadcast.parse::<Ipv4Addr>() else {
            tracing::warn!(broadcast = %target.broadcast, "wake target has an invalid broadcast address, skipped");
            continue;
        };
        valid.push((mac, broadcast));
    }
    if !valid.is_empty() {
        match UdpSocket::bind("0.0.0.0:0").and_then(|s| s.set_broadcast(true).map(|()| s)) {
            Ok(socket) => {
                for round in 0..SEND_ROUNDS {
                    if round > 0 {
                        thread::sleep(ROUND_PAUSE);
                    }
                    for (mac, broadcast) in &valid {
                        let packet = magic_packet(*mac);
                        for dest in [*broadcast, Ipv4Addr::BROADCAST] {
                            match socket.send_to(&packet, SocketAddrV4::new(dest, port)) {
                                Ok(_) => sent += 1,
                                // На macOS отправка в локальную сеть может
                                // упираться в разрешение «Локальная сеть».
                                Err(err) => {
                                    tracing::warn!(?err, %dest, "failed to send wake packet")
                                }
                            }
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(?err, "failed to open wake UDP socket"),
        }
    }
    for (mac, broadcast) in &valid {
        tracing::debug!(mac = %format_mac(*mac), %broadcast, "wake target processed");
    }
    tracing::info!(targets = targets.len(), sent, "wake packets sent");
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

    fn good() -> RawIface {
        RawIface {
            name: "en0".into(),
            mac: GOOD_MAC,
            ipv4: Ipv4Addr::new(192, 168, 1, 10),
            prefix: 24,
            is_up: true,
            is_loopback: false,
            is_point_to_point: false,
            kind: IfaceKind::Ethernet,
            description: "Intel Ethernet".into(),
        }
    }

    #[test]
    fn magic_packet_layout() {
        let mac = [1, 2, 3, 4, 5, 6];
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert!(p[..6].iter().all(|&b| b == 0xFF));
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    #[test]
    fn parse_mac_valid_and_invalid() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse_mac("AA:BB:CC:DD:EE:FF"),
            parse_mac("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff:00"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:gg"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:f"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn select_keeps_good_iface() {
        let out = select_ifaces(vec![good()]);
        assert_eq!(
            out,
            vec![NetIface {
                mac: "00:11:22:33:44:55".into(),
                ipv4: "192.168.1.10".into(),
                prefix: 24
            }]
        );
    }

    fn rejected(mutate: impl FnOnce(&mut RawIface)) -> bool {
        let mut r = good();
        mutate(&mut r);
        select_ifaces(vec![r]).is_empty()
    }

    #[test]
    fn select_rejects_each_rule() {
        assert!(rejected(|r| r.is_up = false));
        assert!(rejected(|r| r.is_loopback = true));
        assert!(rejected(|r| r.is_point_to_point = true));
        assert!(rejected(|r| r.kind = IfaceKind::Other));
        assert!(rejected(|r| r.mac = [0; 6]));
        assert!(rejected(|r| r.mac = [0x01, 0, 0x5e, 0, 0, 1]));
        assert!(rejected(|r| r.mac = [0x02, 0x42, 0xac, 0, 0, 1]));
        assert!(rejected(|r| r.ipv4 = Ipv4Addr::LOCALHOST));
        assert!(rejected(|r| r.ipv4 = Ipv4Addr::new(169, 254, 1, 1)));
        assert!(rejected(|r| r.ipv4 = Ipv4Addr::UNSPECIFIED));
        assert!(rejected(|r| r.prefix = 7));
        assert!(rejected(|r| r.prefix = 31));
        assert!(rejected(|r| r.prefix = 32));
    }

    #[test]
    fn select_accepts_prefix_bounds() {
        assert!(!rejected(|r| r.prefix = 8));
        assert!(!rejected(|r| r.prefix = 30));
    }

    #[test]
    fn select_rejects_virtual_names_case_insensitive() {
        for marker in VIRTUAL_MARKERS {
            let upper = marker.to_uppercase();
            assert!(rejected(|r| r.name = format!("x{upper}0")), "name {marker}");
            assert!(
                rejected(|r| r.description = format!("My {upper} adapter")),
                "description {marker}"
            );
        }
        assert!(rejected(|r| r.name = "vEthernet (Default Switch)".into()));
    }

    #[test]
    fn select_orders_ethernet_before_wifi_and_dedups() {
        let mut wifi = good();
        wifi.name = "wlan0".into();
        wifi.kind = IfaceKind::Wifi;
        wifi.mac = [0x00, 0xaa, 0, 0, 0, 1];
        wifi.ipv4 = Ipv4Addr::new(192, 168, 1, 11);
        let eth = good();
        let dup = good();
        let out = select_ifaces(vec![wifi, eth, dup]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].mac, "00:11:22:33:44:55");
        assert_eq!(out[1].mac, "00:aa:00:00:00:01");
    }

    #[test]
    fn select_limits_to_eight() {
        let raw: Vec<RawIface> = (0..12u8)
            .map(|i| {
                let mut r = good();
                r.mac = [0x00, 0x11, 0, 0, 0, i];
                r.ipv4 = Ipv4Addr::new(10, 0, i, 1);
                r
            })
            .collect();
        assert_eq!(select_ifaces(raw).len(), 8);
    }

    #[test]
    fn send_wake_to_delivers_packet_over_loopback() {
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let port = receiver.local_addr().expect("addr").port();
        let targets = vec![
            WakeTarget {
                mac: "00:11:22:33:44:55".into(),
                broadcast: "127.0.0.1".into(),
            },
            WakeTarget {
                mac: "bad".into(),
                broadcast: "127.0.0.1".into(),
            },
            WakeTarget {
                mac: "00:11:22:33:44:55".into(),
                broadcast: "not-an-ip".into(),
            },
        ];
        let sent = send_wake_to(&targets, port);
        assert!(sent >= SEND_ROUNDS, "sent {sent}");
        let mut buf = [0u8; 256];
        let (n, _) = receiver.recv_from(&mut buf).expect("receive packet");
        assert_eq!(&buf[..n], &magic_packet(GOOD_MAC));
    }

    #[test]
    fn send_wake_with_no_targets_does_not_panic() {
        send_wake(&[]);
    }

    #[test]
    fn local_ifaces_does_not_panic() {
        let _ = local_ifaces();
    }
}
