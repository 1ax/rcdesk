//! Переключение раскладки на macOS (слайс 4.6c) через Text Input Sources
//! (HIToolbox, фреймворк Carbon). Реализует `crate::input_source::InputSourceSwitcher`.
//!
//! API потокобезопасен для нашего использования: спайк показал, что
//! `TISSelectInputSource` работает и из фонового потока процесса без окна,
//! поэтому маршалинга на главный run loop нет.
//!
//! Владение CF-объектами: `TISCreate*`/`TISCopy*` возвращают +1 (оборачиваем в
//! `CFRetained`, он отпускает сам), `TISGetInputSourceProperty` -- без
//! передачи владения (не отпускаем, живёт пока жив источник).

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{anyhow, Context};
use objc2_core_foundation::{kCFBooleanTrue, CFArray, CFDictionary, CFRetained, CFString, CFType};

use crate::input_source::InputSourceSwitcher;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyInputSourceCategory: &'static CFString;
    static kTISCategoryKeyboardInputSource: &'static CFString;
    static kTISPropertyInputSourceIsSelectCapable: &'static CFString;
    static kTISPropertyInputSourceID: &'static CFString;
    static kTISPropertyInputSourceLanguages: &'static CFString;

    /// Create rule: результат (+1) отпускает вызывающий. `include_all_installed`
    /// -- `Boolean` (u8).
    fn TISCreateInputSourceList(
        properties: Option<&CFDictionary<CFString, CFType>>,
        include_all_installed: u8,
    ) -> *mut CFArray<CFType>;
    /// Copy rule (+1).
    fn TISCopyCurrentKeyboardInputSource() -> *mut CFType;
    /// Get rule: возвращаемое значение не наше, не отпускать.
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *const c_void;
    /// `OSStatus`, 0 -- успех.
    fn TISSelectInputSource(source: &CFType) -> i32;
}

/// Реальный переключатель раскладки macOS.
pub struct MacInputSourceSwitcher;

impl MacInputSourceSwitcher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacInputSourceSwitcher {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSourceSwitcher for MacInputSourceSwitcher {
    fn select_next(&self) -> anyhow::Result<()> {
        select_next()
    }

    fn current_label(&self) -> Option<String> {
        current_label()
    }
}

/// ID источника ввода (`com.apple.keylayout.US`).
fn source_id(source: &CFType) -> Option<String> {
    // Safety: `source` -- валидный TISInputSourceRef, ключ -- статик системы;
    // значение kTISPropertyInputSourceID -- CFString, владеет им источник.
    let ptr = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
    if ptr.is_null() {
        return None;
    }
    // Safety: ptr непустой и указывает на CFString (контракт ключа), живущий
    // не меньше `source`; берём ссылку на время этого вызова.
    let s = unsafe { &*(ptr as *const CFString) };
    Some(s.to_string())
}

/// Включённые клавиатурные источники, которые можно выбрать.
fn selectable_keyboard_sources() -> anyhow::Result<CFRetained<CFArray<CFType>>> {
    // Safety: чтение extern-статиков CFStringRef, инициализированных системой.
    let (cat_key, cat_val, cap_key) = unsafe {
        (
            kTISPropertyInputSourceCategory,
            kTISCategoryKeyboardInputSource,
            kTISPropertyInputSourceIsSelectCapable,
        )
    };
    // Safety: kCFBooleanTrue -- системная константа.
    let yes = unsafe { kCFBooleanTrue }.ok_or_else(|| anyhow!("kCFBooleanTrue недоступен"))?;
    let keys: [&CFString; 2] = [cat_key, cap_key];
    let values: [&CFType; 2] = [cat_val.as_ref(), yes.as_ref()];
    let filter = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // Safety: filter -- валидный словарь; includeAll=false (только включённые).
    let raw = unsafe { TISCreateInputSourceList(Some(&filter), 0) };
    let ptr = NonNull::new(raw).ok_or_else(|| anyhow!("TISCreateInputSourceList вернул NULL"))?;
    // Safety: Create rule -- владение +1 переходит к CFRetained.
    Ok(unsafe { CFRetained::from_raw(ptr) })
}

fn copy_current() -> Option<CFRetained<CFType>> {
    // Safety: Copy rule, +1; NULL проверяется.
    let raw = unsafe { TISCopyCurrentKeyboardInputSource() };
    // Safety: ненулевой указатель с +1 передаём CFRetained.
    NonNull::new(raw).map(|p| unsafe { CFRetained::from_raw(p) })
}

/// Выбирает следующую включённую раскладку циклически.
pub fn select_next() -> anyhow::Result<()> {
    let list = selectable_keyboard_sources()?;
    if list.len() < 2 {
        return Ok(());
    }
    let current = copy_current().context("не удалось получить текущую раскладку")?;
    let current_id = source_id(&current);
    let pos = current_id.as_ref().and_then(|cur| {
        (0..list.len()).find(|&i| {
            list.get(i)
                .and_then(|s| source_id(&s))
                .is_some_and(|id| &id == cur)
        })
    });
    let next_index = pos.map_or(0, |i| (i + 1) % list.len());
    let next = list
        .get(next_index)
        .context("раскладка пропала из списка")?;
    // Safety: next -- валидный источник из списка, удерживаемый CFRetained.
    let status = unsafe { TISSelectInputSource(&next) };
    if status != 0 {
        return Err(anyhow!("TISSelectInputSource вернул OSStatus {status}"));
    }
    Ok(())
}

/// Метка из кода языка: `"ru"` -> `"RU"`, `"zh-Hans"` -> `"ZH"`.
fn label_from_language(lang: &str) -> Option<String> {
    let primary = lang.split(['-', '_']).next().unwrap_or("");
    let label: String = primary
        .chars()
        .take(3)
        .flat_map(char::to_uppercase)
        .collect();
    (!label.is_empty()).then_some(label)
}

/// Метка текущей раскладки по первому языку источника.
pub fn current_label() -> Option<String> {
    let current = copy_current()?;
    // Safety: current валиден, ключ -- системный статик.
    let ptr = unsafe { TISGetInputSourceProperty(&current, kTISPropertyInputSourceLanguages) };
    if ptr.is_null() {
        return None;
    }
    // Safety: значение ключа Languages -- CFArray<CFString>, владеет им
    // источник (живёт, пока жив `current`); берём ссылку на время вызова.
    let langs = unsafe { &*(ptr as *const CFArray<CFString>) };
    let first = langs.get(0)?;
    label_from_language(&first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_uppercased_primary_language_subtag() {
        assert_eq!(label_from_language("ru").as_deref(), Some("RU"));
        assert_eq!(label_from_language("en").as_deref(), Some("EN"));
        assert_eq!(label_from_language("zh-Hans").as_deref(), Some("ZH"));
        assert_eq!(label_from_language("haw").as_deref(), Some("HAW"));
        assert_eq!(label_from_language("").as_deref(), None);
    }
}
