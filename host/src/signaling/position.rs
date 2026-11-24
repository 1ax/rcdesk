//! Положение курсора хоста для режима просмотра (слайс VM-2): нормализация
//! в долю транслируемого дисплея и решение «слать ли». Чистая логика без
//! платформы и сети; опрос и отправка -- в `start::spawn_position_task`.

use crate::input::CaptureRect;

/// Как часто опрашивается положение указателя в режиме просмотра (~30 Гц).
pub(super) const CURSOR_POSITION_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(33);

/// Обратная операция к `input::to_pixel`: глобальная точка -> доля `rect`.
/// Не зажимается: вне `[0, 1]` курсор на другом мониторе (клиент скроет).
/// Дисплей с неположительным размером даёт `(0.0, 0.0)`, а не `NaN`/`inf`.
pub(super) fn normalize(pos: (i32, i32), rect: CaptureRect) -> (f64, f64) {
    if rect.width <= 0 || rect.height <= 0 {
        return (0.0, 0.0);
    }
    (
        f64::from(pos.0 - rect.x) / f64::from(rect.width),
        f64::from(pos.1 - rect.y) / f64::from(rect.height),
    )
}

/// Помнит последнее отправленное нормализованное положение: шлём при
/// изменении, а при `fresh` (вход в просмотр) -- безусловно.
#[derive(Debug, Default)]
pub(super) struct PositionGate {
    last: Option<(f64, f64)>,
}

impl PositionGate {
    /// `Some` -- положение, которое надо отправить.
    pub(super) fn update(
        &mut self,
        pos: (i32, i32),
        rect: CaptureRect,
        fresh: bool,
    ) -> Option<(f64, f64)> {
        let norm = normalize(pos, rect);
        if !fresh && self.last == Some(norm) {
            return None;
        }
        self.last = Some(norm);
        Some(norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> CaptureRect {
        CaptureRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn normalizes_inside_the_display() {
        assert_eq!(normalize((480, 270), rect(0, 0, 1920, 1080)), (0.25, 0.25));
        assert_eq!(normalize((0, 0), rect(0, 0, 1920, 1080)), (0.0, 0.0));
    }

    #[test]
    fn normalizes_relative_to_a_non_zero_origin() {
        // Второй монитор справа от главного, левый верхний угол в (1920, 0).
        assert_eq!(
            normalize((2400, 540), rect(1920, 0, 1920, 1080)),
            (0.25, 0.5)
        );
        // Монитор слева (отрицательное начало).
        assert_eq!(
            normalize((-960, 540), rect(-1920, 0, 1920, 1080)),
            (0.5, 0.5)
        );
    }

    #[test]
    fn point_outside_the_display_is_not_clamped() {
        let r = rect(0, 0, 1000, 500);
        assert_eq!(normalize((1500, 250), r), (1.5, 0.5));
        assert_eq!(normalize((-100, -250), r), (-0.1, -0.5));
    }

    #[test]
    fn degenerate_display_does_not_produce_nan() {
        assert_eq!(normalize((10, 10), rect(0, 0, 0, 0)), (0.0, 0.0));
    }

    #[test]
    fn gate_sends_only_changes() {
        let r = rect(0, 0, 100, 100);
        let mut gate = PositionGate::default();
        assert_eq!(gate.update((50, 50), r, false), Some((0.5, 0.5)));
        assert_eq!(gate.update((50, 50), r, false), None);
        assert_eq!(gate.update((60, 50), r, false), Some((0.6, 0.5)));
    }

    #[test]
    fn gate_forces_the_first_position_on_entering_view_mode() {
        let r = rect(0, 0, 100, 100);
        let mut gate = PositionGate::default();
        assert!(gate.update((50, 50), r, true).is_some());
        assert_eq!(gate.update((50, 50), r, false), None);
        // Повторный вход в просмотр: то же положение, но шлём снова.
        assert_eq!(gate.update((50, 50), r, true), Some((0.5, 0.5)));
    }

    #[test]
    fn gate_resends_when_the_display_changes_under_a_still_pointer() {
        let mut gate = PositionGate::default();
        assert!(gate.update((50, 50), rect(0, 0, 100, 100), false).is_some());
        assert!(gate.update((50, 50), rect(0, 0, 200, 100), false).is_some());
    }
}
