//! Ctrl+Alt+Del из сеанса (слайс D46). Комбинацию нельзя передать из
//! браузера, поэтому хост вызывает документированный `SendSAS(FALSE)` из
//! `sas.dll`; Windows выполняет его только при политике
//! `SoftwareSASGeneration` (HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\
//! Policies\System, REG_DWORD, 1 = разрешено службам), которую при старте
//! службы выставляет `ensure_policy`.
//!
//! Компилируется только на `windows-latest` в CI; сигнатуры сверены с
//! `windows` 0.61.3 (`Win32/Security/Authentication/Identity/mod.rs`,
//! `Win32/System/Registry/mod.rs`):
//!
//! ```text
//! pub unsafe fn SendSAS(asuser: bool)
//! pub unsafe fn RegCreateKeyExW<P1, P3>(hkey: HKEY, lpsubkey: P1, reserved: Option<u32>, lpclass: P3, dwoptions: REG_OPEN_CREATE_OPTIONS, samdesired: REG_SAM_FLAGS, lpsecurityattributes: Option<*const SECURITY_ATTRIBUTES>, phkresult: *mut HKEY, lpdwdisposition: Option<*mut REG_CREATE_KEY_DISPOSITION>) -> WIN32_ERROR
//! pub unsafe fn RegQueryValueExW<P1>(hkey: HKEY, lpvaluename: P1, lpreserved: Option<*const u32>, lptype: Option<*mut REG_VALUE_TYPE>, lpdata: Option<*mut u8>, lpcbdata: Option<*mut u32>) -> WIN32_ERROR
//! pub unsafe fn RegSetValueExW<P1>(hkey: HKEY, lpvaluename: P1, reserved: Option<u32>, dwtype: REG_VALUE_TYPE, lpdata: Option<&[u8]>) -> WIN32_ERROR
//! ```

use anyhow::Context;
use windows::core::PCWSTR;
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_VALUE_TYPE,
};

const POLICY_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Посылает Secure Attention Sequence (Ctrl+Alt+Del) в консольный сеанс.
pub fn send() {
    // Safety: `SendSAS` не принимает указателей; `false` -- вызов от службы,
    // не от имени пользователя. Возвращаемого значения нет.
    unsafe { SendSAS(false) };
    tracing::info!("ctrl+alt+del sent (SendSAS)");
}

/// Включает `SoftwareSASGeneration = 1`, если значения нет или оно 0;
/// ненулевое значение не трогает.
pub fn ensure_policy() -> anyhow::Result<()> {
    let subkey = wide_null(POLICY_SUBKEY);
    let value_name = wide_null(POLICY_VALUE);
    let mut hkey = HKEY(std::ptr::null_mut());
    // Safety: `subkey` переживает вызов; `&mut hkey` -- валидный `*mut HKEY`;
    // остальные необязательные параметры `None`.
    let open_result = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    open_result
        .ok()
        .context("RegCreateKeyExW(...Policies\\System) failed")?;

    let result = ensure_policy_in_key(hkey, &value_name);

    // Safety: `hkey` -- открытый ключ из успешного `RegCreateKeyExW` выше.
    let close_result = unsafe { RegCloseKey(hkey) };
    result?;
    close_result.ok().context("RegCloseKey failed")?;
    Ok(())
}

fn ensure_policy_in_key(hkey: HKEY, value_name: &[u16]) -> anyhow::Result<()> {
    let mut value_type = REG_VALUE_TYPE(0);
    let mut data = [0u8; 4];
    let mut data_len: u32 = 4;
    // Safety: `value_name` нуль-терминирован и переживает вызов; `data` --
    // буфер ровно `data_len` байт, `value_type`/`data_len` -- валидные
    // выходные указатели.
    let query_result = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(data.as_mut_ptr()),
            Some(&mut data_len),
        )
    };
    // Нет значения (`ERROR_FILE_NOT_FOUND`), другой тип/размер или 0 --
    // выставляем 1; читаемый ненулевой DWORD не трогаем.
    if query_result.is_ok()
        && value_type == REG_DWORD
        && data_len == 4
        && u32::from_le_bytes(data) != 0
    {
        return Ok(());
    }

    let one = 1u32.to_le_bytes();
    // Safety: `hkey` открыт с KEY_SET_VALUE; `value_name` и `one` переживают
    // вызов.
    let set_result = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            REG_DWORD,
            Some(&one),
        )
    };
    set_result
        .ok()
        .context("RegSetValueExW(SoftwareSASGeneration) failed")?;
    tracing::info!("SoftwareSASGeneration set to 1");
    Ok(())
}
