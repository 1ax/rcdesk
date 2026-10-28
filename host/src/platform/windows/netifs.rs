//! Перечисление сетевых карт Windows для Wake-on-LAN (слайс W):
//! `GetAdaptersAddresses(AF_INET, ..)`.
//!
//! Компилируется только на `windows-latest` в CI: Windows-машины у исполнителя
//! нет, поэтому все сигнатуры и типы сверены с исходниками `windows` 0.61.3
//! в локальном реестре (как в `platform::windows::mouse`).

use std::net::Ipv4Addr;

use anyhow::{bail, Result};
use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

use crate::wake::{IfaceKind, RawIface};

const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_WIFI: u32 = 71;
const IF_TYPE_LOOPBACK: u32 = 24;
const IF_TYPE_PPP: u32 = 23;

/// Все IPv4-адаптеры системы (фильтр -- в `crate::wake::select_ifaces`).
pub fn raw_ifaces() -> Result<Vec<RawIface>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // Буфер из u64, чтобы выравнивание подошло под IP_ADAPTER_ADDRESSES_LH.
    let mut buf: Vec<u64> = Vec::new();
    let mut result = ERROR_BUFFER_OVERFLOW.0;
    for _ in 0..4 {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // Safety: `buf` не меньше `size` байт и выровнен на 8; ОС пишет в него
        // цепочку адаптеров и обновляет `size`.
        result = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if result != ERROR_BUFFER_OVERFLOW.0 {
            break;
        }
    }
    if result != 0 {
        bail!("GetAdaptersAddresses failed: {result}");
    }

    let mut out = Vec::new();
    let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cur.is_null() {
        // Safety: цепочка лежит внутри `buf` (жив до конца функции), её
        // заполнила ОС; `Next` -- null в конце.
        let adapter = unsafe { &*cur };
        cur = adapter.Next;

        if adapter.PhysicalAddressLength != 6 {
            continue;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&adapter.PhysicalAddress[..6]);

        // Первый unicast IPv4 с префиксом.
        let uni = adapter.FirstUnicastAddress;
        if uni.is_null() {
            continue;
        }
        // Safety: узел цепочки unicast-адресов внутри `buf`.
        let uni = unsafe { &*uni };
        let sockaddr = uni.Address.lpSockaddr;
        if sockaddr.is_null() {
            continue;
        }
        // Safety: запрошено AF_INET, значит `lpSockaddr` указывает на SOCKADDR_IN.
        let ip_raw = unsafe { (*(sockaddr as *const SOCKADDR_IN)).sin_addr.S_un.S_addr };
        // `S_addr` хранит байты адреса в сетевом порядке прямо в памяти.
        let ipv4 = Ipv4Addr::from(ip_raw.to_ne_bytes());

        // Safety: строки -- нуль-терминированные UTF-16 внутри `buf` (или null).
        let description = unsafe { wide_to_string(adapter.Description.0) };
        let name = unsafe { wide_to_string(adapter.FriendlyName.0) };

        out.push(RawIface {
            name,
            description,
            mac,
            ipv4,
            prefix: uni.OnLinkPrefixLength,
            is_up: adapter.OperStatus == IfOperStatusUp,
            is_loopback: adapter.IfType == IF_TYPE_LOOPBACK,
            is_point_to_point: adapter.IfType == IF_TYPE_PPP,
            kind: match adapter.IfType {
                IF_TYPE_ETHERNET => IfaceKind::Ethernet,
                IF_TYPE_WIFI => IfaceKind::Wifi,
                _ => IfaceKind::Other,
            },
        });
    }
    Ok(out)
}

/// Нуль-терминированная UTF-16 строка -> `String` (пустая для null).
///
/// # Safety
/// `ptr` -- null или указатель на нуль-терминированную UTF-16 строку.
unsafe fn wide_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ifaces_succeeds() {
        raw_ifaces().expect("enumerate adapters");
    }
}
