//! Wake-on-LAN (слайс W): чистая логика. Валидация сетевых карт, которые
//! присылает хост, и выбор «будильников» — онлайн-хостов владельца в той же
//! подсети, что и выключенная цель. Ввода-вывода здесь нет.

use std::net::Ipv4Addr;

use proto::signal::{NetIface, WakeTarget};

/// Сколько карт принимаем от одного хоста.
pub const MAX_IFACES: usize = 8;

/// Минимальный интервал между попытками включить одно устройство.
pub const WAKE_COOLDOWN_SECS: u64 = 10;

/// Разбирает MAC из шести hex-октетов, разделённых `:` или `-`; возвращает
/// канонический вид `aa:bb:cc:dd:ee:ff` (нижний регистр). Нулевой,
/// широковещательный и multicast-адреса (младший бит первого октета)
/// отвергаются.
pub fn normalize_mac(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut octets = [0u8; 6];
    for (slot, part) in octets.iter_mut().zip(&parts) {
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    if octets == [0; 6] || octets == [0xff; 6] || octets[0] & 1 == 1 {
        return None;
    }
    Some(format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        octets[0], octets[1], octets[2], octets[3], octets[4], octets[5]
    ))
}

/// Валидирует карты от хоста: берёт не более `MAX_IFACES` (лишние
/// отбрасываются), невалидные отбрасывает молча (debug-лог). Возвращает
/// карты с нормализованным MAC.
pub fn validate_ifaces(raw: &[NetIface]) -> Vec<NetIface> {
    let mut out = Vec::new();
    for iface in raw.iter().take(MAX_IFACES) {
        let Some(mac) = normalize_mac(&iface.mac) else {
            tracing::debug!("wake: dropped iface with invalid mac");
            continue;
        };
        let Ok(ip) = iface.ipv4.trim().parse::<Ipv4Addr>() else {
            tracing::debug!("wake: dropped iface with invalid ipv4");
            continue;
        };
        if ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_link_local()
            || ip.is_multicast()
            || ip.is_broadcast()
        {
            tracing::debug!(%ip, "wake: dropped iface with unusable ipv4");
            continue;
        }
        if !(8..=30).contains(&iface.prefix) {
            tracing::debug!(prefix = iface.prefix, "wake: dropped iface with bad prefix");
            continue;
        }
        out.push(NetIface {
            mac,
            ipv4: ip.to_string(),
            prefix: iface.prefix,
        });
    }
    out
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

/// (сеть, широковещательный адрес) карты; `None`, если IPv4 не разбирается
/// (в БД лежат уже проверенные, но защищаемся).
fn network(iface: &NetIface) -> Option<(u32, u8, Ipv4Addr)> {
    let ip: Ipv4Addr = iface.ipv4.parse().ok()?;
    let m = mask(iface.prefix);
    let net = u32::from(ip) & m;
    Some((net, iface.prefix, Ipv4Addr::from(u32::from(ip) | !m)))
}

/// Выбирает будильников для цели. `target` — карты цели, `candidates` —
/// (device_id, карты) онлайн-кандидатов. Кандидат — будильник, если одна из
/// его карт в той же сети (адрес & маска при одинаковом префиксе), что одна из
/// карт цели. Для каждого будильника `targets` — карты цели с совпавшими
/// сетями (без дублей mac+broadcast). Порядок результата — как у кандидатов.
pub fn select_wakers(
    target: &[NetIface],
    candidates: &[(String, Vec<NetIface>)],
) -> Vec<(String, Vec<WakeTarget>)> {
    let target_nets: Vec<(&NetIface, (u32, u8, Ipv4Addr))> = target
        .iter()
        .filter_map(|iface| network(iface).map(|n| (iface, n)))
        .collect();
    let mut result = Vec::new();
    for (device_id, ifaces) in candidates {
        let cand_nets: Vec<(u32, u8)> = ifaces
            .iter()
            .filter_map(|iface| network(iface).map(|(net, prefix, _)| (net, prefix)))
            .collect();
        let mut targets: Vec<WakeTarget> = Vec::new();
        for (iface, (net, prefix, broadcast)) in &target_nets {
            if !cand_nets.contains(&(*net, *prefix)) {
                continue;
            }
            let wt = WakeTarget {
                mac: iface.mac.clone(),
                broadcast: broadcast.to_string(),
            };
            if !targets.contains(&wt) {
                targets.push(wt);
            }
        }
        if !targets.is_empty() {
            result.push((device_id.clone(), targets));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(mac: &str, ip: &str, prefix: u8) -> NetIface {
        NetIface {
            mac: mac.to_string(),
            ipv4: ip.to_string(),
            prefix,
        }
    }

    const MAC: &str = "aa:bb:cc:dd:ee:01";

    #[test]
    fn mac_is_normalized_to_lowercase_colons() {
        assert_eq!(
            normalize_mac("AA-BB-CC-DD-EE-0F").as_deref(),
            Some("aa:bb:cc:dd:ee:0f")
        );
        assert_eq!(normalize_mac(MAC).as_deref(), Some(MAC));
    }

    #[test]
    fn mac_rejects_malformed_zero_broadcast_and_multicast() {
        assert!(normalize_mac("aa:bb:cc:dd:ee").is_none());
        assert!(normalize_mac("aa:bb:cc:dd:ee:ff:00").is_none());
        assert!(normalize_mac("aa:bb:cc:dd:ee:zz").is_none());
        assert!(normalize_mac("a:bb:cc:dd:ee:01").is_none());
        assert!(normalize_mac("00:00:00:00:00:00").is_none());
        assert!(normalize_mac("ff:ff:ff:ff:ff:ff").is_none());
        assert!(normalize_mac("01:00:5e:00:00:01").is_none());
        assert!(normalize_mac("").is_none());
    }

    #[test]
    fn validate_drops_bad_ipv4_and_prefix() {
        let raw = vec![
            iface(MAC, "192.168.1.10", 24),
            iface(MAC, "10.0.0.5", 8),
            iface(MAC, "10.0.0.6", 30),
            iface(MAC, "not-an-ip", 24),
            iface(MAC, "127.0.0.1", 8),
            iface(MAC, "0.0.0.0", 24),
            iface(MAC, "169.254.1.1", 16),
            iface(MAC, "224.0.0.1", 24),
            iface(MAC, "255.255.255.255", 24),
            iface(MAC, "192.168.1.10", 7),
            iface(MAC, "192.168.1.10", 31),
            iface("bad", "192.168.1.11", 24),
        ];
        let out = validate_ifaces(&raw);
        assert_eq!(
            out,
            vec![
                iface(MAC, "192.168.1.10", 24),
                iface(MAC, "10.0.0.5", 8),
                iface(MAC, "10.0.0.6", 30),
            ]
        );
    }

    #[test]
    fn validate_normalizes_mac_and_caps_count() {
        let out = validate_ifaces(&[iface("AA:BB:CC:DD:EE:01", "10.1.1.1", 24)]);
        assert_eq!(out[0].mac, "aa:bb:cc:dd:ee:01");

        let many: Vec<NetIface> = (0..12)
            .map(|i| iface(&format!("aa:bb:cc:dd:ee:{i:02x}"), "10.1.1.1", 24))
            .collect();
        assert_eq!(validate_ifaces(&many).len(), MAX_IFACES);
    }

    #[test]
    fn broadcast_for_24_and_22() {
        let c = vec![(
            "w".to_string(),
            vec![iface("aa:aa:aa:aa:aa:02", "192.168.1.5", 24)],
        )];
        let r = select_wakers(&[iface(MAC, "192.168.1.77", 24)], &c);
        assert_eq!(r[0].1[0].broadcast, "192.168.1.255");

        let c = vec![(
            "w".to_string(),
            vec![iface("aa:aa:aa:aa:aa:02", "10.0.5.9", 22)],
        )];
        let r = select_wakers(&[iface(MAC, "10.0.6.77", 22)], &c);
        assert_eq!(r[0].1[0].broadcast, "10.0.7.255");
        assert_eq!(r[0].1[0].mac, MAC);
    }

    #[test]
    fn different_subnet_or_prefix_is_not_a_waker() {
        let target = [iface(MAC, "192.168.1.77", 24)];
        let other_net = vec![(
            "w".to_string(),
            vec![iface("aa:aa:aa:aa:aa:02", "192.168.2.5", 24)],
        )];
        assert!(select_wakers(&target, &other_net).is_empty());
        // Одинаковая сеть по адресу, но разные префиксы -- не совпадение.
        let other_prefix = vec![(
            "w".to_string(),
            vec![iface("aa:aa:aa:aa:aa:02", "192.168.1.5", 16)],
        )];
        assert!(select_wakers(&target, &other_prefix).is_empty());
        assert!(select_wakers(&[], &other_net).is_empty());
    }

    #[test]
    fn targets_are_only_matching_cards_without_duplicates() {
        let target = [
            iface("aa:aa:aa:aa:aa:01", "192.168.1.77", 24),
            iface("aa:aa:aa:aa:aa:03", "10.9.9.9", 24),
            // Та же карта дважды (две адреса в одной сети).
            iface("aa:aa:aa:aa:aa:01", "192.168.1.78", 24),
        ];
        let cands = vec![
            (
                "w1".to_string(),
                vec![
                    iface("bb:bb:bb:bb:bb:01", "192.168.1.5", 24),
                    iface("bb:bb:bb:bb:bb:02", "192.168.1.6", 24),
                ],
            ),
            (
                "w2".to_string(),
                vec![iface("cc:cc:cc:cc:cc:01", "10.9.9.1", 24)],
            ),
            ("w3".to_string(), vec![]),
        ];
        let r = select_wakers(&target, &cands);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "w1");
        assert_eq!(
            r[0].1,
            vec![WakeTarget {
                mac: "aa:aa:aa:aa:aa:01".to_string(),
                broadcast: "192.168.1.255".to_string()
            }]
        );
        assert_eq!(r[1].0, "w2");
        assert_eq!(r[1].1[0].mac, "aa:aa:aa:aa:aa:03");
    }
}
