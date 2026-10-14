//! Шторка на macOS (слайс 4.2c2): чёрные окна `NSWindow` поверх всех экранов
//! и блокировка локального ввода активным `CGEventTap`. Реализует трейт
//! `crate::curtain::Curtain` для `rcdesk-agent`.
//!
//! AppKit и run loop принадлежат главному потоку, а `Curtain` зовут из
//! произвольных потоков (`spawn_blocking`). Поэтому тип-прокси `MacCurtain`
//! шлёт команды по `std::sync::mpsc` в главный поток и ждёт ответ с таймаутом
//! (см. `Bridge`); главный цикл агента после каждого `pump` вызывает
//! `CurtainMain::poll`, который выполняет все накопившиеся команды.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSScreen, NSTextField, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFBoolean, CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, CFString,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGSessionCopyCurrentDictionary,
    CGShieldingWindowLevel,
};
use objc2_foundation::{NSPoint, NSSize};

use crate::curtain::Curtain;

/// Сколько `MacCurtain` ждёт ответа главного потока.
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Общее время ожидания блокировки экрана в `lock_screen`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);
/// Шаг опроса состояния сеанса.
const LOCK_POLL: Duration = Duration::from_millis(100);
/// `kVK_ANSI_Q`.
const KEY_Q: u16 = 12;
/// Заголовок окон шторки: scap перечисляет только окна с заголовком, а
/// исключение из захвата строится по ним (см. `capture::scap::excluded_targets`).
const WINDOW_TITLE: &str = "rcdesk curtain";
const CURTAIN_TEXT: &str = "Удалённый сеанс rcdesk — экран скрыт";

// ---------------------------------------------------------------------------
// Мост в главный поток (платформенно-независимая часть)
// ---------------------------------------------------------------------------

/// Запрос + канал для ответа.
struct Envelope<Req, Resp> {
    request: Req,
    reply: Sender<Resp>,
}

/// Ошибка вызова через мост.
#[derive(Debug, PartialEq, Eq)]
enum BridgeError {
    /// Главный поток не ответил за отведённое время.
    Timeout,
    /// Принимающая сторона уничтожена.
    Closed,
}

/// Клиентская сторона моста: безопасна для вызова из любого потока.
struct BridgeClient<Req, Resp> {
    tx: Mutex<Sender<Envelope<Req, Resp>>>,
    timeout: Duration,
}

impl<Req, Resp> BridgeClient<Req, Resp> {
    fn call(&self, request: Req) -> Result<Resp, BridgeError> {
        let (reply, reply_rx) = mpsc::channel();
        {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            tx.send(Envelope { request, reply })
                .map_err(|_| BridgeError::Closed)?;
        }
        match reply_rx.recv_timeout(self.timeout) {
            Ok(resp) => Ok(resp),
            Err(RecvTimeoutError::Timeout) => Err(BridgeError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(BridgeError::Closed),
        }
    }
}

/// Серверная сторона: выполняет все ожидающие запросы (`try_recv` до
/// опустошения) и возвращает, сколько обслужено.
fn serve_pending<Req, Resp>(
    rx: &Receiver<Envelope<Req, Resp>>,
    mut handler: impl FnMut(Req) -> Resp,
) -> usize {
    let mut served = 0;
    while let Ok(envelope) = rx.try_recv() {
        let resp = handler(envelope.request);
        // Клиент мог уйти по таймауту -- ответ тогда некому читать.
        let _ = envelope.reply.send(resp);
        served += 1;
    }
    served
}

fn bridge<Req, Resp>(
    timeout: Duration,
) -> (BridgeClient<Req, Resp>, Receiver<Envelope<Req, Resp>>) {
    let (tx, rx) = mpsc::channel();
    (
        BridgeClient {
            tx: Mutex::new(tx),
            timeout,
        },
        rx,
    )
}

enum Command {
    Show,
    Hide,
}

enum Reply {
    Shown(Result<Vec<u32>, String>),
    Hidden,
}

// ---------------------------------------------------------------------------
// Прокси для потоков tokio
// ---------------------------------------------------------------------------

/// Реализация `Curtain` для не-главных потоков.
pub struct MacCurtain {
    client: BridgeClient<Command, Reply>,
    /// Окна показанной сейчас шторки (для `Curtain::shown`).
    shown_ids: Mutex<Vec<u32>>,
}

/// Создаёт пару: прокси для `HostContext` и сторону главного потока.
pub fn create() -> (MacCurtain, CurtainMain) {
    let (client, rx) = bridge(BRIDGE_TIMEOUT);
    (
        MacCurtain {
            client,
            shown_ids: Mutex::new(Vec::new()),
        },
        CurtainMain {
            rx,
            controller: Controller::default(),
        },
    )
}

impl Curtain for MacCurtain {
    fn show(&self) -> anyhow::Result<Vec<u32>> {
        match self.client.call(Command::Show) {
            Ok(Reply::Shown(result)) => {
                let ids = result.map_err(|e| anyhow!(e))?;
                if let Ok(mut shown) = self.shown_ids.lock() {
                    *shown = ids.clone();
                }
                Ok(ids)
            }
            Ok(Reply::Hidden) => Err(anyhow!("неожиданный ответ главного потока")),
            Err(err) => Err(anyhow!("шторка: главный поток не ответил ({err:?})")),
        }
    }

    fn hide(&self) {
        match self.client.call(Command::Hide) {
            Ok(Reply::Hidden) => {
                if let Ok(mut shown) = self.shown_ids.lock() {
                    shown.clear();
                }
            }
            Ok(Reply::Shown(_)) => {
                tracing::warn!("шторка: неожиданный ответ главного потока на hide");
            }
            Err(err) => tracing::warn!(?err, "шторка: hide не выполнен главным потоком"),
        }
    }

    fn lock_screen(&self) -> anyhow::Result<()> {
        lock_screen_with(std::thread::sleep)
    }

    fn shown(&self) -> Vec<u32> {
        self.shown_ids
            .lock()
            .map(|ids| ids.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Сторона главного потока
// ---------------------------------------------------------------------------

/// Принимающая сторона моста + состояние шторки. Живёт на главном потоке
/// (не `Send`: держит объекты AppKit).
pub struct CurtainMain {
    rx: Receiver<Envelope<Command, Reply>>,
    controller: Controller,
}

impl CurtainMain {
    /// Выполняет все ожидающие команды. Звать из главного цикла после `pump`.
    pub fn poll(&mut self) {
        let controller = &mut self.controller;
        serve_pending(&self.rx, |cmd| match cmd {
            Command::Show => Reply::Shown(controller.show().map_err(|e| format!("{e:#}"))),
            Command::Hide => {
                controller.hide();
                Reply::Hidden
            }
        });
    }

    /// Выход агента: если шторка всё ещё показана -- блокирует экран и
    /// снимает шторку прямо на главном потоке. `pump_wait` обслуживает
    /// главный run loop во время ожидания блокировки (иначе наш же tap
    /// встанет на таймауте).
    pub fn finish(&mut self, mut pump_wait: impl FnMut(Duration)) {
        if !self.controller.is_shown() {
            return;
        }
        if !screen_is_locked() {
            if let Err(err) = lock_screen_with(&mut pump_wait) {
                tracing::warn!(error = %err, "шторка: экран не заблокирован при выходе");
            }
        }
        self.controller.hide();
    }
}

/// Состояние показанной шторки.
#[derive(Default)]
struct Controller {
    shown: Option<Shown>,
}

struct Shown {
    windows: Vec<Retained<NSWindow>>,
    ids: Vec<u32>,
    tap: InputTap,
}

impl Controller {
    fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    fn show(&mut self) -> anyhow::Result<Vec<u32>> {
        if let Some(shown) = &self.shown {
            return Ok(shown.ids.clone());
        }
        let mtm = MainThreadMarker::new().context("шторка: show вне главного потока")?;
        let screens = NSScreen::screens(mtm);
        let mut windows = Vec::new();
        let mut ids = Vec::new();
        for screen in screens.iter() {
            let window = make_window(mtm, &screen);
            window.orderFrontRegardless();
            let number = window.windowNumber();
            if number <= 0 {
                tracing::warn!(number, "шторка: у окна нет номера (windowNumber <= 0)");
            } else {
                ids.push(number as u32);
            }
            windows.push(window);
        }
        if windows.is_empty() {
            return Err(anyhow!("шторка: нет ни одного экрана"));
        }
        // Если tap не создался (нет Универсального доступа), шторка без блокировки
        // ввода -- не шторка: откатываем окна и сообщаем об ошибке.
        let tap = match InputTap::install() {
            Ok(tap) => tap,
            Err(err) => {
                close_windows(&windows);
                return Err(err);
            }
        };
        tracing::info!(windows = ids.len(), "шторка показана");
        self.shown = Some(Shown {
            windows,
            ids: ids.clone(),
            tap,
        });
        Ok(ids)
    }

    fn hide(&mut self) {
        if let Some(shown) = self.shown.take() {
            shown.tap.remove();
            close_windows(&shown.windows);
            tracing::info!("шторка снята");
        }
    }
}

fn close_windows(windows: &[Retained<NSWindow>]) {
    for window in windows {
        window.orderOut(None);
    }
}

fn make_window(mtm: MainThreadMarker, screen: &NSScreen) -> Retained<NSWindow> {
    let frame = screen.frame();
    // Safety: стандартный инициализатор NSWindow на главном потоке; defer=false.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // Safety: окно живёт в `Retained` у нас; закрытие не должно делать лишний release.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&objc2_foundation::NSString::from_str(WINDOW_TITLE));
    // Не `setOpaque(true)`: непрозрачное окно на весь экран macOS считает
    // перекрывающим всё под собой (`occlusionState` без `.visible`), и
    // приложения перестают рисоваться -- в захвате застывший рабочий стол.
    // 99 % чёрного на глаз не отличить, а окна под шторкой остаются
    // «видимыми» (проверено пробой 2026-10-12).
    window.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.99)));
    window.setOpaque(false);
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    window.setLevel(CGShieldingWindowLevel() as isize);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setFrame_display(frame, false);

    let label =
        NSTextField::labelWithString(&objc2_foundation::NSString::from_str(CURTAIN_TEXT), mtm);
    label.setTextColor(Some(&NSColor::lightGrayColor()));
    label.setFont(Some(&NSFont::systemFontOfSize(24.0)));
    label.sizeToFit();
    let label_size: NSSize = label.frame().size;
    label.setFrameOrigin(NSPoint::new(
        ((frame.size.width - label_size.width) / 2.0).max(0.0),
        ((frame.size.height - label_size.height) / 2.0).max(0.0),
    ));
    if let Some(content) = window.contentView() {
        content.addSubview(&label);
    }
    window
}

// ---------------------------------------------------------------------------
// Блокировка локального ввода
// ---------------------------------------------------------------------------

/// Решение фильтра событий.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TapDecision {
    /// Пропустить событие дальше.
    Pass,
    /// Отбросить событие.
    Drop,
    /// Система отключила tap -- включить заново.
    Reenable,
}

/// Чистая функция: что сделать с событием типа `event_type` с полем
/// `EventSourceUserData` = `user_data`. Свои события (инъекция enigo)
/// помечены `enigo::EVENT_MARKER`.
fn tap_decision(event_type: CGEventType, user_data: i64) -> TapDecision {
    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        TapDecision::Reenable
    } else if user_data == i64::from(enigo::EVENT_MARKER) {
        TapDecision::Pass
    } else {
        TapDecision::Drop
    }
}

fn event_mask() -> CGEventMask {
    [
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
    ]
    .iter()
    .fold(0, |mask, t| mask | (1u64 << t.0))
}

/// Состояние, доступное из колбэка tap: сам порт (для повторного включения).
struct TapState {
    port: *const CFMachPort,
}

/// Установленный tap + его run loop source.
struct InputTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    /// Должен жить, пока жив tap (колбэк держит сырой указатель).
    state: Box<TapState>,
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // Для служебных типов "tap отключён" событие не определено -- не читаем.
    let disabled = event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput;
    let user_data = if disabled {
        0
    } else {
        // Safety: `event` валиден на время вызова колбэка.
        unsafe {
            CGEvent::integer_value_field(Some(event.as_ref()), CGEventField::EventSourceUserData)
        }
    };
    match tap_decision(event_type, user_data) {
        TapDecision::Pass => event.as_ptr(),
        TapDecision::Drop => ptr::null_mut(),
        TapDecision::Reenable => {
            // Safety: `user_info` -- `*mut TapState`, живёт, пока tap установлен
            // (`InputTap::remove` сначала гасит tap, потом освобождает состояние).
            let state = unsafe { &*(user_info as *const TapState) };
            if let Some(port) = unsafe { state.port.as_ref() } {
                CGEvent::tap_enable(port, true);
            }
            tracing::warn!(
                ?event_type,
                "шторка: tap был отключён системой, включён заново"
            );
            ptr::null_mut()
        }
    }
}

impl InputTap {
    fn install() -> anyhow::Result<Self> {
        let mut state = Box::new(TapState { port: ptr::null() });
        let user_info = &mut *state as *mut TapState as *mut c_void;
        // Safety: колбэк реализован корректно (см. `tap_callback`), `user_info`
        // указывает на `Box<TapState>`, который живёт дольше tap.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                event_mask(),
                Some(tap_callback),
                user_info,
            )
        }
        .ok_or_else(|| {
            anyhow!(
                "шторка: не удалось создать event tap (нужно разрешение «Универсальный доступ»)"
            )
        })?;
        state.port = &*port as *const CFMachPort;
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0)
            .ok_or_else(|| anyhow!("шторка: не удалось создать run loop source для tap"))?;
        let run_loop = CFRunLoop::main().ok_or_else(|| anyhow!("шторка: нет главного run loop"))?;
        // Safety: `kCFRunLoopCommonModes` -- константа CoreFoundation.
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&port, true);
        Ok(Self {
            port,
            source,
            state,
        })
    }

    fn remove(self) {
        CGEvent::tap_enable(&self.port, false);
        if let Some(run_loop) = CFRunLoop::main() {
            // Safety: см. `install`.
            run_loop.remove_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        }
        self.port.invalidate();
        // `state` освобождается после невозможности нового вызова колбэка.
        drop(self.state);
    }
}

// ---------------------------------------------------------------------------
// Блокировка экрана
// ---------------------------------------------------------------------------

/// Значение ключа `CGSSessionScreenIsLocked` -> заблокирован ли экран.
/// Ключ присутствует (true) только при блокировке.
fn locked_from_flag(flag: Option<bool>) -> bool {
    flag.unwrap_or(false)
}

fn screen_is_locked() -> bool {
    let Some(dict) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let key = CFString::from_static_str("CGSSessionScreenIsLocked");
    // Safety: ключ -- валидный CFString; для отсутствующего ключа вернётся null,
    // для присутствующего -- CFBoolean (по документации CGSession).
    let flag = unsafe {
        let value = dict.value(&*key as *const CFString as *const c_void);
        (value as *const CFBoolean).as_ref().map(CFBoolean::value)
    };
    locked_from_flag(flag)
}

fn post_lock_hotkey() -> anyhow::Result<()> {
    for key_down in [true, false] {
        let event = CGEvent::new_keyboard_event(None, KEY_Q, key_down)
            .ok_or_else(|| anyhow!("не удалось создать CGEvent для Ctrl+Cmd+Q"))?;
        CGEvent::set_flags(
            Some(&event),
            CGEventFlags::MaskControl | CGEventFlags::MaskCommand,
        );
        // Метка "свои события": иначе наш же tap отбросит нажатие.
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            i64::from(enigo::EVENT_MARKER),
        );
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

/// Шлёт Ctrl+Cmd+Q и ждёт блокировки до `LOCK_TIMEOUT`; `wait` -- способ
/// подождать `LOCK_POLL` (поток спит, либо главный поток крутит run loop).
fn lock_screen_with(mut wait: impl FnMut(Duration)) -> anyhow::Result<()> {
    post_lock_hotkey()?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        wait(LOCK_POLL);
        if screen_is_locked() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("экран не заблокировался за 3 с"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn bridge_serves_command_and_returns_reply() {
        let (client, rx) = bridge::<u32, u32>(Duration::from_secs(2));
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if serve_pending(&rx, |n| n + 1) > 0 {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        assert_eq!(client.call(41), Ok(42));
        server.join().unwrap();
    }

    #[test]
    fn bridge_times_out_when_nobody_serves() {
        let (client, _rx) = bridge::<u32, u32>(Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(client.call(1), Err(BridgeError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bridge_reports_closed_when_server_dropped() {
        let (client, rx) = bridge::<u32, u32>(Duration::from_secs(1));
        drop(rx);
        assert_eq!(client.call(1), Err(BridgeError::Closed));
    }

    #[test]
    fn serve_pending_drains_all_queued_commands() {
        let (client, rx) = bridge::<u32, u32>(Duration::from_millis(30));
        let client = std::sync::Arc::new(client);
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let c = std::sync::Arc::clone(&client);
                thread::spawn(move || {
                    let _ = c.call(i);
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(10));
        let served = serve_pending(&rx, |n| n);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(served, 3);
    }

    #[test]
    fn tap_passes_own_marked_events() {
        let marker = i64::from(enigo::EVENT_MARKER);
        assert_eq!(
            tap_decision(CGEventType::KeyDown, marker),
            TapDecision::Pass
        );
        assert_eq!(
            tap_decision(CGEventType::MouseMoved, marker),
            TapDecision::Pass
        );
    }

    #[test]
    fn tap_drops_foreign_events() {
        assert_eq!(tap_decision(CGEventType::KeyDown, 0), TapDecision::Drop);
        assert_eq!(
            tap_decision(CGEventType::LeftMouseDown, 99),
            TapDecision::Drop
        );
        assert_eq!(
            tap_decision(CGEventType::ScrollWheel, 101),
            TapDecision::Drop
        );
    }

    #[test]
    fn tap_reenables_when_disabled_by_system() {
        assert_eq!(
            tap_decision(CGEventType::TapDisabledByTimeout, 0),
            TapDecision::Reenable
        );
        assert_eq!(
            tap_decision(CGEventType::TapDisabledByUserInput, 0),
            TapDecision::Reenable
        );
    }

    #[test]
    fn event_mask_covers_keys_mouse_and_scroll() {
        let mask = event_mask();
        for t in [
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::MouseMoved,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
        ] {
            assert_ne!(mask & (1u64 << t.0), 0, "{t:?}");
        }
    }

    #[test]
    fn locked_flag_parsing() {
        assert!(locked_from_flag(Some(true)));
        assert!(!locked_from_flag(Some(false)));
        assert!(!locked_from_flag(None));
    }
}
