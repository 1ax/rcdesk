//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`-based "start at
//! login" (slice 2.6d). Formatting/comparing the `rcdesk` value lives in
//! `crate::agent::autostart` (`format_run_value`/`run_value_matches`) --
//! this file only turns that into registry IO, no admin rights needed since
//! `HKEY_CURRENT_USER` is always writable by the owning user.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see `platform::windows::power`'s doc comment,
//! which documents and uses the same approach. Signatures actually used
//! here (from `windows-0.61.3/src/Windows/Win32/System/Registry/mod.rs`):
//!
//! ```text
//! pub unsafe fn RegOpenKeyExW<P1>(hkey: HKEY, lpsubkey: P1, uloptions: Option<u32>, samdesired: REG_SAM_FLAGS, phkresult: *mut HKEY) -> WIN32_ERROR
//! pub unsafe fn RegSetValueExW<P1>(hkey: HKEY, lpvaluename: P1, reserved: Option<u32>, dwtype: REG_VALUE_TYPE, lpdata: Option<&[u8]>) -> WIN32_ERROR
//! pub unsafe fn RegGetValueW<P1, P2>(hkey: HKEY, lpsubkey: P1, lpvalue: P2, dwflags: REG_ROUTINE_FLAGS, pdwtype: Option<*mut REG_VALUE_TYPE>, pvdata: Option<*mut c_void>, pcbdata: Option<*mut u32>) -> WIN32_ERROR
//! pub unsafe fn RegDeleteKeyValueW<P1, P2>(hkey: HKEY, lpsubkey: P1, lpvaluename: P2) -> WIN32_ERROR
//! pub unsafe fn RegCloseKey(hkey: HKEY) -> WIN32_ERROR
//! ```
//!
//! D83c (`ensure_machine_autostart`/`remove_machine_autostart`) использует
//! те же сверенные сигнатуры с `HKEY_LOCAL_MACHINE` (константа `HKEY` из того
//! же модуля, что и `HKEY_CURRENT_USER`; `sas.rs` уже открывает по ней ключ).
//!
//! `RegCreateKeyExW` (the other function named in this slice's plan) is
//! `#[cfg(feature = "Win32_Security")]` in this crate version -- its
//! `lpsecurityattributes: Option<*const SECURITY_ATTRIBUTES>` parameter
//! needs that feature, which isn't otherwise enabled by this crate and
//! wasn't listed as an approved addition. `RegOpenKeyExW` needs no extra
//! feature and the `Run` key always exists on a real Windows install (it's
//! one of the handful of keys Windows itself creates), so `enable` opens it
//! rather than creates it -- flagged in this slice's report as a deviation
//! from the plan's exact function list, not from its intent.
//!
//! `P1`/`P2` above are generic over `windows_core::Param<PCWSTR>`, which
//! `PCWSTR` itself satisfies (`windows-core-0.61.2/src/windows.rs`'s
//! `impl TypeKind for PCWSTR { type TypeKind = CopyType; }` plus the blanket
//! `Param` impl for any `CopyType`) -- so a `PCWSTR::from_raw` built from a
//! `Vec<u16>` this function still owns is passed directly, no `HSTRING`
//! needed.
//!
//! Every `WIN32_ERROR` return is compared against `ERROR_FILE_NOT_FOUND`
//! (a missing key/value, not an error condition here) and otherwise checked
//! with its own `.ok()` (`WIN32_ERROR::ok(self) -> windows_core::Result<()>`,
//! from `windows-0.61.3/src/extensions/Win32/Foundation/WIN32_ERROR.rs`),
//! whose `Err` side (`windows_core::Error`) converts into `anyhow::Error`
//! via `anyhow::Context`.

use std::path::Path;

use anyhow::Context;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteKeyValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_SZ, RRF_RT_REG_SZ,
};

use crate::agent::autostart::{
    format_run_value, machine_run_value_needs_write, run_value_matches, MACHINE_RUN_VALUE_NAME,
    RUN_VALUE_NAME,
};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Null-terminated UTF-16, the form every `*W` registry function expects.
/// The caller must keep the returned `Vec` alive for as long as any
/// `PCWSTR::from_raw` built from its pointer is in use.
fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn is_enabled(agent: &Path) -> anyhow::Result<bool> {
    match read_run_value()? {
        Some(stored) => Ok(run_value_matches(&stored, agent)),
        None => Ok(false),
    }
}

pub fn enable(agent: &Path) -> anyhow::Result<()> {
    let subkey = wide_null(RUN_SUBKEY);
    let mut hkey = HKEY(std::ptr::null_mut());
    // Safety: `subkey` outlives the call; `&mut hkey` is a valid `*mut HKEY`
    // for the duration of the call.
    let open_result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            None,
            KEY_WRITE,
            &mut hkey,
        )
    };
    open_result
        .ok()
        .context("RegOpenKeyExW(...CurrentVersion\\Run) failed")?;

    let value = format_run_value(agent);
    let wide_value = wide_null(&value);
    // Safety: reinterpreting a `u16` buffer as bytes for `RegSetValueExW`'s
    // `&[u8]` parameter -- valid for reads, and the length below (`* 2`) is
    // computed from the same buffer so it never reads out of bounds.
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(wide_value.as_ptr().cast::<u8>(), wide_value.len() * 2)
    };
    let value_name = wide_null(RUN_VALUE_NAME);

    // Safety: `hkey` was just opened above and is closed below regardless of
    // the outcome; `value_name`/`bytes` outlive the call.
    let set_result = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(bytes),
        )
    };
    // Safety: `hkey` is a valid, still-open key handle from the successful
    // `RegOpenKeyExW` above.
    let close_result = unsafe { RegCloseKey(hkey) };

    set_result.ok().context("RegSetValueExW(rcdesk) failed")?;
    close_result.ok().context("RegCloseKey failed")?;
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    let subkey = wide_null(RUN_SUBKEY);
    let value_name = wide_null(RUN_VALUE_NAME);
    // Safety: `subkey`/`value_name` outlive the call.
    let result = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            PCWSTR::from_raw(value_name.as_ptr()),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    result.ok().context("RegDeleteKeyValueW(rcdesk) failed")?;
    Ok(())
}

/// D83c: машинный автозапуск просмотрщика PIN (`rcdesk-agent.exe`) для входа
/// любого пользователя -- `HKLM\...\Run`, значение `MACHINE_RUN_VALUE_NAME`
/// (не пересекается с пользовательским `rcdesk`). Пишет только если значения
/// нет или оно отличается. Вызывается службой (SYSTEM), правам хватает.
pub fn ensure_machine_autostart(agent: &Path) -> anyhow::Result<()> {
    let stored = read_run_value_in(HKEY_LOCAL_MACHINE, MACHINE_RUN_VALUE_NAME)?;
    if !machine_run_value_needs_write(stored.as_deref(), agent) {
        return Ok(());
    }

    let subkey = wide_null(RUN_SUBKEY);
    let mut hkey = HKEY(std::ptr::null_mut());
    // Safety: `subkey` переживает вызов; `&mut hkey` -- валидный `*mut HKEY`.
    let open_result = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(subkey.as_ptr()),
            None,
            KEY_WRITE,
            &mut hkey,
        )
    };
    open_result
        .ok()
        .context("RegOpenKeyExW(HKLM...CurrentVersion\\Run) failed")?;

    let wide_value = wide_null(&format_run_value(agent));
    // Safety: `u16`-буфер как байты для `&[u8]`; длина `* 2` посчитана по
    // тому же буферу, чтения за границу нет.
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(wide_value.as_ptr().cast::<u8>(), wide_value.len() * 2)
    };
    let value_name = wide_null(MACHINE_RUN_VALUE_NAME);
    // Safety: `hkey` открыт выше с KEY_WRITE и закрывается ниже в любом
    // случае; `value_name`/`bytes` переживают вызов.
    let set_result = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(bytes),
        )
    };
    // Safety: `hkey` -- действительный открытый хендл из `RegOpenKeyExW`.
    let close_result = unsafe { RegCloseKey(hkey) };

    set_result
        .ok()
        .context("RegSetValueExW(HKLM rcdesk-viewer) failed")?;
    close_result.ok().context("RegCloseKey failed")?;
    tracing::info!(agent = %agent.display(), "machine autostart for the PIN viewer set");
    Ok(())
}

/// Удаляет машинный автозапуск просмотрщика; отсутствие значения -- не ошибка.
pub fn remove_machine_autostart() -> anyhow::Result<()> {
    let subkey = wide_null(RUN_SUBKEY);
    let value_name = wide_null(MACHINE_RUN_VALUE_NAME);
    // Safety: `subkey`/`value_name` переживают вызов.
    let result = unsafe {
        RegDeleteKeyValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(subkey.as_ptr()),
            PCWSTR::from_raw(value_name.as_ptr()),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    result
        .ok()
        .context("RegDeleteKeyValueW(HKLM rcdesk-viewer) failed")?;
    Ok(())
}

/// `None` when the `rcdesk` value doesn't exist (or the `Run` key itself
/// doesn't, which `RegGetValueW` reports the same way) -- not an error, just
/// "not enabled".
fn read_run_value() -> anyhow::Result<Option<String>> {
    read_run_value_in(HKEY_CURRENT_USER, RUN_VALUE_NAME)
}

/// Как `read_run_value`, но для произвольного куста (`HKCU`/`HKLM`) и имени.
fn read_run_value_in(root: HKEY, name: &str) -> anyhow::Result<Option<String>> {
    let subkey = wide_null(RUN_SUBKEY);
    let value_name = wide_null(name);
    let subkey_pcwstr = PCWSTR::from_raw(subkey.as_ptr());
    let value_pcwstr = PCWSTR::from_raw(value_name.as_ptr());

    let mut byte_len: u32 = 0;
    // Safety: `subkey`/`value_name` outlive the call; a `None` `pvdata` with
    // `Some(&mut byte_len)` is `RegGetValueW`'s documented way to query the
    // required buffer size without reading the value itself. `HKEY_CURRENT_USER`
    // needs no prior `RegOpenKeyExW`/`KEY_READ` -- `RegGetValueW` takes the
    // subkey path directly.
    let size_result = unsafe {
        RegGetValueW(
            root,
            subkey_pcwstr,
            value_pcwstr,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut byte_len),
        )
    };
    if size_result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    size_result
        .ok()
        .context("RegGetValueW (size query) failed")?;
    if byte_len == 0 {
        return Ok(Some(String::new()));
    }

    let mut buf = vec![0u8; byte_len as usize];
    let mut actual_len = byte_len;
    // Safety: `buf` has exactly `byte_len` bytes, matching `pcbdata`'s
    // in/out size contract; `subkey`/`value_name` still outlive this call.
    let read_result = unsafe {
        RegGetValueW(
            root,
            subkey_pcwstr,
            value_pcwstr,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut actual_len),
        )
    };
    if read_result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    read_result.ok().context("RegGetValueW failed")?;

    let len = (actual_len as usize).min(buf.len());
    let words: Vec<u16> = buf[..len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_ne_bytes(*pair))
        .collect();
    let s = String::from_utf16_lossy(&words);
    Ok(Some(s.trim_end_matches('\0').to_string()))
}

#[cfg(test)]
mod tests {
    // No unit tests here: every function in this file touches the real
    // Windows registry and this executor run has no Windows machine to
    // verify behaviour on. The pure logic it depends on
    // (`format_run_value`/`run_value_matches`) is tested in
    // `crate::agent::autostart` instead.
}
