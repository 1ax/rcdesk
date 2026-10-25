//! Полный режим Windows (слайс 4.1a), `ARCHITECTURE.md` §4.1/§8: служба
//! `rcdesk` (LocalSystem) внутри того же `rcdesk-host.exe` и помощник
//! `rcdesk-host serve --machine` в активной консольной сессии. Чистая логика
//! (командные строки, автомат супервизора, `pin.txt`, перенос файлов) --
//! `crate::service`; здесь только Win32: SCM, токены, процесс помощника,
//! Job object, DACL машинного каталога.
//!
//! Компилируется только на `windows-latest` в CI: Windows-машины у
//! исполнителя нет, поэтому каждая сигнатура ниже взята из исходников
//! `windows` 0.61.3 в локальном кэше реестра
//! (`~/.cargo/registry/src/*/windows-0.61.3/src/Windows/Win32/...`), не из
//! памяти. Использованные сигнатуры (`P*` -- `windows_core::Param<PCWSTR>`,
//! им удовлетворяет сам `PCWSTR`):
//!
//! ```text
//! // System/Services (feature Win32_System_Services)
//! pub unsafe fn OpenSCManagerW<P0, P1>(lpmachinename: P0, lpdatabasename: P1, dwdesiredaccess: u32) -> Result<SC_HANDLE>
//! pub unsafe fn CreateServiceW<P1, P2, P7, P8, P10, P11, P12>(hscmanager: SC_HANDLE, lpservicename: P1, lpdisplayname: P2, dwdesiredaccess: u32, dwservicetype: ENUM_SERVICE_TYPE, dwstarttype: SERVICE_START_TYPE, dwerrorcontrol: SERVICE_ERROR, lpbinarypathname: P7, lploadordergroup: P8, lpdwtagid: Option<*mut u32>, lpdependencies: P10, lpservicestartname: P11, lppassword: P12) -> Result<SC_HANDLE>
//! pub unsafe fn OpenServiceW<P1>(hscmanager: SC_HANDLE, lpservicename: P1, dwdesiredaccess: u32) -> Result<SC_HANDLE>
//! pub unsafe fn ChangeServiceConfig2W(hservice: SC_HANDLE, dwinfolevel: SERVICE_CONFIG, lpinfo: Option<*const c_void>) -> Result<()>
//! pub unsafe fn StartServiceW(hservice: SC_HANDLE, lpserviceargvectors: Option<&[PCWSTR]>) -> Result<()>
//! pub unsafe fn ControlService(hservice: SC_HANDLE, dwcontrol: u32, lpservicestatus: *mut SERVICE_STATUS) -> Result<()>
//! pub unsafe fn QueryServiceStatus(hservice: SC_HANDLE, lpservicestatus: *mut SERVICE_STATUS) -> Result<()>
//! pub unsafe fn DeleteService(hservice: SC_HANDLE) -> Result<()>
//! pub unsafe fn StartServiceCtrlDispatcherW(lpservicestarttable: *const SERVICE_TABLE_ENTRYW) -> Result<()>
//! pub unsafe fn RegisterServiceCtrlHandlerExW<P0>(lpservicename: P0, lphandlerproc: LPHANDLER_FUNCTION_EX, lpcontext: Option<*const c_void>) -> Result<SERVICE_STATUS_HANDLE>
//! pub unsafe fn SetServiceStatus(hservicestatus: SERVICE_STATUS_HANDLE, lpservicestatus: *const SERVICE_STATUS) -> Result<()>
//! pub type LPHANDLER_FUNCTION_EX = Option<unsafe extern "system" fn(dwcontrol: u32, dweventtype: u32, lpeventdata: *mut c_void, lpcontext: *mut c_void) -> u32>
//! pub type LPSERVICE_MAIN_FUNCTIONW = Option<unsafe extern "system" fn(dwnumservicesargs: u32, lpserviceargvectors: *mut PWSTR)>
//! pub struct SERVICE_TABLE_ENTRYW { pub lpServiceName: PWSTR, pub lpServiceProc: LPSERVICE_MAIN_FUNCTIONW }
//! pub struct SERVICE_STATUS { dwServiceType: ENUM_SERVICE_TYPE, dwCurrentState: SERVICE_STATUS_CURRENT_STATE, dwControlsAccepted: u32, dwWin32ExitCode: u32, dwServiceSpecificExitCode: u32, dwCheckPoint: u32, dwWaitHint: u32 }
//! pub struct SERVICE_DESCRIPTIONW { pub lpDescription: PWSTR }
//! pub struct SERVICE_FAILURE_ACTIONSW { dwResetPeriod: u32, lpRebootMsg: PWSTR, lpCommand: PWSTR, cActions: u32, lpsaActions: *mut SC_ACTION }
//! pub struct SC_ACTION { pub Type: SC_ACTION_TYPE, pub Delay: u32 }
//! // System/RemoteDesktop (feature Win32_System_RemoteDesktop)
//! pub unsafe fn WTSGetActiveConsoleSessionId() -> u32
//! // System/JobObjects (feature Win32_System_JobObjects; CreateJobObjectW
//! // additionally needs Win32_Security, the extended-limit struct needs
//! // Win32_System_Threading -- both already enabled)
//! pub unsafe fn CreateJobObjectW<P1>(lpjobattributes: Option<*const SECURITY_ATTRIBUTES>, lpname: P1) -> Result<HANDLE>
//! pub unsafe fn SetInformationJobObject(hjob: HANDLE, jobobjectinformationclass: JOBOBJECTINFOCLASS, lpjobobjectinformation: *const c_void, cbjobobjectinformationlength: u32) -> Result<()>
//! pub unsafe fn AssignProcessToJobObject(hjob: HANDLE, hprocess: HANDLE) -> Result<()>
//! // System/Threading (feature Win32_System_Threading; the *Token/*AsUser
//! // functions also need Win32_Security)
//! pub unsafe fn GetCurrentProcess() -> HANDLE
//! pub unsafe fn OpenProcessToken(processhandle: HANDLE, desiredaccess: TOKEN_ACCESS_MASK, tokenhandle: *mut HANDLE) -> Result<()>
//! pub unsafe fn CreateProcessAsUserW<P1, P8>(htoken: Option<HANDLE>, lpapplicationname: P1, lpcommandline: Option<PWSTR>, lpprocessattributes: Option<*const SECURITY_ATTRIBUTES>, lpthreadattributes: Option<*const SECURITY_ATTRIBUTES>, binherithandles: bool, dwcreationflags: PROCESS_CREATION_FLAGS, lpenvironment: Option<*const c_void>, lpcurrentdirectory: P8, lpstartupinfo: *const STARTUPINFOW, lpprocessinformation: *mut PROCESS_INFORMATION) -> Result<()>
//! pub unsafe fn TerminateProcess(hprocess: HANDLE, uexitcode: u32) -> Result<()>
//! pub unsafe fn WaitForSingleObject(hhandle: HANDLE, dwmilliseconds: u32) -> WAIT_EVENT
//! pub unsafe fn GetExitCodeProcess(hprocess: HANDLE, lpexitcode: *mut u32) -> Result<()>
//! // Security (feature Win32_Security)
//! pub unsafe fn DuplicateTokenEx(hexistingtoken: HANDLE, dwdesiredaccess: TOKEN_ACCESS_MASK, lptokenattributes: Option<*const SECURITY_ATTRIBUTES>, impersonationlevel: SECURITY_IMPERSONATION_LEVEL, tokentype: TOKEN_TYPE, phnewtoken: *mut HANDLE) -> Result<()>
//! pub unsafe fn SetTokenInformation(tokenhandle: HANDLE, tokeninformationclass: TOKEN_INFORMATION_CLASS, tokeninformation: *const c_void, tokeninformationlength: u32) -> Result<()>
//! pub unsafe fn GetSecurityDescriptorDacl(psecuritydescriptor: PSECURITY_DESCRIPTOR, lpbdaclpresent: *mut BOOL, pdacl: *mut *mut ACL, lpbdacldefaulted: *mut BOOL) -> Result<()>
//! pub unsafe fn CreateWellKnownSid(wellknownsidtype: WELL_KNOWN_SID_TYPE, domainsid: Option<PSID>, psid: Option<PSID>, cbsid: *mut u32) -> Result<()>
//! pub unsafe fn EqualSid(psid1: PSID, psid2: PSID) -> Result<()>   // Ok = SID равны
//! pub const SECURITY_MAX_SID_SIZE: u32 = 68
//! pub const OWNER_SECURITY_INFORMATION: OBJECT_SECURITY_INFORMATION
//! pub const WinLocalSystemSid / WinBuiltinAdministratorsSid: WELL_KNOWN_SID_TYPE
//! pub struct PSID(pub *mut c_void); pub struct PSECURITY_DESCRIPTOR(pub *mut c_void)
//! // Security/Authorization (feature Win32_Security_Authorization)
//! pub unsafe fn ConvertStringSecurityDescriptorToSecurityDescriptorW<P0>(stringsecuritydescriptor: P0, stringsdrevision: u32, securitydescriptor: *mut PSECURITY_DESCRIPTOR, securitydescriptorsize: Option<*mut u32>) -> Result<()>
//! pub unsafe fn GetNamedSecurityInfoW<P0>(pobjectname: P0, objecttype: SE_OBJECT_TYPE, securityinfo: OBJECT_SECURITY_INFORMATION, ppsidowner: Option<*mut PSID>, ppsidgroup: Option<*mut PSID>, ppdacl: Option<*mut *mut ACL>, ppsacl: Option<*mut *mut ACL>, ppsecuritydescriptor: *mut PSECURITY_DESCRIPTOR) -> WIN32_ERROR
//! pub unsafe fn SetNamedSecurityInfoW<P0>(pobjectname: P0, objecttype: SE_OBJECT_TYPE, securityinfo: OBJECT_SECURITY_INFORMATION, psidowner: Option<PSID>, psidgroup: Option<PSID>, pdacl: Option<*const ACL>, psacl: Option<*const ACL>) -> WIN32_ERROR
//! // Foundation (no feature)
//! pub unsafe fn LocalFree(hmem: Option<HLOCAL>) -> HLOCAL
//! ```
//!
//! Проверка владельца `%ProgramData%\rcdesk` перед установкой:
//! `create_dir_all` на существующем каталоге -- no-op, а `%ProgramData%` по
//! умолчанию даёт Users право создавать подкаталоги, так что не-админ мог
//! создать каталог заранее и остаться его владельцем (неявный WRITE_DAC).
//!
//! `Result<T>` -- `windows_core::Result<T>`, `Err` несёт `GetLastError`;
//! `SC_HANDLE` и `HANDLE` реализуют `windows_core::Free` (закрытие), поэтому
//! живут в `Owned<_>`.
//!
//! Схема запуска помощника: токен самого процесса службы (SYSTEM)
//! дублируется как primary, ему меняется `TokenSessionId` на активную
//! консольную сессию (нужна `SeTcbPrivilege`, у LocalSystem она есть), затем
//! `CreateProcessAsUserW` с рабочим столом `winsta0\default`. Процесс
//! помещается в Job object с `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: умерла
//! служба -- ядро закрыло хендл job -- умер помощник.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::core::{Owned, BOOL, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    LocalFree, ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE, HANDLE, HLOCAL, NO_ERROR,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    CreateWellKnownSid, DuplicateTokenEx, EqualSid, GetSecurityDescriptorDacl,
    SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_MAX_SID_SIZE, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, WELL_KNOWN_SID_TYPE,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW, SetServiceStatus,
    StartServiceCtrlDispatcherW, StartServiceW, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::agent::logging;
use crate::agent::paths::{data_dir, machine_data_dir, machine_log_dir};
use crate::hub::{self, HubConfig};
use crate::service::{
    active_session_from_raw, helper_command_line, helper_server, migrate_files,
    service_command_line, validate_server, Action, HelperState, ServiceState, Supervisor,
    MIGRATED_FILES, SERVICE_LOG, SERVICE_NAME,
};

/// Имя в services.msc.
const DISPLAY_NAME: &str = "rcdesk remote desktop host";
const DESCRIPTION: &str =
    "rcdesk: удалённый доступ к рабочему столу (полный режим: экран входа, окна администратора).";
/// DACL машинного каталога: защищённый, только SYSTEM и Administrators,
/// с наследованием на файлы и подкаталоги.
const DATA_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// Рабочий стол помощника.
const HELPER_DESKTOP: &str = "winsta0\\default";
/// Сколько ждать остановки службы.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// Период опроса супервизора.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Null-terminated UTF-16 для `*W`-функций. Владелец обязан держать `Vec`
/// живым, пока жив любой `PCWSTR`/`PWSTR`, построенный из его указателя.
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

fn code_is(err: &windows::core::Error, win32: u32) -> bool {
    err.code() == HRESULT::from_win32(win32)
}

/// Ошибка Win32 с контекстом; на «доступ запрещён» -- подсказка про
/// администратора.
fn win_err(what: &str, err: windows::core::Error) -> anyhow::Error {
    if code_is(&err, ERROR_ACCESS_DENIED.0) {
        anyhow::anyhow!("{what}: {err} -- запустите cmd от имени администратора")
    } else {
        anyhow::anyhow!("{what}: {err}")
    }
}

fn open_scm(access: u32) -> anyhow::Result<Owned<SC_HANDLE>> {
    // Safety: оба имени -- null (локальная машина, база ServicesActive).
    let handle = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map_err(|e| win_err("OpenSCManagerW", e))?;
    // Safety: хендл только что вернул успешный `OpenSCManagerW`, владелец -- мы.
    Ok(unsafe { Owned::new(handle) })
}

/// `Ok(None)` -- службы нет.
fn open_service(scm: &Owned<SC_HANDLE>, access: u32) -> anyhow::Result<Option<Owned<SC_HANDLE>>> {
    let name = wide_null(SERVICE_NAME);
    // Safety: `scm` -- живой хендл SCM, `name` переживает вызов.
    match unsafe { OpenServiceW(**scm, PCWSTR(name.as_ptr()), access) } {
        // Safety: хендл только что вернул успешный `OpenServiceW`.
        Ok(handle) => Ok(Some(unsafe { Owned::new(handle) })),
        Err(e) if code_is(&e, ERROR_SERVICE_DOES_NOT_EXIST.0) => Ok(None),
        Err(e) => Err(win_err("OpenServiceW", e)),
    }
}

fn query_state(service: &Owned<SC_HANDLE>) -> anyhow::Result<ServiceState> {
    let mut status = SERVICE_STATUS::default();
    // Safety: `service` -- живой хендл с `SERVICE_QUERY_STATUS`, `status` --
    // валидный out-параметр.
    unsafe { QueryServiceStatus(**service, &mut status) }
        .map_err(|e| win_err("QueryServiceStatus", e))?;
    ServiceState::from_raw(status.dwCurrentState.0)
        .with_context(|| format!("неизвестное состояние службы {}", status.dwCurrentState.0))
}

/// Состояние службы. Работает без прав администратора (`SC_MANAGER_CONNECT`
/// + `SERVICE_QUERY_STATUS`).
pub fn status() -> anyhow::Result<ServiceState> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    match open_service(&scm, SERVICE_QUERY_STATUS)? {
        Some(service) => query_state(&service),
        None => Ok(ServiceState::NotInstalled),
    }
}

/// Служба работает (или запускается). Любая ошибка опроса -- `false`.
pub fn is_running() -> bool {
    matches!(
        status(),
        Ok(ServiceState::Running | ServiceState::StartPending)
    )
}

/// Служба зарегистрирована в SCM. Ошибка опроса SCM -- `false`.
pub fn is_installed() -> bool {
    matches!(status(), Ok(state) if state != ServiceState::NotInstalled)
}

/// SID известной группы (`WinLocalSystemSid` и т.п.) в буфере `buf`.
fn well_known_sid(
    kind: WELL_KNOWN_SID_TYPE,
    buf: &mut [u8; SECURITY_MAX_SID_SIZE as usize],
) -> anyhow::Result<PSID> {
    let mut size = SECURITY_MAX_SID_SIZE;
    let sid = PSID(buf.as_mut_ptr().cast());
    // Safety: `buf` -- SECURITY_MAX_SID_SIZE байт, `size` сообщает его
    // размер; `domainsid` не нужен (None); буфер переживает возвращаемый PSID
    // (владелец -- вызывающий).
    unsafe { CreateWellKnownSid(kind, None, Some(sid), &mut size) }
        .context("CreateWellKnownSid")?;
    Ok(sid)
}

/// Отказывает, если каталог уже существует и принадлежит не SYSTEM и не
/// Administrators (`create_dir_all` на существующем каталоге -- no-op, а
/// владелец с неявным WRITE_DAC мог бы переписать DACL). Нет каталога --
/// `Ok`: создадим сами под токеном администратора.
fn check_machine_dir_owner(dir: &Path) -> anyhow::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("проверка {}", dir.display())),
    };
    // На Windows std считает ссылкой и symlink, и junction.
    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "каталог {} является ссылкой (symlink/junction) -- удалите его и повторите установку",
            dir.display()
        );
    }
    if !meta.is_dir() {
        anyhow::bail!(
            "{} не каталог -- удалите его и повторите установку",
            dir.display()
        );
    }

    let path = wide_path(dir);
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // Safety: `path` -- валидная NUL-терминированная строка, переживает
    // вызов; `owner` и `descriptor` -- валидные out-параметры; остальные
    // части дескриптора не запрашиваются (None). Память под дескриптор
    // выделяет система, `owner` указывает внутрь неё; освобождается
    // `LocalFree` ниже.
    let error = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    if let Err(e) = error.ok() {
        return Err(win_err(
            &format!("GetNamedSecurityInfoW({})", dir.display()),
            e,
        ));
    }

    let trusted = owner_is_trusted(owner);

    // Safety: `descriptor.0` выделен системой (`LocalAlloc`) внутри
    // `GetNamedSecurityInfoW` и освобождается ровно один раз, здесь, на всех
    // путях после успеха; `owner` после этого не используется.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }

    if !trusted? {
        anyhow::bail!(
            "каталог {} уже существует и создан не администратором -- удалите его (от имени администратора) и повторите установку",
            dir.display()
        );
    }
    Ok(())
}

/// `owner` равен SYSTEM или Administrators. `owner` должен быть валидным.
fn owner_is_trusted(owner: PSID) -> anyhow::Result<bool> {
    let mut system_buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admins_buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let system = well_known_sid(WinLocalSystemSid, &mut system_buf)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid, &mut admins_buf)?;
    // Safety: все три SID валидны (`owner` -- от вызывающего, остальные --
    // в локальных буферах, живых до конца функции); `Err` значит «не равны».
    let equal = |a: PSID, b: PSID| unsafe { EqualSid(a, b) }.is_ok();
    Ok(equal(owner, system) || equal(owner, admins))
}

/// Создаёт машинный каталог, закрывает его DACL (SYSTEM + Administrators,
/// наследуется) и только потом подкаталог логов -- он унаследует DACL.
/// Заранее отказывает, если каталог уже есть и принадлежит не админу.
fn prepare_machine_dir() -> anyhow::Result<()> {
    let dir = machine_data_dir();
    check_machine_dir_owner(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
    apply_data_dir_acl(&dir)?;
    let logs = machine_log_dir();
    std::fs::create_dir_all(&logs).with_context(|| format!("создание {}", logs.display()))?;
    Ok(())
}

/// Ставит `DATA_DIR_SDDL` на каталог. `SetNamedSecurityInfoW` (а не
/// `SetFileSecurityW`) -- он сам протаскивает наследуемые ACE на уже
/// существующие файлы, что важно при переустановке поверх старых данных.
fn apply_data_dir_acl(dir: &Path) -> anyhow::Result<()> {
    let sddl = wide_null(DATA_DIR_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // Safety: `sddl` -- валидная NUL-терминированная строка, переживает
    // вызов; `descriptor` -- валидный out-параметр, память под дескриптор
    // выделяет система (освобождается `LocalFree` ниже); размер не нужен.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;

    let result = set_dacl_from_descriptor(dir, descriptor);

    // Safety: `descriptor.0` выделен системой через `LocalAlloc` внутри
    // `Convert...` и освобождается ровно один раз, здесь, на всех путях;
    // `HLOCAL` и `PSECURITY_DESCRIPTOR` -- обёртки над одним указателем.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

fn set_dacl_from_descriptor(dir: &Path, descriptor: PSECURITY_DESCRIPTOR) -> anyhow::Result<()> {
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // Safety: `descriptor` -- валидный дескриптор из `Convert...`, указатель
    // на ACL остаётся внутри его памяти (жива до `LocalFree` в вызывающем).
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .context("GetSecurityDescriptorDacl")?;
    if !present.as_bool() || dacl.is_null() {
        anyhow::bail!("в дескрипторе {DATA_DIR_SDDL} нет DACL");
    }

    let path = wide_path(dir);
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
        .with_context(|| format!("SetNamedSecurityInfoW({})", dir.display()))
}

/// Установка службы: каталог данных + DACL, перенос `device.json` /
/// `access.json` из каталога установщика, `CreateServiceW`, описание,
/// перезапуск при сбое, старт. Нужны права администратора.
pub fn install(server: Option<&str>) -> anyhow::Result<()> {
    if let Some(server) = server {
        validate_server(server).map_err(|m| anyhow::anyhow!(m))?;
    }
    if is_installed() {
        anyhow::bail!(
            "служба {SERVICE_NAME} уже установлена; сначала `rcdesk-host service uninstall`"
        );
    }
    let exe = std::env::current_exe().context("путь к rcdesk-host.exe")?;

    let scm = open_scm(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;

    prepare_machine_dir()?;
    let machine = machine_data_dir();
    println!("Данные службы: {}", machine.display());
    let user_dir = data_dir();
    match migrate_files(&user_dir, &machine, &MIGRATED_FILES) {
        Ok(copied) if copied.is_empty() => {}
        Ok(copied) => println!(
            "Скопировано из {}: {}",
            user_dir.display(),
            copied.join(", ")
        ),
        Err(err) => anyhow::bail!("перенос данных из {}: {err}", user_dir.display()),
    }

    let name = wide_null(SERVICE_NAME);
    let display = wide_null(DISPLAY_NAME);
    let binpath = wide_null(&service_command_line(&exe, server));
    // Safety: все строки переживают вызов; `lpdwtagid`/группа загрузки/
    // зависимости/пароль -- не используются; пустое имя учётной записи
    // (`PCWSTR::null()`) значит LocalSystem.
    let created = unsafe {
        CreateServiceW(
            *scm,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(binpath.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .map_err(|e| win_err("CreateServiceW", e))?;
    // Safety: хендл только что вернул успешный `CreateServiceW`.
    let service = unsafe { Owned::new(created) };

    let mut description = wide_null(DESCRIPTION);
    let description_info = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(description.as_mut_ptr()),
    };
    // Safety: `service` -- хендл с `SERVICE_ALL_ACCESS`; `description_info`
    // и буфер строки переживают вызов.
    if let Err(err) = unsafe {
        ChangeServiceConfig2W(
            *service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::ptr::from_ref(&description_info).cast::<c_void>()),
        )
    } {
        eprintln!("предупреждение: описание службы не задано: {err}");
    }

    // Рестарт через 5 с, три раза; счётчик сбоев сбрасывается через сутки.
    let mut actions = [SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 5_000,
    }; 3];
    let failure_info = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // Safety: `service` -- хендл с `SERVICE_ALL_ACCESS`; `failure_info` и
    // массив `actions` переживают вызов, `cActions` равно его длине.
    if let Err(err) = unsafe {
        ChangeServiceConfig2W(
            *service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(std::ptr::from_ref(&failure_info).cast::<c_void>()),
        )
    } {
        eprintln!("предупреждение: перезапуск при сбое не настроен: {err}");
    }

    // Safety: `service` -- хендл с `SERVICE_ALL_ACCESS`, аргументов нет.
    match unsafe { StartServiceW(*service, None) } {
        Ok(()) => {}
        Err(e) if code_is(&e, ERROR_SERVICE_ALREADY_RUNNING.0) => {}
        Err(e) => return Err(win_err("StartServiceW", e)),
    }
    println!(
        "Служба {SERVICE_NAME} установлена и запущена ({})",
        exe.display()
    );
    Ok(())
}

/// Ждёт `STOPPED` до `STOP_WAIT`; `true`, если дождались.
fn wait_stopped(service: &Owned<SC_HANDLE>) -> anyhow::Result<bool> {
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        if query_state(service)? == ServiceState::Stopped {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Шлёт STOP; «уже остановлена» -- не ошибка.
fn send_stop(service: &Owned<SC_HANDLE>) -> anyhow::Result<()> {
    let mut status = SERVICE_STATUS::default();
    // Safety: `service` -- хендл с `SERVICE_STOP`, `status` -- валидный
    // out-параметр.
    match unsafe { ControlService(**service, SERVICE_CONTROL_STOP, &mut status) } {
        Ok(()) => Ok(()),
        Err(e) if code_is(&e, ERROR_SERVICE_NOT_ACTIVE.0) => Ok(()),
        Err(e) => Err(win_err("ControlService(STOP)", e)),
    }
}

/// Остановка и удаление службы. Данные не трогаются.
pub fn uninstall() -> anyhow::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_ALL_ACCESS)? else {
        anyhow::bail!("служба {SERVICE_NAME} не установлена");
    };
    if query_state(&service)? != ServiceState::Stopped {
        send_stop(&service)?;
        if !wait_stopped(&service)? {
            eprintln!(
                "предупреждение: служба не остановилась за {} с, удаление отложится до остановки",
                STOP_WAIT.as_secs()
            );
        }
    }
    // Safety: `service` -- хендл с правом DELETE (в `SERVICE_ALL_ACCESS`).
    unsafe { DeleteService(*service) }.map_err(|e| win_err("DeleteService", e))?;
    println!("Служба {SERVICE_NAME} удалена");
    println!("Данные не тронуты: {}", machine_data_dir().display());
    Ok(())
}

/// Запуск установленной службы.
pub fn start() -> anyhow::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_START)? else {
        anyhow::bail!("служба {SERVICE_NAME} не установлена (`rcdesk-host service install`)");
    };
    // Safety: `service` -- хендл с `SERVICE_START`, аргументов нет.
    match unsafe { StartServiceW(*service, None) } {
        Ok(()) => println!("Служба {SERVICE_NAME} запущена"),
        Err(e) if code_is(&e, ERROR_SERVICE_ALREADY_RUNNING.0) => {
            println!("Служба {SERVICE_NAME} уже работает");
        }
        Err(e) => return Err(win_err("StartServiceW", e)),
    }
    Ok(())
}

/// Остановка службы с ожиданием до 10 с.
pub fn stop() -> anyhow::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_STOP | SERVICE_QUERY_STATUS)? else {
        anyhow::bail!("служба {SERVICE_NAME} не установлена");
    };
    send_stop(&service)?;
    if wait_stopped(&service)? {
        println!("Служба {SERVICE_NAME} остановлена");
        Ok(())
    } else {
        anyhow::bail!(
            "служба {SERVICE_NAME} не остановилась за {} с",
            STOP_WAIT.as_secs()
        )
    }
}

// ---------------------------------------------------------------------------
// Работа внутри службы (`service run`).
// ---------------------------------------------------------------------------

/// Итоговый адрес сервера для помощника (флаг, `settings.json` или локальный
/// слушатель хаба); задаётся в `run` до диспетчера.
static SERVER: OnceLock<String> = OnceLock::new();
/// Режим хаба: служба поднимает LAN-хаб у себя (`settings.json`, D47-5).
static HUB_MODE: AtomicBool = AtomicBool::new(false);
/// `SERVICE_STATUS_HANDLE.0` для `report` (обработчик управления зовётся из
/// другого потока).
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Пришёл STOP/SHUTDOWN.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// `STOPPED` уже отправлен -- дальше статусов не шлём.
static FINISHED: AtomicBool = AtomicBool::new(false);

/// Точка входа `service run`: логи в `service.log`, диспетчер SCM. Возвращает
/// управление, когда служба остановилась.
pub fn run(server: Option<String>) -> anyhow::Result<()> {
    if let Some(server) = &server {
        validate_server(server).map_err(|m| anyhow::anyhow!(m))?;
    }
    let log_dir = machine_log_dir();
    logging::init_named(&log_dir, SERVICE_LOG)
        .with_context(|| format!("лог службы в {}", log_dir.display()))?;
    let (server, hub) = helper_server(server, &machine_data_dir());
    tracing::info!(
        version = crate::build_info::FULL_VERSION,
        server,
        hub,
        "rcdesk service starting"
    );
    HUB_MODE.store(hub, Ordering::SeqCst);
    let _ = SERVER.set(server);

    let mut name = wide_null(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    // Safety: `table` -- массив, оканчивающийся нулевой записью, и `name`
    // переживают вызов (он блокируется до остановки службы); `service_main`
    // имеет требуемую сигнатуру `LPSERVICE_MAIN_FUNCTIONW`.
    let result = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if let Err(err) = result {
        if code_is(&err, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.0) {
            anyhow::bail!(
                "`service run` запускается только менеджером служб; используйте `service install`/`start`"
            );
        }
        return Err(win_err("StartServiceCtrlDispatcherW", err));
    }
    tracing::info!("rcdesk service stopped");
    Ok(())
}

/// Отправляет SCM текущее состояние.
fn report(
    state: SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
    exit_code: u32,
    checkpoint: u32,
    wait_hint_ms: u32,
) {
    let raw = STATUS_HANDLE.load(Ordering::SeqCst);
    if raw.is_null() {
        return;
    }
    if state == SERVICE_STOPPED {
        FINISHED.store(true, Ordering::SeqCst);
    } else if FINISHED.load(Ordering::SeqCst) {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint_ms,
    };
    // Safety: `raw` -- хендл, полученный от `RegisterServiceCtrlHandlerExW`
    // и действительный всю жизнь службы; `status` переживает вызов.
    if let Err(err) = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(raw), &status) } {
        tracing::warn!(error = %err, "SetServiceStatus failed");
    }
}

/// Обработчик управляющих команд SCM: принимаем только STOP и SHUTDOWN.
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            report(SERVICE_STOP_PENDING, 0, 0, 1, 10_000);
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

/// `ServiceMain`: регистрирует обработчик, рапортует RUNNING, крутит
/// супервизор до STOP, рапортует STOPPED.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide_null(SERVICE_NAME);
    // Safety: `name` переживает вызов; `control_handler` имеет сигнатуру
    // `LPHANDLER_FUNCTION_EX`; контекст не используется.
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None)
    } {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(error = %err, "RegisterServiceCtrlHandlerExW failed");
            return;
        }
    };
    STATUS_HANDLE.store(handle.0, Ordering::SeqCst);

    report(SERVICE_START_PENDING, 0, 0, 1, 3_000);
    report(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
        0,
        0,
        0,
    );
    tracing::info!("rcdesk service running");

    // Хаб стартует до первого тика супервизора; помощник и так
    // переподключается с backoff, ждать готовности хаба не нужно.
    let hub = if HUB_MODE.load(Ordering::SeqCst) {
        match spawn_hub() {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::error!(error = %format!("{err:#}"), "не удалось запустить хаб");
                report(SERVICE_STOPPED, 0, 1, 0, 0);
                return;
            }
        }
    } else {
        None
    };

    let exit_code = match supervise(hub) {
        Ok(()) => 0,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "supervisor failed");
            1
        }
    };
    report(SERVICE_STOPPED, 0, exit_code, 0, 0);
}

/// Поток хаба со своим tokio-рантаймом; завершается по `STOP_REQUESTED` или
/// при падении хаба (порт занят и т.п.).
fn spawn_hub() -> anyhow::Result<std::thread::JoinHandle<anyhow::Result<()>>> {
    let cfg = HubConfig {
        data_dir: machine_data_dir().join("hub"),
        port: hub::DEFAULT_PORT,
        local_port: hub::DEFAULT_LOCAL_PORT,
    };
    std::thread::Builder::new()
        .name("rcdesk-hub".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("tokio-рантайм хаба")?;
            runtime.block_on(hub::run(cfg, hub::stop_flag_signal(&STOP_REQUESTED)))
        })
        .context("поток хаба")
}

/// Цикл надзора: раз в секунду смотрит активную консольную сессию и
/// состояние помощника, исполняет решение `Supervisor::tick`. В режиме хаба
/// выходит ещё и когда поток хаба завершился сам (ошибка -- SCM
/// перезапустит службу по failure actions); при штатной остановке ждёт
/// поток хаба.
fn supervise(hub: Option<std::thread::JoinHandle<anyhow::Result<()>>>) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("путь к rcdesk-host.exe")?;
    let server = SERVER.get().cloned().unwrap_or_default();
    let mut supervisor = Supervisor::new();
    let mut helper: Option<Helper> = None;
    let mut next_tick = Instant::now();

    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        if hub.as_ref().is_some_and(|h| h.is_finished()) {
            break;
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK_INTERVAL;
            tick_once(&mut supervisor, &mut helper, &exe, &server, now);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let Some(helper) = helper.take() {
        tracing::info!(
            pid = helper.pid,
            session = helper.session,
            "stopping helper"
        );
        helper.kill();
    }

    if let Some(hub) = hub {
        let finished_early = !STOP_REQUESTED.load(Ordering::SeqCst);
        let result = match hub.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("поток хаба паниковал")),
        };
        if finished_early {
            // Хаб упал сам: служба останавливается с кодом 1.
            return match result {
                Ok(()) => Err(anyhow::anyhow!("хаб завершился неожиданно")),
                Err(err) => Err(err.context("хаб остановился")),
            };
        }
        if let Err(err) = result {
            tracing::warn!(error = %format!("{err:#}"), "хаб завершился с ошибкой при остановке");
        }
    }
    Ok(())
}

fn tick_once(
    supervisor: &mut Supervisor,
    helper: &mut Option<Helper>,
    exe: &Path,
    server: &str,
    now: Instant,
) {
    // Safety: вызов без аргументов и предусловий.
    let raw_session = unsafe { WTSGetActiveConsoleSessionId() };
    let active = active_session_from_raw(raw_session);

    let exit_code = helper.as_ref().and_then(Helper::exit_code);
    let state = helper.as_ref().map(|h| HelperState {
        session: h.session,
        exited: exit_code.is_some(),
        started_at: h.started_at,
    });

    let action = supervisor.tick(now, active, state);

    if let (Some(code), Some(h)) = (exit_code, helper.as_ref()) {
        tracing::warn!(
            pid = h.pid,
            session = h.session,
            exit_code = code,
            uptime_s = now.saturating_duration_since(h.started_at).as_secs(),
            "helper exited"
        );
        *helper = None;
    }

    match action {
        Action::None => {}
        Action::Kill => {
            if let Some(h) = helper.take() {
                tracing::info!(
                    pid = h.pid,
                    session = h.session,
                    active = ?active,
                    "active console session changed, killing helper"
                );
                h.kill();
            }
        }
        Action::Launch(session) => match Helper::launch(exe, server, session) {
            Ok(h) => {
                tracing::info!(pid = h.pid, session, "helper started");
                *helper = Some(h);
            }
            Err(err) => {
                tracing::error!(session, error = %format!("{err:#}"), "helper launch failed");
                supervisor.launch_failed(now);
            }
        },
    }
}

/// Живой помощник: хендл процесса и Job object, который убьёт его при
/// закрытии (смерть службы).
struct Helper {
    process: Owned<HANDLE>,
    /// Не читается: держит `KILL_ON_JOB_CLOSE` до `Drop`.
    _job: Owned<HANDLE>,
    pid: u32,
    session: u32,
    started_at: Instant,
}

impl Helper {
    fn launch(exe: &Path, server: &str, session: u32) -> anyhow::Result<Self> {
        // Job object с убийством процессов при закрытии хендла.
        // Safety: без атрибутов безопасности и без имени.
        let job_handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| win_err("CreateJobObjectW", e))?;
        // Safety: хендл только что вернул успешный `CreateJobObjectW`.
        let job = unsafe { Owned::new(job_handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // BREAKAWAY_OK: установщик обновления должен пережить остановку службы (D50c).
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        // Safety: `job` -- живой хендл; `limits` соответствует классу
        // `JobObjectExtendedLimitInformation` и переживает вызов, длина
        // передаётся размером этой же структуры.
        unsafe {
            SetInformationJobObject(
                *job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| win_err("SetInformationJobObject", e))?;

        // SYSTEM-токен службы -> primary-копия -> номер сессии.
        let mut own_token = HANDLE::default();
        // Safety: `GetCurrentProcess` -- псевдохендл; `own_token` -- валидный
        // out-параметр.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY,
                &mut own_token,
            )
        }
        .map_err(|e| win_err("OpenProcessToken", e))?;
        // Safety: хендл только что вернул успешный `OpenProcessToken`.
        let own_token = unsafe { Owned::new(own_token) };

        let mut primary = HANDLE::default();
        // Safety: `own_token` -- живой хендл с `TOKEN_DUPLICATE`; `primary` --
        // валидный out-параметр; атрибуты по умолчанию (не наследуется).
        unsafe {
            DuplicateTokenEx(
                *own_token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        }
        .map_err(|e| win_err("DuplicateTokenEx", e))?;
        // Safety: хендл только что вернул успешный `DuplicateTokenEx`.
        let primary = unsafe { Owned::new(primary) };

        // Safety: `primary` -- живой primary-токен с `TOKEN_ALL_ACCESS`
        // (включая `TOKEN_ADJUST_SESSIONID`); `session` -- `u32`, длина 4
        // соответствует классу `TokenSessionId`.
        unsafe {
            SetTokenInformation(
                *primary,
                TokenSessionId,
                std::ptr::from_ref(&session).cast::<c_void>(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .map_err(|e| win_err("SetTokenInformation(TokenSessionId)", e))?;

        let mut command_line = wide_null(&helper_command_line(exe, server));
        let mut desktop = wide_null(HELPER_DESKTOP);
        let cwd = exe.parent().map(wide_path);
        let cwd_ptr = cwd.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        // Safety: `primary` -- токен с нужной сессией; имя приложения null,
        // команда в изменяемом буфере `command_line` (Win32 вправе его
        // править); атрибуты и окружение не заданы (окружение -- службы),
        // хендлы не наследуются (`false`); `cwd`, `desktop`, `startup`,
        // `info` переживают вызов.
        unsafe {
            CreateProcessAsUserW(
                Some(*primary),
                PCWSTR::null(),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                cwd_ptr,
                &startup,
                &mut info,
            )
        }
        .map_err(|e| win_err("CreateProcessAsUserW", e))?;
        // Safety: оба хендла только что вернул успешный `CreateProcessAsUserW`.
        let process = unsafe { Owned::new(info.hProcess) };
        let _thread = unsafe { Owned::new(info.hThread) };

        // Safety: оба хендла живы.
        if let Err(err) = unsafe { AssignProcessToJobObject(*job, *process) } {
            // Без job служба не гарантирует смерть помощника -- не оставляем.
            // Safety: `process` -- живой хендл с правом завершения.
            let _ = unsafe { TerminateProcess(*process, 1) };
            return Err(win_err("AssignProcessToJobObject", err));
        }

        Ok(Self {
            process,
            _job: job,
            pid: info.dwProcessId,
            session,
            started_at: Instant::now(),
        })
    }

    /// Код выхода, если процесс уже завершился.
    fn exit_code(&self) -> Option<u32> {
        // Safety: `process` -- живой хендл; нулевое ожидание только опрашивает.
        if unsafe { WaitForSingleObject(*self.process, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        // Safety: процесс завершён, `code` -- валидный out-параметр.
        match unsafe { GetExitCodeProcess(*self.process, &mut code) } {
            Ok(()) => Some(code),
            Err(_) => Some(u32::MAX),
        }
    }

    /// Завершает процесс; хендлы (и job) закрываются при выходе из функции.
    fn kill(self) {
        // Safety: `process` -- живой хендл с правом завершения.
        if let Err(err) = unsafe { TerminateProcess(*self.process, 1) } {
            tracing::warn!(pid = self.pid, error = %err, "TerminateProcess failed");
        }
    }
}

#[cfg(test)]
mod tests {
    // Без юнит-тестов: каждая функция здесь трогает реальные SCM/токены, а
    // Windows-машины у исполнителя нет. Чистая логика (командные строки,
    // супервизор, pin.txt, перенос файлов) тестируется в `crate::service`.
}
