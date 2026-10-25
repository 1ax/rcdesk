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
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        embed_cg_prelogin_section();
    }
    embed_web_client();
}

/// Вшивание собранного веб-клиента (`web/dist`) для LAN-хаба (D47-3): в
/// `OUT_DIR/web_files.rs` попадает `static WEB_FILES: &[(&str, &[u8])]` с
/// `include_bytes!` на каждый файл (пути относительные к dist, через `/`,
/// порядок отсортирован). Нет dist -- пустой список и предупреждение: хаб
/// отдаёт страницу-заглушку. `RCDESK_REQUIRE_WEB=1` (релизные сборки) делает
/// отсутствие dist ошибкой сборки.
fn embed_web_client() {
    println!("cargo:rerun-if-env-changed=RCDESK_REQUIRE_WEB");
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR задаёт cargo");
    let dist = std::path::Path::new(&manifest_dir).join("../web/dist");
    // Каталог слежения: появление dist пересобирает хост.
    println!("cargo:rerun-if-changed=../web/dist");

    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    if dist.is_dir() {
        collect_web_files(&dist, "", &mut files);
    }
    files.sort();

    let require = std::env::var("RCDESK_REQUIRE_WEB").as_deref() == Ok("1");
    if files.is_empty() {
        assert!(
            !require,
            "RCDESK_REQUIRE_WEB=1, но web/dist нет или он пуст: соберите веб-клиент (cd web && npm ci && npm run build)"
        );
        println!("cargo:warning=web/dist not built; hub serves a placeholder");
    }

    let mut code = String::from("static WEB_FILES: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in &files {
        println!("cargo:rerun-if-changed={}", abs.display());
        // {:?} экранирует обратные слэши Windows-путей.
        code.push_str(&format!(
            "    ({rel:?}, include_bytes!({:?})),\n",
            abs.to_string_lossy()
        ));
    }
    code.push_str("];\n");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR задаёт cargo");
    std::fs::write(std::path::Path::new(&out_dir).join("web_files.rs"), code)
        .expect("не удалось записать web_files.rs");
}

/// Рекурсивный обход `dir`; `prefix` -- относительный путь с `/`. Скрытые
/// файлы (`.DS_Store` и т.п.) пропускаются.
fn collect_web_files(
    dir: &std::path::Path,
    prefix: &str,
    out: &mut Vec<(String, std::path::PathBuf)>,
) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let entries = std::fs::read_dir(dir).expect("не удалось прочитать web/dist");
    for entry in entries {
        let entry = entry.expect("не удалось прочитать запись web/dist");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            collect_web_files(&path, &rel, out);
        } else {
            out.push((rel, path));
        }
    }
}

/// Секция `__CGPreLoginApp,__cgpreloginapp` в обоих бинарниках пакета: без неё
/// CoreGraphics не принимает события от процесса в сессии LoginWindow (экран
/// входа macOS). Содержимое не важно -- достаточно наличия секции (один байт).
fn embed_cg_prelogin_section() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR задаёт cargo");
    let path = std::path::Path::new(&out_dir).join("cgpreloginapp.bin");
    std::fs::write(&path, [0u8]).expect("не удалось записать заполнитель cgpreloginapp");
    let path = path.to_string_lossy();
    // -Wl,a,b,c передаёт ld аргументы через запятую, путь с запятой её сломает.
    assert!(!path.contains(','), "OUT_DIR содержит запятую: {path}");
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__CGPreLoginApp,__cgpreloginapp,{path}");
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
