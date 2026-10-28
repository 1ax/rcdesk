//! Перечисление сетевых интерфейсов на unix (macOS, Linux) через
//! `libc::getifaddrs` для Wake-on-LAN (слайс W). Общий код видит только
//! `crate::wake::RawIface` через `platform::netifs::raw_ifaces`.

use std::collections::HashMap;
use std::ffi::CStr;
use std::net::Ipv4Addr;

use anyhow::{bail, Result};

use crate::wake::{IfaceKind, RawIface};

/// Освобождает список `getifaddrs` при любом выходе.
struct IfAddrs(*mut libc::ifaddrs);

impl Drop for IfAddrs {
    fn drop(&mut self) {
        // Safety: указатель получен из успешного `getifaddrs` и освобождается
        // ровно один раз, здесь.
        unsafe { libc::freeifaddrs(self.0) };
    }
}

/// MAC и тип из канальной записи (AF_LINK на macOS).
#[cfg(target_vendor = "apple")]
unsafe fn link_info(sa: *const libc::sockaddr) -> Option<([u8; 6], IfaceKind)> {
    const IFT_ETHER: u8 = 6;
    if (*sa).sa_family as i32 != libc::AF_LINK {
        return None;
    }
    let sdl = sa as *const libc::sockaddr_dl;
    let nlen = (*sdl).sdl_nlen as usize;
    let alen = (*sdl).sdl_alen as usize;
    let kind = if (*sdl).sdl_type == IFT_ETHER {
        // Wi-Fi на macOS тоже IFT_ETHER -- различать не нужно.
        IfaceKind::Ethernet
    } else {
        IfaceKind::Other
    };
    let mut mac = [0u8; 6];
    if alen == 6 {
        // Имя и адрес лежат подряд в хвосте структуры, который длиннее
        // объявленного `sdl_data[12]`, поэтому идём от сырого указателя.
        let data = std::ptr::addr_of!((*sdl).sdl_data) as *const u8;
        for (i, b) in mac.iter_mut().enumerate() {
            *b = *data.add(nlen + i);
        }
    }
    Some((mac, kind))
}

/// MAC из канальной записи (AF_PACKET на Linux); тип -- из /sys.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn link_info(sa: *const libc::sockaddr) -> Option<([u8; 6], IfaceKind)> {
    if (*sa).sa_family as i32 != libc::AF_PACKET {
        return None;
    }
    let sll = sa as *const libc::sockaddr_ll;
    let mut mac = [0u8; 6];
    if (*sll).sll_halen == 6 {
        let addr = (*sll).sll_addr;
        mac.copy_from_slice(&addr[..6]);
    }
    // Тип определяется позже по имени (нужен /sys), здесь заглушка.
    Some((mac, IfaceKind::Ethernet))
}

/// Прочие unix: канальных адресов нет, MAC нулевой (интерфейс отсеется).
#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
unsafe fn link_info(_sa: *const libc::sockaddr) -> Option<([u8; 6], IfaceKind)> {
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn refine_kind(name: &str, kind: IfaceKind) -> IfaceKind {
    let base = std::path::Path::new("/sys/class/net").join(name);
    if base.join("wireless").exists() {
        IfaceKind::Wifi
    } else if !base.join("device").exists() {
        IfaceKind::Other
    } else {
        kind
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn refine_kind(_name: &str, kind: IfaceKind) -> IfaceKind {
    kind
}

struct V4 {
    name: String,
    ip: Ipv4Addr,
    prefix: u8,
    flags: u32,
}

/// Все IPv4-интерфейсы системы (включая loopback; фильтр -- в `wake`).
pub fn raw_ifaces() -> Result<Vec<RawIface>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // Safety: `head` -- валидный указатель на указатель; при успехе список
    // принадлежит нам и освобождается guard'ом.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        bail!("getifaddrs failed: {}", std::io::Error::last_os_error());
    }
    let guard = IfAddrs(head);

    let mut links: HashMap<String, ([u8; 6], IfaceKind)> = HashMap::new();
    let mut v4s: Vec<V4> = Vec::new();

    let mut cur = guard.0;
    while !cur.is_null() {
        // Safety: узлы списка валидны до `freeifaddrs` (guard живёт до конца
        // функции); `ifa_name` -- C-строка, адреса -- либо null, либо
        // sockaddr нужного семейства (проверяем `sa_family` до приведения).
        unsafe {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            if let Some(info) = link_info(ifa.ifa_addr) {
                links.insert(name, info);
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let ip_sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(ip_sa.sin_addr.s_addr));
            let prefix = if ifa.ifa_netmask.is_null() {
                0
            } else {
                let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                u32::from_be(m.sin_addr.s_addr).count_ones() as u8
            };
            v4s.push(V4 {
                name,
                ip,
                prefix,
                flags: ifa.ifa_flags as u32,
            });
        }
    }
    drop(guard);

    Ok(v4s
        .into_iter()
        .map(|v| {
            let (mac, kind) = links
                .get(&v.name)
                .copied()
                .unwrap_or(([0u8; 6], IfaceKind::Other));
            RawIface {
                kind: refine_kind(&v.name, kind),
                description: v.name.clone(),
                name: v.name,
                mac,
                ipv4: v.ip,
                prefix: v.prefix,
                is_up: v.flags & libc::IFF_UP as u32 != 0,
                is_loopback: v.flags & libc::IFF_LOOPBACK as u32 != 0,
                is_point_to_point: v.flags & libc::IFF_POINTOPOINT as u32 != 0,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ifaces_includes_loopback() {
        let raw = raw_ifaces().expect("enumerate");
        assert!(raw.iter().any(|r| r.is_loopback), "{raw:?}");
    }
}
