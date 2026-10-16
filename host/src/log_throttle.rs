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
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

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
}

impl<S: Subscriber> Layer<S> for LogThrottle {
    fn event_enabled(&self, event: &Event<'_>, _ctx: Context<'_, S>) -> bool {
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
}
