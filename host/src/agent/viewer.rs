//! Просмотрщик PIN (D83b): чистая логика «прочитанный `status.json` помощника
//! службы -> что показать». Сам значок и опрос файла -- в `agent_main`
//! (Windows); здесь только то, что тестируется без трея.

use crate::service::HelperStatus;
use crate::signaling::AgentStatus;

/// Статус для значка: нет файла, нет права чтения, битый JSON или состояние
/// без PIN -- `Connecting` (то есть «PIN нет»).
pub fn viewer_status(read: Option<HelperStatus>) -> AgentStatus {
    read.and_then(|status| status.to_agent())
        .unwrap_or(AgentStatus::Connecting)
}

/// Пропускает только смены статуса: первое наблюдение всегда считается сменой.
#[derive(Debug, Default)]
pub struct StatusDedup {
    last: Option<AgentStatus>,
}

impl StatusDedup {
    /// `Some(status)`, если он отличается от предыдущего наблюдения.
    pub fn observe(&mut self, status: AgentStatus) -> Option<AgentStatus> {
        if self.last.as_ref() == Some(&status) {
            return None;
        }
        self.last = Some(status.clone());
        Some(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{read_status, STATUS_FILE};

    fn registered(pin: &str) -> AgentStatus {
        AgentStatus::Registered { pin: pin.into() }
    }

    #[test]
    fn missing_status_shows_connecting() {
        assert_eq!(viewer_status(None), AgentStatus::Connecting);
    }

    #[test]
    fn broken_or_unreadable_file_shows_connecting() {
        let dir = std::env::temp_dir().join(format!("rcdesk-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            viewer_status(read_status(&dir)),
            AgentStatus::Connecting,
            "файла нет"
        );
        std::fs::write(dir.join(STATUS_FILE), "{broken").unwrap();
        assert_eq!(viewer_status(read_status(&dir)), AgentStatus::Connecting);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_with_pin_is_shown() {
        let dir = std::env::temp_dir().join(format!("rcdesk-viewer-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(STATUS_FILE),
            r#"{"state":"in_session","pin":"123456"}"#,
        )
        .unwrap();
        assert_eq!(
            viewer_status(read_status(&dir)),
            AgentStatus::InSession {
                pin: "123456".into()
            }
        );
        std::fs::write(
            dir.join(STATUS_FILE),
            r#"{"state":"registered","pin":null}"#,
        )
        .unwrap();
        assert_eq!(viewer_status(read_status(&dir)), AgentStatus::Connecting);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_passes_only_changes() {
        let mut dedup = StatusDedup::default();
        assert_eq!(
            dedup.observe(AgentStatus::Connecting),
            Some(AgentStatus::Connecting)
        );
        assert_eq!(dedup.observe(AgentStatus::Connecting), None);
        assert_eq!(
            dedup.observe(registered("111111")),
            Some(registered("111111"))
        );
        assert_eq!(dedup.observe(registered("111111")), None);
        assert_eq!(
            dedup.observe(registered("222222")),
            Some(registered("222222"))
        );
        assert_eq!(
            dedup.observe(AgentStatus::Connecting),
            Some(AgentStatus::Connecting)
        );
    }
}
