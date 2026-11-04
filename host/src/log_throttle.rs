//! Подавление повторов шума `Failed to write packet ... (os error 10051)` из
//! webrtc (крейт `log`, уровень error): ICE шлёт пакеты с виртуальных
//! адаптеров (Hyper-V/WSL), у которых нет маршрута. Пакеты безвредны, а строка
//! повторяется много раз в секунду. Слой фильтрует ТОЛЬКО лог, по тексту
//! сообщения (события из `log` приходят через мост tracing-log с target `log`,
//! так что `metadata().target()` не годится).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Минимальный интервал между событиями одного места в коде (`file:line`)
/// для шумного target `webrtc::peer_connection::driver`.
pub const THROTTLE_INTERVAL: Duration = Duration::from_secs(5);
const DRIVER_TARGET: &str = "webrtc::peer_connection::driver";
const MAX_CALLSITES: usize = 256;

/// Живая проверка 0.4.2: всплеск сообщений data channel от клиента дал 1180
/// строк ERROR `Failed to send DataChannelMessage ...` за 20 мс (webrtc
/// `driver.rs` пишет error на КАЖДОЕ отброшенное сообщение). Глушить target
/// целиком нельзя (там ещё ~40 полезных error/warn), поэтому режем частоту
/// per-callsite: не чаще одного события в `THROTTLE_INTERVAL`.
#[derive(Debug)]
pub struct CallsiteThrottle {
    last: Mutex<HashMap<String, Instant>>,
    interval: Duration,
}

impl CallsiteThrottle {
    pub fn new(interval: Duration) -> Self {
        Self {
            last: Mutex::new(HashMap::new()),
            interval,
        }
    }

    /// `true` -- событие с ключом `key` пропустить, `false` -- подавить.
    pub fn allow(&self, key: &str, now: Instant) -> bool {
        let Ok(mut last) = self.last.lock() else {
            return true;
        };
        if let Some(prev) = last.get_mut(key) {
            if now.saturating_duration_since(*prev) < self.interval {
                return false;
            }
            *prev = now;
            return true;
        }
        if last.len() >= MAX_CALLSITES {
            last.clear();
        }
        last.insert(key.to_owned(), now);
        true
    }
}

impl Default for CallsiteThrottle {
    fn default() -> Self {
        Self::new(THROTTLE_INTERVAL)
    }
}

/// Поля события: настоящие (`message`) и перенесённые мостом tracing-log
/// (`log.target`/`log.file`/`log.line`) -- webrtc пишет через крейт `log`,
/// у таких событий `metadata().target()` равен `log`, а callsite один на
/// уровень, поэтому место определяем по `log.file:log.line`.
#[derive(Default)]
struct SourceVisitor {
    target: Option<String>,
    file: Option<String>,
    line: Option<u64>,
}

impl Visit for SourceVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "log.target" => self.target = Some(value.to_owned()),
            "log.file" => self.file = Some(value.to_owned()),
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "log.line" {
            self.line = Some(value);
        }
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

const PREFIX: &str = "Failed to write packet to ";
const WINDOW: Duration = Duration::from_secs(300);
const MAX_KEYS: usize = 64;
/// Ошибки класса «нет маршрута» (Windows, macOS, Linux).
const ROUTE_ERRORS: [&str; 6] = [
    "(os error 10051)",
    "(os error 10065)",
    "(os error 51)",
    "(os error 65)",
    "(os error 101)",
    "(os error 113)",
];

/// Если `message` -- шум «нет маршрута», возвращает локальный адрес (ключ).
pub fn parse_noise(message: &str) -> Option<&str> {
    let rest = message.strip_prefix(PREFIX)?;
    let from = rest.rfind(" from ")?;
    let after = &rest[from + " from ".len()..];
    let colon = after.find(": ")?;
    let key = &after[..colon];
    let tail = &after[colon + 2..];
    if ROUTE_ERRORS.iter().any(|e| tail.contains(e)) {
        Some(key)
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Пропустить; `suppressed` -- сколько подавлено с прошлого пропуска.
    Pass {
        suppressed: u64,
    },
    Suppress,
}

#[derive(Default)]
pub struct Throttle {
    keys: HashMap<String, (Instant, u64)>,
}

impl Throttle {
    pub fn check(&mut self, key: &str, now: Instant) -> Decision {
        if let Some((last, count)) = self.keys.get_mut(key) {
            if now.saturating_duration_since(*last) < WINDOW {
                *count += 1;
                return Decision::Suppress;
            }
            let suppressed = *count;
            *last = now;
            *count = 0;
            return Decision::Pass { suppressed };
        }
        if self.keys.len() >= MAX_KEYS {
            self.keys.clear();
        }
        self.keys.insert(key.to_owned(), (now, 0));
        Decision::Pass { suppressed: 0 }
    }
}

#[derive(Default)]
struct MessageVisitor(Option<String>);

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = Some(value.to_owned());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && self.0.is_none() {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

/// tracing-слой: подавляет повторы шума, раз в окно пишет строку-итог.
#[derive(Default)]
pub struct LogThrottle {
    state: Mutex<Throttle>,
    callsites: CallsiteThrottle,
}

impl LogThrottle {
    /// `false`, если событие -- повтор из шумного места webrtc-драйвера.
    fn driver_allowed(&self, event: &Event<'_>) -> bool {
        let meta = event.metadata();
        // Ниже WARN этот target не троттлим (его и так режет фильтр).
        if *meta.level() > Level::WARN {
            return true;
        }
        let from_log = meta.target() == "log";
        if !from_log && !meta.target().starts_with(DRIVER_TARGET) {
            return true;
        }
        let mut src = SourceVisitor::default();
        event.record(&mut src);
        let key = if from_log {
            match (&src.target, &src.file, src.line) {
                (Some(t), Some(f), Some(l)) if t.starts_with(DRIVER_TARGET) => {
                    format!("{f}:{l}")
                }
                _ => return true,
            }
        } else {
            format!(
                "{}:{}",
                meta.file().unwrap_or("?"),
                meta.line().unwrap_or(0)
            )
        };
        self.callsites.allow(&key, Instant::now())
    }
}

impl<S: Subscriber> Layer<S> for LogThrottle {
    fn event_enabled(&self, event: &Event<'_>, _ctx: Context<'_, S>) -> bool {
        if !self.driver_allowed(event) {
            return false;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let Some(message) = visitor.0 else {
            return true;
        };
        let Some(key) = parse_noise(&message) else {
            return true;
        };
        // Блокировка отпускается до эмиссии итога (иначе риск самоблокировки).
        let decision = match self.state.lock() {
            Ok(mut state) => state.check(key, Instant::now()),
            Err(_) => return true,
        };
        match decision {
            Decision::Suppress => false,
            Decision::Pass { suppressed } => {
                if suppressed > 0 {
                    // Вложенное событие доходит только при глобальном
                    // подписчике (`init()`, как в агенте и CLI): при scoped
                    // (`with_default`) tracing-core внутри диспетчеризации
                    // отдаёт `Dispatch::none`, и итог теряется.
                    tracing::info!(
                        "suppressed repeated \"Failed to write packet\" errors local={key} suppressed={suppressed} window_s={}",
                        WINDOW.as_secs()
                    );
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Arc;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt;

    const WIN: &str = "Failed to write packet to 1.2.3.4:3478 from 172.23.240.1:55000: A socket operation was attempted to an unreachable network. (os error 10051)";

    #[test]
    fn recognizes_windows_and_macos() {
        assert_eq!(parse_noise(WIN), Some("172.23.240.1:55000"));
        let mac = "Failed to write packet to 1.2.3.4:3478 from 10.0.0.5:4000: No route to host (os error 51)";
        assert_eq!(parse_noise(mac), Some("10.0.0.5:4000"));
    }

    #[test]
    fn rejects_others() {
        let other =
            "Failed to write packet to 1.2.3.4:1 from 10.0.0.5:4000: reset (os error 10054)";
        assert_eq!(parse_noise(other), None);
        let near = "Failed to write packet to 1.2.3.4:1 from 10.0.0.5:4000: x (os error 510)";
        assert_eq!(parse_noise(near), None);
        assert_eq!(parse_noise("something else (os error 10051)"), None);
    }

    #[test]
    fn policy_window_and_counts() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        assert_eq!(t.check("a", t0), Decision::Pass { suppressed: 0 });
        for s in 1..=3 {
            assert_eq!(
                t.check("a", t0 + Duration::from_secs(s)),
                Decision::Suppress
            );
        }
        assert_eq!(
            t.check("a", t0 + Duration::from_secs(301)),
            Decision::Pass { suppressed: 3 }
        );
        assert_eq!(
            t.check("a", t0 + Duration::from_secs(302)),
            Decision::Suppress
        );
    }

    #[test]
    fn keys_independent_and_overflow_ok() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        assert_eq!(t.check("a", t0), Decision::Pass { suppressed: 0 });
        assert_eq!(t.check("b", t0), Decision::Pass { suppressed: 0 });
        assert_eq!(t.check("a", t0), Decision::Suppress);
        for i in 0..(MAX_KEYS * 3) {
            let _ = t.check(&format!("k{i}"), t0);
        }
        assert!(t.keys.len() <= MAX_KEYS);
    }

    #[test]
    fn callsite_allow_policy() {
        let t = CallsiteThrottle::default();
        let t0 = Instant::now();
        assert!(t.allow("a:1", t0));
        assert!(!t.allow("a:1", t0 + Duration::from_secs(1)));
        assert!(!t.allow("a:1", t0 + Duration::from_millis(4999)));
        assert!(t.allow("b:2", t0 + Duration::from_secs(1)));
        assert!(t.allow("a:1", t0 + Duration::from_secs(5)));
        assert!(!t.allow("a:1", t0 + Duration::from_secs(6)));
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[test]
    fn integration_with_real_subscriber() {
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish()
            .with(LogThrottle::default());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                // target "log" -- как у событий из крейта `log` через tracing-log.
                tracing::error!(target: "log", "{WIN}");
            }
            for i in 0..5 {
                tracing::error!("unrelated failure {i}");
            }
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("os error 10051").count(), 1, "{out}");
        assert_eq!(out.matches("unrelated failure").count(), 5, "{out}");
    }

    #[test]
    fn driver_burst_is_throttled_others_not() {
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish()
            .with(LogThrottle::default());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                tracing::error!(target: "webrtc::peer_connection::driver", "driver burst");
            }
            for _ in 0..100 {
                tracing::event!(target: "log", tracing::Level::ERROR,
                    log.target = "webrtc::peer_connection::driver",
                    log.file = "driver.rs", log.line = 1215u64,
                    "bridged burst");
            }
            for _ in 0..5 {
                tracing::error!(target: "other", "other error");
            }
            for _ in 0..5 {
                tracing::event!(target: "log", tracing::Level::ERROR, log.target = "rtc_ice", log.file = "x.rs",
                    log.line = 1u64, "foreign log");
            }
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("driver burst").count(), 1, "{out}");
        assert_eq!(out.matches("bridged burst").count(), 1, "{out}");
        assert_eq!(out.matches("other error").count(), 5, "{out}");
        assert_eq!(out.matches("foreign log").count(), 5, "{out}");
    }
}
