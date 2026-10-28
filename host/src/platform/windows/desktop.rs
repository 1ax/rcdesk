//! Следование потока за входным рабочим столом (D45a).
//!
//! Когда Windows показывает экран блокировки или запрос UAC, входным
//! становится рабочий стол `Winlogon` той же оконной станции, а потоки
//! помощника остаются на `Default`: захват чёрный/замерший, ввод не
//! доходит. [`sync_thread`] перепривязывает вызывающий поток к входному
//! рабочему столу (`OpenInputDesktop` + `SetThreadDesktop`). Включается
//! только в машинном режиме через [`enable`]; без него все вызовы -- no-op.
//!
//! Компилируется только на `windows-latest` в CI: типы и сигнатуры
//! сверены с исходником `windows` 0.61.3 (`Win32/System/StationsAndDesktops`).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// Минимальный интервал между проверками входного рабочего стола на поток.
const CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Не чаще одного `warn` за этот интервал на поток.
const WARN_INTERVAL: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Включает следование за входным рабочим столом для всего процесса.
pub fn enable() {
    ENABLED.store(true, Ordering::Release);
}

/// Включено ли следование.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

struct ThreadState {
    last_check: Option<Instant>,
    last_warn: Option<Instant>,
    /// Имя рабочего стола, к которому привязан поток (после нашего `SetThreadDesktop`).
    name: Option<String>,
    /// Открытый хендл этого рабочего стола; держим, пока поток к нему привязан.
    hdesk: Option<HDESK>,
}

thread_local! {
    static STATE: RefCell<ThreadState> = const {
        RefCell::new(ThreadState {
            last_check: None,
            last_warn: None,
            name: None,
            hdesk: None,
        })
    };
}

/// Пора ли проверять: первая проверка потока -- всегда, дальше не чаще `interval`.
fn due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last {
        None => true,
        Some(prev) => now.saturating_duration_since(prev) >= interval,
    }
}

/// Сменился ли рабочий стол; отсутствие запомненного имени считается сменой.
fn desktop_changed(current: Option<&str>, new: &str) -> bool {
    current != Some(new)
}

/// Привязывает поток к входному рабочему столу, не чаще раза в 100 мс.
/// `true` -- поток только что перешёл на другой рабочий стол.
pub fn sync_thread() -> bool {
    sync(false)
}

/// То же, но без троттлинга.
pub fn sync_thread_now() -> bool {
    sync(true)
}

fn sync(force: bool) -> bool {
    if !is_enabled() {
        return false;
    }
    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        let now = Instant::now();
        if !force && !due(st.last_check, now, CHECK_INTERVAL) {
            return false;
        }
        st.last_check = Some(now);
        match try_switch(&mut st) {
            Ok(changed) => changed,
            Err(msg) => {
                if due(st.last_warn, now, WARN_INTERVAL) {
                    st.last_warn = Some(now);
                    tracing::warn!("input desktop sync failed: {msg}");
                }
                false
            }
        }
    })
}

fn try_switch(st: &mut ThreadState) -> Result<bool, String> {
    // Safety: параметры -- значения по умолчанию; в случае ошибки возвращается `Err`.
    let hdesk = unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
    }
    .map_err(|err| format!("OpenInputDesktop: {err}"))?;

    let name = match desktop_name(hdesk) {
        Ok(name) => name,
        Err(msg) => {
            close(hdesk);
            return Err(msg);
        }
    };
    if !desktop_changed(st.name.as_deref(), &name) {
        close(hdesk);
        return Ok(false);
    }
    // Safety: `hdesk` -- только что открытый валидный хендл рабочего стола.
    if let Err(err) = unsafe { SetThreadDesktop(hdesk) } {
        close(hdesk);
        return Err(format!("SetThreadDesktop({name}): {err}"));
    }
    if let Some(old) = st.hdesk.replace(hdesk) {
        close(old);
    }
    tracing::info!(desktop = %name, "thread follows input desktop");
    st.name = Some(name);
    Ok(true)
}

fn close(hdesk: HDESK) {
    // Safety: `hdesk` открыт нами и больше не используется; для привязанного
    // потока старый хендл закрывают уже после успешного `SetThreadDesktop`.
    let _ = unsafe { CloseDesktop(hdesk) };
}

fn desktop_name(hdesk: HDESK) -> Result<String, String> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    // Safety: `buf` -- валидный буфер указанной длины в байтах, `needed` -- out-параметр;
    // `HDESK` и `HANDLE` -- один и тот же указатель на объект USER.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            std::mem::size_of_val(&buf) as u32,
            Some(&mut needed),
        )
    }
    .map_err(|err| format!("GetUserObjectInformationW: {err}"))?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_check_is_always_due() {
        assert!(due(None, Instant::now(), CHECK_INTERVAL));
    }

    #[test]
    fn check_is_throttled_within_interval() {
        let t0 = Instant::now();
        assert!(!due(
            Some(t0),
            t0 + Duration::from_millis(50),
            CHECK_INTERVAL
        ));
    }

    #[test]
    fn check_is_due_after_interval() {
        let t0 = Instant::now();
        assert!(due(Some(t0), t0 + CHECK_INTERVAL, CHECK_INTERVAL));
    }

    #[test]
    fn missing_name_counts_as_change() {
        assert!(desktop_changed(None, "Default"));
    }

    #[test]
    fn same_name_is_not_change() {
        assert!(!desktop_changed(Some("Default"), "Default"));
    }

    #[test]
    fn different_name_is_change() {
        assert!(desktop_changed(Some("Default"), "Winlogon"));
    }

    #[test]
    fn disabled_sync_is_noop() {
        // `enable()` в тестах не вызывается: процессный флаг остаётся выключенным.
        assert!(!is_enabled());
        assert!(!sync_thread());
        assert!(!sync_thread_now());
    }
}
