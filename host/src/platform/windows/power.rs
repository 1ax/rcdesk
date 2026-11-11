//! Keeps the display/system awake for the duration of an active remote
//! session (slice 2.6c) -- the Windows counterpart of
//! `platform::macos::activity`. `rcdesk-agent` has no visible window and no
//! direct user input while a session runs, so without this the OS may sleep
//! the display or the whole system out from under a live session.
//!
//! V-14: `ES_DISPLAY_REQUIRED` не включает уже погасший дисплей, а при
//! погасшем дисплее GDI-захват отдаёт застывший кадр (1 кадр/с на стенде);
//! включает дисплей только ввод. Поэтому `begin_session_activity` в конце
//! сдвигает указатель на 1 пиксель и обратно (`wake_display`) -- курсор
//! остаётся на месте.
//!
//! Must be called from the same thread every time (`SetThreadExecutionState`
//! is per-thread) -- `agent_main` calls both from its main thread, the one
//! driving `platform::windows::event_loop::pump`.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see `platform::windows::mouse`'s doc comment,
//! which documents and uses the same approach.

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_XVIRTUALSCREEN,
};

/// Call when a session becomes active: keeps the system and display from
/// sleeping until `end_session_activity` is called or the process exits.
///
/// Затем будит дисплей (V-14): `ES_DISPLAY_REQUIRED` лишь не даёт ему
/// погаснуть дальше, уже погасший он не включает, а захват при этом отдаёт
/// застывший кадр. Включает только ввод -- `wake_display` сдвигает указатель
/// на 1 пиксель и возвращает обратно.
pub fn begin_session_activity() {
    // Safety: `SetThreadExecutionState` has no preconditions beyond passing
    // a valid `EXECUTION_STATE` flag combination; `ES_CONTINUOUS` here means
    // "keep this state in effect until changed again", not just for this
    // one call.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
    wake_display();
}

/// Соседний по X пиксель для «толчка» указателя: `x + 1`, если он внутри
/// виртуального экрана, иначе `x - 1`.
fn nudge_x(x: i32, virt_left: i32, virt_width: i32) -> i32 {
    if x + 1 < virt_left + virt_width {
        x + 1
    } else {
        x - 1
    }
}

/// Будит погасший дисплей движением мыши на 1 пиксель и обратно (V-14).
/// Ошибки только логируются: сеанс без пробуждения лучше, чем падение.
fn wake_display() {
    // В машинном режиме поток должен стоять на входном рабочем столе, иначе
    // `SendInput` на `Winlogon` не пройдёт; без машинного режима -- no-op.
    let _ = super::desktop::sync_thread_now();

    let mut pos = POINT::default();
    // Safety: `pos` -- валидный изменяемый `POINT`, `GetCursorPos` только
    // записывает в него и не хранит указатель после вызова.
    if let Err(err) = unsafe { GetCursorPos(&mut pos) } {
        tracing::warn!(error = %err, "не удалось прочитать позицию курсора для пробуждения дисплея");
        return;
    }
    // Safety: `GetSystemMetrics` не имеет предусловий; оба `SM_*` --
    // допустимые `SYSTEM_METRICS_INDEX`.
    let (left, width) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
        )
    };
    let nudged = nudge_x(pos.x, left, width);
    if let Err(err) = super::mouse::move_to(nudged, pos.y) {
        tracing::warn!(error = %err, "не удалось сдвинуть указатель для пробуждения дисплея");
        return;
    }
    if let Err(err) = super::mouse::move_to(pos.x, pos.y) {
        tracing::warn!(error = %err, "не удалось вернуть указатель после пробуждения дисплея");
        return;
    }
    tracing::info!("дисплей разбужен сдвигом указателя на 1 пиксель и обратно (V-14)");
}

/// Call when a session ends: clears the flags `begin_session_activity` set,
/// letting the system/display sleep normally again.
pub fn end_session_activity() {
    // Safety: same as `begin_session_activity` -- `ES_CONTINUOUS` alone
    // means "stop overriding the idle timers", the documented way to
    // release a previous `SetThreadExecutionState` call from the same
    // thread.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS);
    }
}

/// Выделенный std-поток для `SetThreadExecutionState` (слайс 4.1a).
/// Состояние выполнения привязано к потоку, а tokio-задачи переезжают между
/// потоками рантайма -- поэтому `serve --machine` не зовёт `begin`/`end`
/// из задач, а шлёт сюда флаг «сессия идёт». Закрытие отправителя (drop)
/// завершает поток; перед выходом он снимает флаги.
pub struct SessionActivityThread {
    tx: std::sync::mpsc::Sender<bool>,
}

impl SessionActivityThread {
    pub fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("session-activity".to_string())
            .spawn(move || {
                while let Ok(active) = rx.recv() {
                    if active {
                        begin_session_activity();
                    } else {
                        end_session_activity();
                    }
                }
                end_session_activity();
            });
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "не удалось запустить поток удержания от сна");
        }
        Self { tx }
    }

    /// `true` -- сессия началась (держать систему от сна), `false` -- закончилась.
    pub fn set(&self, active: bool) {
        // Поток мог не стартовать -- тогда отправка молча не удаётся.
        let _ = self.tx.send(active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_x_moves_right_inside_screen() {
        assert_eq!(nudge_x(100, 0, 1920), 101);
    }

    #[test]
    fn nudge_x_moves_left_at_right_edge() {
        assert_eq!(nudge_x(1919, 0, 1920), 1918);
    }

    #[test]
    fn nudge_x_moves_right_with_negative_virt_left() {
        // Монитор слева от основного: виртуальный экран -1920..1920.
        assert_eq!(nudge_x(-500, -1920, 3840), -499);
    }

    #[test]
    fn nudge_x_moves_left_at_right_edge_with_negative_virt_left() {
        assert_eq!(nudge_x(1919, -1920, 3840), 1918);
    }
}
