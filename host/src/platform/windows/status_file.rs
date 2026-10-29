//! Файл статуса помощника `status.json` в полном режиме Windows (D83a).
//! Каталог данных закрыт для обычных пользователей (SYSTEM + Administrators),
//! а PIN должен видеть пользователь активной консоли (`service status` из
//! обычной cmd, значок трея). Поэтому файл пишется атомарно: временный файл
//! рядом, на него ставится защищённый DACL (SYSTEM и Administrators --
//! полный доступ, пользователь активной консоли -- чтение, SDDL собирает
//! `crate::service::status_file_sddl`), затем `std::fs::rename` (на Windows
//! это `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, DACL перемещаемого файла
//! сохраняется). Временный файл создаётся пустым внутри каталога с DACL
//! SYSTEM + Administrators, поэтому до установки DACL содержимого в нём нет.
//!
//! Компилируется только на `windows-latest` в CI; сигнатуры сверены с
//! `windows` 0.61.3 (`~/.cargo/registry/src/*/windows-0.61.3/src/Windows/
//! Win32/...`). Новых фич крейта не требуется (все уже включены для
//! `platform::windows::service` и `files::windows`):
//!
//! ```text
//! // System/RemoteDesktop (Win32_System_RemoteDesktop)
//! pub unsafe fn WTSGetActiveConsoleSessionId() -> u32
//! pub unsafe fn WTSQueryUserToken(sessionid: u32, phtoken: *mut HANDLE) -> Result<()>
//! // Security (Win32_Security)
//! pub unsafe fn GetTokenInformation(tokenhandle: HANDLE, tokeninformationclass: TOKEN_INFORMATION_CLASS, tokeninformation: Option<*mut c_void>, tokeninformationlength: u32, returnlength: *mut u32) -> Result<()>
//! pub struct TOKEN_USER { pub User: SID_AND_ATTRIBUTES }   // SID_AND_ATTRIBUTES { pub Sid: PSID, pub Attributes: u32 }
//! pub const TokenUser: TOKEN_INFORMATION_CLASS
//! // Security/Authorization (Win32_Security_Authorization)
//! pub unsafe fn ConvertSidToStringSidW(sid: PSID, stringsid: *mut PWSTR) -> Result<()>
//! pub unsafe fn ConvertStringSecurityDescriptorToSecurityDescriptorW<P0>(stringsecuritydescriptor: P0, stringsdrevision: u32, securitydescriptor: *mut PSECURITY_DESCRIPTOR, securitydescriptorsize: Option<*mut u32>) -> Result<()>
//! pub unsafe fn SetNamedSecurityInfoW<P0>(pobjectname: P0, objecttype: SE_OBJECT_TYPE, securityinfo: OBJECT_SECURITY_INFORMATION, psidowner: Option<PSID>, psidgroup: Option<PSID>, pdacl: Option<*const ACL>, psacl: Option<*const ACL>) -> WIN32_ERROR
//! // Security
//! pub unsafe fn GetSecurityDescriptorDacl(psecuritydescriptor: PSECURITY_DESCRIPTOR, lpbdaclpresent: *mut BOOL, pdacl: *mut *mut ACL, lpbdacldefaulted: *mut BOOL) -> Result<()>
//! // Foundation
//! pub unsafe fn LocalFree(hmem: Option<HLOCAL>) -> HLOCAL
//! ```

use std::io::Write as _;
use std::path::Path;

use anyhow::Context;
use windows::core::{Owned, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_USER,
};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};

use crate::service::{active_session_from_raw, status_file_sddl};

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Строковый SID пользователя активной консольной сессии. `None` -- сессии
/// нет или в ней никто не вошёл (экран входа): это не ошибка. Нужны права
/// SYSTEM (`WTSQueryUserToken` требует `SeTcbPrivilege`).
pub fn console_user_sid() -> Option<String> {
    // Safety: вызов без аргументов, указателей не принимает.
    let raw = unsafe { WTSGetActiveConsoleSessionId() };
    let session = active_session_from_raw(raw)?;
    let mut token = HANDLE::default();
    // Safety: `token` -- валидный out-параметр.
    unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
    // Safety: при успехе `token` -- новый хэндл, которым владеем мы; `Owned`
    // закроет его ровно один раз.
    let token = unsafe { Owned::new(token) };
    match token_user_sid(&token) {
        Ok(sid) => Some(sid),
        Err(err) => {
            tracing::warn!(error = %err, "не удалось получить SID пользователя консоли");
            None
        }
    }
}

fn token_user_sid(token: &Owned<HANDLE>) -> anyhow::Result<String> {
    let mut needed = 0u32;
    // Safety: запрос размера -- буфер `None`, длина 0; `needed` валиден.
    // Ожидаемая ошибка ERROR_INSUFFICIENT_BUFFER игнорируется, смотрим `needed`.
    let _ = unsafe { GetTokenInformation(**token, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        anyhow::bail!("GetTokenInformation(TokenUser) не вернул размер");
    }
    // `u64` -- чтобы буфер был выровнен под TOKEN_USER (указатели).
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // Safety: `buf` не меньше `needed` байт и выровнен; `needed` валиден.
    unsafe {
        GetTokenInformation(
            **token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .context("GetTokenInformation(TokenUser)")?;
    // Safety: система записала в начало `buf` структуру TOKEN_USER; SID, на
    // который она ссылается, лежит в том же буфере, жив до конца функции.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut out = PWSTR::null();
    // Safety: `sid` валиден (см. выше); `out` -- валидный out-параметр, строку
    // выделяет система (`LocalAlloc`), освобождается `LocalFree` ниже.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }.context("ConvertSidToStringSidW")?;
    // Safety: `out` -- NUL-терминированная UTF-16 строка из вызова выше.
    let text = unsafe { out.to_string() };
    // Safety: `out.0` выделен системой через `LocalAlloc` и освобождается
    // ровно один раз, здесь; `text` уже владеет копией.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.0.cast())));
    }
    text.context("SID не UTF-16")
}

/// Атомарно пишет `bytes` в `path` с защищённым DACL (SYSTEM + Administrators
/// -- полный доступ, пользователь активной консоли -- чтение).
pub fn write_status_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("создание {}", parent.display()))?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);

    // Пустой временный файл; DACL ставится до записи содержимого.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .with_context(|| format!("создание {}", tmp.display()))?;
    let result = (|| -> anyhow::Result<()> {
        apply_status_dacl(&tmp, console_user_sid().as_deref())?;
        file.write_all(bytes)
            .with_context(|| format!("запись {}", tmp.display()))?;
        file.sync_all().ok();
        drop(file);
        std::fs::rename(&tmp, path).with_context(|| format!("замена {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Ставит защищённый DACL по SDDL из `status_file_sddl` на файл.
fn apply_status_dacl(file: &Path, sid: Option<&str>) -> anyhow::Result<()> {
    let sddl = wide_null(&status_file_sddl(sid));
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

    let result = set_dacl(file, descriptor);

    // Safety: `descriptor.0` выделен системой через `LocalAlloc` и
    // освобождается ровно один раз, здесь, на всех путях.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

fn set_dacl(file: &Path, descriptor: PSECURITY_DESCRIPTOR) -> anyhow::Result<()> {
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // Safety: `descriptor` -- валидный дескриптор из `Convert...`; указатель
    // на ACL остаётся внутри его памяти (жива до `LocalFree` в вызывающем).
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .context("GetSecurityDescriptorDacl")?;
    if !present.as_bool() || dacl.is_null() {
        anyhow::bail!("в дескрипторе статуса нет DACL");
    }
    let path = wide_path(file);
    // Safety: `path` переживает вызов; `dacl` -- валидный ACL (см. выше);
    // владелец/группа/SACL не меняются (None).
    let error = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl.cast_const()),
            None,
        )
    };
    error
        .ok()
        .with_context(|| format!("SetNamedSecurityInfoW({})", file.display()))
}
