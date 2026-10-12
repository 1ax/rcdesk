//! Будит спящий дисплей при старте сеанса (слайс 4.6a). Пока дисплей спит,
//! `capture::list_displays` не видит ни одного дисплея и сеанс отбивается.
//! `IOPMAssertionDeclareUserActivity` сообщает системе о «пользовательской
//! активности» -- дисплей включается, как от движения мыши. Ассерция живёт
//! только пока жив guard; дальше дисплей держит `activity::SessionActivity`.

use objc2_core_foundation::CFString;

type IOReturn = i32;
type IOPMAssertionID = u32;
const K_IORETURN_SUCCESS: IOReturn = 0;
/// `kIOPMUserActiveLocal`: активность пользователя за локальной консолью.
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: *const CFString,
        user_type: u32,
        assertion_id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
}

/// Держит ассерцию UserIsActive; в `Drop` отпускает её. `None` -- заглушка
/// (вызов не удался, сеанс от этого не падает).
pub struct DisplayWakeGuard(Option<IOPMAssertionID>);

/// Будит дисплей и возвращает guard ассерции.
pub fn wake() -> DisplayWakeGuard {
    let name = CFString::from_str("rcdesk remote session");
    let mut id: IOPMAssertionID = 0;
    // Safety: `name` -- валидный CFString, живущий до конца вызова (функция
    // его не удерживает без Retain); `id` -- валидный указатель на локальную
    // переменную, в которую функция пишет id при успехе.
    let rc = unsafe {
        IOPMAssertionDeclareUserActivity(
            &*name as *const CFString,
            K_IOPM_USER_ACTIVE_LOCAL,
            &mut id as *mut IOPMAssertionID,
        )
    };
    if rc == K_IORETURN_SUCCESS {
        DisplayWakeGuard(Some(id))
    } else {
        tracing::warn!(code = rc, "IOPMAssertionDeclareUserActivity failed");
        DisplayWakeGuard(None)
    }
}

impl Drop for DisplayWakeGuard {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            // Safety: `id` получен от успешного `IOPMAssertionDeclareUserActivity`
            // в `wake` и отпускается ровно один раз (`take`).
            let rc = unsafe { IOPMAssertionRelease(id) };
            if rc != K_IORETURN_SUCCESS {
                tracing::warn!(code = rc, "IOPMAssertionRelease failed");
            }
        }
    }
}
