//! Номер сборки для `build_info`: первые 7 символов `GITHUB_SHA` (в CI) или
//! `dev` (локально). Git не дёргаем.
fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let build = std::env::var("GITHUB_SHA")
        .ok()
        .map(|sha| sha.trim().chars().take(7).collect::<String>())
        .filter(|short| !short.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=RCDESK_BUILD={build}");
}
