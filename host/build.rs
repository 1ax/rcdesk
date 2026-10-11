//! Номер сборки для `build_info`: первые 7 символов `GITHUB_SHA` (в CI) или
//! `dev` (локально). Git не дёргаем. Для Windows-цели дополнительно вшивает
//! в exe иконку и сведения о версии (4.3b).
fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let build = std::env::var("GITHUB_SHA")
        .ok()
        .map(|sha| sha.trim().chars().take(7).collect::<String>())
        .filter(|short| !short.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=RCDESK_BUILD={build}");
    println!("cargo:rerun-if-changed=../installer/windows/rcdesk.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// Иконка и VERSIONINFO для обоих exe пакета (`rcdesk-host`, `rcdesk-agent`).
/// build-скрипт собирается под платформу сборки, поэтому вызов `winresource`
/// -- под `cfg(windows)`: на Mac крейта нет в графе вовсе. Кросс-сборки Windows
/// с не-Windows хоста не поддерживаются (openh264 из исходников под MSVC).
#[cfg(windows)]
fn embed_windows_resources() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION задаёт cargo");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../installer/windows/rcdesk.ico")
        .set("ProductName", "rcdesk")
        .set("FileDescription", "rcdesk")
        .set("FileVersion", &version)
        .set("ProductVersion", &version);
    // Без ресурса exe всё равно работает, но без иконки в проводнике и
    // установщике -- падаем громко, чтобы CI это заметил.
    res.compile()
        .expect("не удалось вшить ресурсы Windows (иконка/версия)");
}

#[cfg(not(windows))]
fn embed_windows_resources() {}
