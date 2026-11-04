//! Дешёвый опрос геометрии дисплеев для V-2 (смена разрешения на ходу).
//!
//! Почему не `scap::get_all_targets` (он же `capture::list_displays`): это
//! `SCShareableContent::current()` с перечислением ВСЕХ окон, десятки-сотни
//! миллисекунд за вызов, и без autorelease-пула на потоках blocking-пула это
//! копит объекты в агенте, живущем неделями. Здесь -- только CoreGraphics:
//! `CGGetActiveDisplayList` + `CGDisplayBounds`, без прав на запись экрана.
//!
//! Единицы те же, что у `capture::list_displays` (там `raw_handle.bounds()` --
//! тот же `CGDisplayBounds`): точки глобального пространства координат.
//! `id` -- `CGDirectDisplayID`, как у `scap`-дисплея.

use objc2_core_graphics::{CGDisplayBounds, CGError, CGGetActiveDisplayList, CGMainDisplayID};

use crate::capture::DisplayInfo;

/// Верхняя граница числа дисплеев в одном запросе.
const MAX_DISPLAYS: usize = 32;

/// Геометрия всех активных дисплеев. `title` пустой: для сравнения
/// геометрии название не нужно. Ошибка CG -- пустой список.
pub fn geometries() -> Vec<DisplayInfo> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // Safety: `ids` вмещает MAX_DISPLAYS элементов, `count` -- валидный
    // указатель на u32.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        tracing::debug!(code = err.0, "CGGetActiveDisplayList failed");
        return Vec::new();
    }
    let main = CGMainDisplayID();
    ids.iter()
        .take((count as usize).min(MAX_DISPLAYS))
        .map(|&id| {
            let b = CGDisplayBounds(id);
            DisplayInfo {
                id,
                title: String::new(),
                x: b.origin.x as i32,
                y: b.origin.y as i32,
                width: b.size.width as u32,
                height: b.size.height as u32,
                primary: id == main,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Без дисплея (CI) список может быть пустым; главное -- без паники.
    #[test]
    fn geometries_does_not_panic() {
        let list = geometries();
        for d in &list {
            assert!(d.width > 0 && d.height > 0);
        }
    }
}
