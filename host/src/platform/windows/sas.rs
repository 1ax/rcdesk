//! Ctrl+Alt+Del из сеанса (слайс D46). Комбинацию нельзя передать из
//! браузера, поэтому хост вызывает документированный `SendSAS(FALSE)` из
//! `sas.dll`. По документации вызывать его должна служба: живьём в 0.3.5
//! вызов из помощника (`serve --machine`, SYSTEM в консольном сеансе)
//! Windows игнорирует. Поэтому помощник лишь выставляет именованное событие
//! `Global\rcdesk-sas` ([`request`]), а поток службы ([`spawn_listener`])
//! ждёт его и вызывает `SendSAS`. Windows выполняет вызов только при политике
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
use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL, WAIT_OBJECT_0};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};

const POLICY_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
/// Имя события, которым помощник просит службу послать SAS.
const SAS_EVENT_NAME: &str = r"Global\rcdesk-sas";
/// Доступ к событию только у SYSTEM (помощник и служба -- оба SYSTEM).
const SAS_EVENT_SDDL: &str = "D:P(A;;GA;;;SY)";

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Посылает Secure Attention Sequence (Ctrl+Alt+Del) в консольный сеанс.
/// Вызывать только из потока службы ([`spawn_listener`]).
fn send() {
    // Safety: `SendSAS` не принимает указателей; `false` -- вызов от службы,
    // не от имени пользователя. Возвращаемого значения нет.
    unsafe { SendSAS(false) };
    tracing::info!("ctrl+alt+del sent (SendSAS)");
}

/// Создаёт событие `Global\rcdesk-sas` и запускает поток, который на каждое
/// его выставление вызывает `SendSAS`. Только для службы. Поток живёт до
/// конца процесса службы (join не нужен), хендл события не закрывается.
pub fn spawn_listener() -> anyhow::Result<std::thread::JoinHandle<()>> {
    let sddl = wide_null(SAS_EVENT_SDDL);
    let name = wide_null(SAS_EVENT_NAME);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // Safety: `sddl` -- валидная NUL-терминированная строка, переживает
    // вызов; `descriptor` -- валидный out-параметр, память выделяет система
    // (освобождается `LocalFree` ниже); размер не нужен.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;

    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // Safety: `attributes` и `name` живут до конца вызова; дескриптор в
    // `attributes` валиден до `LocalFree` ниже (система копирует его в объект).
    let created = unsafe {
        CreateEventW(
            Some(&attributes),
            false,
            false,
            PCWSTR::from_raw(name.as_ptr()),
        )
    };

    // Safety: `descriptor.0` выделен системой через `LocalAlloc` внутри
    // `Convert...` и освобождается ровно один раз, здесь, на всех путях;
    // `HLOCAL` и `PSECURITY_DESCRIPTOR` -- обёртки над одним указателем.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    let event = created.context("CreateEventW(Global\\rcdesk-sas)")?;

    // `HANDLE` не `Send`; передаём значение указателя числом. Событие в
    // потоке не закрывается: оно нужно до конца процесса.
    let raw = event.0 as usize;
    let handle = std::thread::Builder::new()
        .name("sas-listener".into())
        .spawn(move || {
            let event = HANDLE(raw as *mut std::ffi::c_void);
            loop {
                // Safety: `event` -- действительный хендл события, не
                // закрывается до конца процесса.
                let wait = unsafe { WaitForSingleObject(event, INFINITE) };
                if wait != WAIT_OBJECT_0 {
                    tracing::warn!(result = wait.0, "ожидание события rcdesk-sas прервано");
                    break;
                }
                send();
            }
        })
        .context("запуск потока sas-listener")?;
    Ok(handle)
}

/// Просит службу послать Ctrl+Alt+Del: выставляет событие `Global\rcdesk-sas`.
/// Вызывается из помощника; любая ошибка (например, служба старая и события
/// нет) -- только `warn!`.
pub fn request() {
    if let Err(err) = request_inner() {
        tracing::warn!(error = %format!("{err:#}"), "не удалось запросить ctrl+alt+del у службы");
    }
}

fn request_inner() -> anyhow::Result<()> {
    let name = wide_null(SAS_EVENT_NAME);
    // Safety: `name` -- NUL-терминированная строка, переживает вызов.
    let opened = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR::from_raw(name.as_ptr())) }
        .context("OpenEventW(Global\\rcdesk-sas)")?;
    // Safety: `opened` -- свежий хендл из успешного `OpenEventW`; `Owned`
    // закроет его ровно один раз.
    let event = unsafe { Owned::new(opened) };
    // Safety: `*event` -- действительный хендл с правом EVENT_MODIFY_STATE.
    unsafe { SetEvent(*event) }.context("SetEvent")?;
    tracing::info!("ctrl+alt+del requested from service");
    Ok(())
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
