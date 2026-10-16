//! Будит спящий дисплей при старте сеанса (слайс 4.6a). Пока дисплей спит,
//! `capture::list_displays` не видит ни одного дисплея и сеанс отбивается.
//! `IOPMAssertionDeclareUserActivity` сообщает системе о «пользовательской
//! активности» -- дисплей включается, как от движения мыши. Слайс I7: guard
//! живёт ВЕСЬ сеанс и каждые `RENEW_INTERVAL` продлевает активность, потому что
//! macOS гасит разбуженный дисплей через ~10 с, несмотря на
//! `activity::SessionActivity`. Ассерция отпускается в `Drop` guard-а.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_foundation::CFString;

type IOReturn = i32;
type IOPMAssertionID = u32;
const K_IORETURN_SUCCESS: IOReturn = 0;
/// `kIOPMUserActiveLocal`: активность пользователя за локальной консолью.
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;
/// Период продления активности пользователя.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: *const CFString,
        user_type: u32,
        assertion_id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
}

/// Объявляет (`id == 0`) или продлевает (`id != 0`) активность пользователя.
/// Возвращает актуальный id ассерции либо код ошибки.
fn declare_activity(id: IOPMAssertionID) -> Result<IOPMAssertionID, IOReturn> {
    let name = CFString::from_str("rcdesk remote session");
    let mut out = id;
    // Safety: `name` -- валидный CFString, живущий до конца вызова (функция
    // его не удерживает без Retain); `out` -- валидный указатель на локальную
    // переменную: при `id != 0` API продлевает существующую ассерцию, иначе
    // пишет в неё новый id при успехе.
    let rc = unsafe {
        IOPMAssertionDeclareUserActivity(
            &*name as *const CFString,
            K_IOPM_USER_ACTIVE_LOCAL,
            &mut out as *mut IOPMAssertionID,
        )
    };
    if rc == K_IORETURN_SUCCESS {
        Ok(out)
    } else {
        Err(rc)
    }
}

struct Renewer {
    stop_tx: Sender<()>,
    /// Возвращает последний актуальный id ассерции.
    handle: JoinHandle<IOPMAssertionID>,
}

/// Держит ассерцию UserIsActive и продлевает её фоновым потоком; в `Drop`
/// останавливает поток и отпускает ассерцию. `None` -- заглушка (вызов не
/// удался, сеанс от этого не падает).
pub struct DisplayWakeGuard(Option<Renewer>);

/// Будит дисплей и возвращает guard ассерции.
pub fn wake() -> DisplayWakeGuard {
    let id = match declare_activity(0) {
        Ok(id) => id,
        Err(rc) => {
            tracing::warn!(code = rc, "IOPMAssertionDeclareUserActivity failed");
            return DisplayWakeGuard(None);
        }
    };
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("rcdesk-display-wake".into())
        .spawn(move || renew_loop(id, stop_rx));
    match spawned {
        Ok(handle) => DisplayWakeGuard(Some(Renewer { stop_tx, handle })),
        Err(err) => {
            tracing::warn!(?err, "failed to spawn display wake thread");
            release(id);
            DisplayWakeGuard(None)
        }
    }
}

/// Продлевает активность каждые `RENEW_INTERVAL` до сигнала/закрытия канала;
/// возвращает последний id. Ошибку продления логирует один раз подряд.
fn renew_loop(mut id: IOPMAssertionID, stop_rx: mpsc::Receiver<()>) -> IOPMAssertionID {
    let mut failing = false;
    loop {
        match stop_rx.recv_timeout(RENEW_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => match declare_activity(id) {
                Ok(new_id) => {
                    id = new_id;
                    failing = false;
                }
                Err(rc) => {
                    if !failing {
                        tracing::warn!(code = rc, "display wake renewal failed");
                    }
                    failing = true;
                }
            },
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return id,
        }
    }
}

fn release(id: IOPMAssertionID) {
    // Safety: `id` получен от успешного `IOPMAssertionDeclareUserActivity`
    // и отпускается ровно один раз.
    let rc = unsafe { IOPMAssertionRelease(id) };
    if rc != K_IORETURN_SUCCESS {
        tracing::warn!(code = rc, "IOPMAssertionRelease failed");
    }
}

impl Drop for DisplayWakeGuard {
    fn drop(&mut self) {
        if let Some(Renewer { stop_tx, handle }) = self.0.take() {
            let _ = stop_tx.send(());
            match handle.join() {
                Ok(id) => release(id),
                Err(_) => tracing::warn!("display wake thread panicked, assertion leaked"),
            }
        }
    }
}
