//! Переключение раскладки клавиатуры хоста (слайс 4.6c). Клиент с Windows не
//! может переключить раскладку Mac хоткеем (Alt+Shift доходит как
//! Option+Shift), поэтому он шлёт `SwitchInputSource`, а хост переключает
//! источник ввода системным API.
//!
//! Модуль платформенно-независим: только трейт. Реализация (macOS:
//! Text Input Sources) живёт в `platform::macos::input_source` и
//! подставляется в `HostContext::input_source`; иначе везде `None`.

/// Переключатель раскладки хоста. См. описание модуля.
pub trait InputSourceSwitcher: Send + Sync {
    /// Выбирает следующую раскладку циклически. Если раскладка одна --
    /// `Ok` без действия.
    fn select_next(&self) -> anyhow::Result<()>;
    /// Короткая метка текущей раскладки (`"RU"`, `"EN"`), `None`, если её
    /// определить не удалось.
    fn current_label(&self) -> Option<String>;
}
