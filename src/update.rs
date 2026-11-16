//! Обновления через релизы GitHub, как у оригинала: проверка раз в полчаса, тост
//! «Доступна версия …» с кнопкой «Обновить».
//!
//! В релизе у каждой системы свой пакет: `yamusic-rs_<версия>_<os>_<arch>.zip`. На Windows
//! и macOS кнопка скачивает свой zip, сверяет SHA-256 с тем, что показывает GitHub,
//! распаковывает и подменяет запущенное: exe на Windows, бандл `.app` на macOS. Обе системы
//! не дают перезаписать работающий файл, но дают его переименовать, поэтому новый становится
//! на место старого, а старый (`*.old.exe` / `*.old.app`) удаляется при следующем запуске.
//! На Linux кнопка всегда открывает страницу релиза: пакет ставит пакетный менеджер.
//! Если папка недоступна для записи, кнопка тоже открывает страницу релиза.

#[cfg(any(windows, target_os = "macos"))]
use sha2::{Digest, Sha256};
#[cfg(any(windows, target_os = "macos"))]
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Как часто спрашивать GitHub. У оригинала UPDATE_POLL_INTERVAL_MS — те же 30 минут.
pub const POLL: Duration = Duration::from_secs(30 * 60);

/// Аргумент, с которым обновлённое приложение ждёт, пока закроется старое.
pub const AFTER_UPDATE: &str = "--after-update";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Страница релиза на GitHub.
    pub page: String,
    /// Zip-пакет: адрес, размер и SHA-256. Нет — обновить можно только руками.
    pub zip: Option<(String, u64, String)>,
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        // GitHub API без User-Agent отвечает 403.
        .user_agent(concat!("yamusic-rs/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// «v1.2.3» / «1.2.3» → (1, 2, 3). Пре-релизы (`/releases/latest` их и так не отдаёт) не нужны.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u32>());
    Some((it.next()?.ok()?, it.next()?.ok()?, it.next().unwrap_or(Ok(0)).ok()?))
}

/// Имя пакета в релизе под сочетание ОС и архитектуры. `None` — самообновление
/// не поддерживается (Linux, необычная архитектура): кнопка откроет страницу релиза.
fn asset_name_for(os: &str, arch: &str, version: &str) -> Option<String> {
    let os = match os {
        "windows" => "windows",
        "macos" => "macos",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("{}_{}_{}_{}.zip", env!("CARGO_PKG_NAME"), version, os, arch))
}

/// Имя пакета этой системы в релизе `<версия>`.
fn asset_name(version: &str) -> Option<String> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH, version)
}

/// Последний релиз, если он новее запущенной версии.
pub fn check() -> Result<Option<Release>, String> {
    let api = env!("CARGO_PKG_REPOSITORY").replace("https://github.com/", "https://api.github.com/repos/");
    let resp = client()?.get(format!("{api}/releases/latest")).send().map_err(|e| crate::config::chain(&e))?;
    // 404 — релизов ещё нет.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub: HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).ok_or("в релизе нет версии")?;
    let (Some(latest), Some(current)) = (parse_version(tag), parse_version(env!("CARGO_PKG_VERSION"))) else {
        return Err(format!("непонятная версия {tag}"));
    };
    if latest <= current {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v');
    let zip = asset_name(version).and_then(|name| {
        let assets = v.get("assets")?.as_array()?;
        let a = assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))?;
        let url = a.get("browser_download_url")?.as_str()?.to_string();
        let size = a.get("size")?.as_u64()?;
        let sha = a.get("digest")?.as_str()?.strip_prefix("sha256:")?.to_ascii_lowercase();
        Some((url, size, sha))
    });
    Ok(Some(Release {
        version: version.to_string(),
        page: v.get("html_url").and_then(|u| u.as_str()).unwrap_or(env!("CARGO_PKG_REPOSITORY")).to_string(),
        zip,
    }))
}

/// Скачать zip обновления во временную папку и сверить размер и SHA-256.
/// Качается и хешируется потоково, чанками: весь пакет в памяти не оказывается.
#[cfg(any(windows, target_os = "macos"))]
fn download(r: &Release) -> Result<PathBuf, String> {
    let (url, size, sha) = r.zip.as_ref().ok_or("у релиза нет пакета для этой системы")?;
    let zip = std::env::temp_dir().join(format!("{}-update-{}.zip", env!("CARGO_PKG_NAME"), r.version));
    let mut resp = client()?
        .get(url)
        .timeout(Duration::from_secs(600))
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("загрузка: {}", crate::config::chain(&e)))?;
    {
        let mut f = std::fs::File::create(&zip).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp, &mut f).map_err(|e| format!("загрузка: {e}"))?;
    }
    let mut f = std::fs::File::open(&zip).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    if len != *size {
        return Err("файл скачался не целиком".into());
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if &got != sha {
        return Err("контрольная сумма не совпала".into());
    }
    Ok(zip)
}

/// Скачать пакет, проверить и поставить на место запущенного. Долго: звать из фонового
/// потока. После успеха нужно запустить [`relaunch`] и закрыться.
pub fn install(r: &Release) -> Result<PathBuf, String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let zip = download(r)?;
        place(&zip)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = r;
        Err("на Linux пакет ставит пакетный менеджер".into())
    }
}

/// Путь для распаковки с защитой от выхода за пределы `dir` (записи вроде `../`,
/// абсолютные имена). `None` — запись распаковывать нельзя.
#[cfg(any(windows, target_os = "macos"))]
fn safe_entry_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(name);
    let rest = path.strip_prefix(dir).ok()?;
    if rest.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    Some(path)
}

/// Распаковать zip в `dir` с правами Unix из архива. Возвращает число файлов.
#[cfg(any(windows, target_os = "macos"))]
fn unzip(zip: &Path, dir: &Path) -> Result<usize, String> {
    let file = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("архив: {e}"))?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("архив: {e}"))?;
        let Some(name) = entry.name().ok().map(|n| n.into_owned()) else { continue };
        // Метаданные AppleDouble (._файл, __MACOSX/) — мусор при распаковке на macOS:
        // расширенные атрибуты обновлённому бандлу не нужны.
        let file = name.rsplit(['/', '\\']).next().unwrap_or(&name);
        if file.starts_with("._") || name.starts_with("__MACOSX/") {
            continue;
        }
        let Some(path) = safe_entry_path(dir, &name) else { continue };
        if entry.is_dir() {
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("архив: {e}"))?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            let perms = std::os::unix::fs::PermissionsExt::from_mode(mode & 0o777);
            let _ = f.set_permissions(perms);
        }
    }
    Ok(archive.len())
}

/// Windows: распаковать exe из zip и подменить запущенный файл.
#[cfg(windows)]
fn place(zip: &Path) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staging = exe.parent().unwrap_or(&exe).join("yamusic-rs-update");
    let _ = std::fs::remove_dir_all(&staging);
    unzip(zip, &staging)?;
    let new = staging.join(concat!(env!("CARGO_PKG_NAME"), ".exe"));
    if !new.is_file() {
        return Err("в архиве нет exe".into());
    }
    let old = exe.with_extension("old.exe");
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        return Err(format!("не удалось заменить exe: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("не удалось заменить exe: {e}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(exe)
}

/// `…/<Приложение>.app/Contents/MacOS/yamusic-rs` → путь к `.app`. `None` — бинарник
/// запущен не из бандла (например, `cargo run`): обновлять нечего, ведём на страницу.
#[cfg(target_os = "macos")]
fn app_bundle(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()? == "app").then_some(bundle)
}

/// macOS: распаковать новый `.app` и подменить бандл запущенного приложения.
#[cfg(target_os = "macos")]
fn place(zip: &Path) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let Some(bundle) = app_bundle(&exe) else {
        return Err("приложение запущено не из .app".into());
    };
    let parent = bundle.parent().ok_or("у бандла нет папки")?;
    let staging = parent.join(".yamusic-rs-update");
    let _ = std::fs::remove_dir_all(&staging);
    unzip(zip, &staging)?;
    // Внутри zip бандл лежит целиком: staging/<Имя>.app.
    let new = std::fs::read_dir(&staging)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "app"))
        .ok_or("в архиве нет .app")?
        .path();
    let old = bundle.with_extension("old.app");
    let _ = std::fs::remove_dir_all(&old);
    if let Err(e) = std::fs::rename(bundle, &old) {
        return Err(format!("не удалось заменить приложение: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, bundle) {
        let _ = std::fs::rename(&old, bundle);
        return Err(format!("не удалось заменить приложение: {e}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(bundle.join("Contents").join("MacOS").join(env!("CARGO_PKG_NAME")))
}

/// Запустить обновлённое приложение. Оно дождётся, пока этот процесс закроется.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe)
        .arg(AFTER_UPDATE)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("новая версия не запустилась: {e}"))
}

/// Убрать файлы прошлой версии, оставшиеся после обновления: `*.old.exe` на Windows,
/// `*.old.app` на macOS. Старый процесс может ещё завершаться и держать их, поэтому
/// пробуем несколько раз в фоне.
pub fn cleanup() {
    let old: Option<PathBuf> = (|| {
        let exe = std::env::current_exe().ok()?;
        #[cfg(windows)]
        {
            Some(exe.with_extension("old.exe"))
        }
        #[cfg(target_os = "macos")]
        {
            app_bundle(&exe).map(|b| b.with_extension("old.app").to_path_buf())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            None
        }
    })();
    let Some(old) = old.filter(|p| p.exists()) else { return };
    std::thread::spawn(move || {
        for _ in 0..10 {
            let gone = if old.is_dir() { std::fs::remove_dir_all(&old) } else { std::fs::remove_file(&old) };
            if gone.is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10"), Some((1, 10, 0)));
        assert!(parse_version("v0.10.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn asset_names() {
        // Индустриальный нейминг: имя_версия_os_arch.zip, архитектуры без Rust-троек.
        assert_eq!(asset_name_for("windows", "x86_64", "1.2.3").as_deref(), Some("yamusic-rs_1.2.3_windows_x86_64.zip"));
        assert_eq!(asset_name_for("macos", "aarch64", "1.2.3").as_deref(), Some("yamusic-rs_1.2.3_macos_arm64.zip"));
        assert_eq!(asset_name_for("macos", "x86_64", "1.2.3").as_deref(), Some("yamusic-rs_1.2.3_macos_x86_64.zip"));
        // Linux и неизвестные архитектуры сами не обновляются.
        assert_eq!(asset_name_for("linux", "x86_64", "1.2.3"), None);
        assert_eq!(asset_name_for("windows", "riscv64", "1.2.3"), None);
    }

    #[test]
    fn own_asset_name() {
        let name = asset_name("9.9.9").expect("на сборочных системах имя есть");
        assert!(name.starts_with("yamusic-rs_9.9.9_"));
        assert!(name.ends_with(".zip"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn zip_roundtrip() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("yamusic-rs-test-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Собираем zip вручную: файл в корне и файл в папке — как пакеты из релиза;
        // плюс метаданные AppleDouble, которые ditto кладёт в zip для macOS.
        let zip = dir.join("a.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            w.add_directory("inner/", opts).unwrap();
            w.start_file("yamusic-rs.exe", opts).unwrap();
            w.write_all(b"binary").unwrap();
            w.start_file("inner/readme.txt", opts).unwrap();
            w.write_all(b"hi").unwrap();
            w.start_file("._yamusic-rs.exe", opts).unwrap();
            w.write_all(b"xattr").unwrap();
            w.start_file("__MACOSX/x", opts).unwrap();
            w.write_all(b"xattr").unwrap();
            w.finish().unwrap();
        }
        let out = dir.join("out");
        assert_eq!(unzip(&zip, &out).unwrap(), 5);
        assert_eq!(std::fs::read(out.join("yamusic-rs.exe")).unwrap(), b"binary");
        assert_eq!(std::fs::read(out.join("inner/readme.txt")).unwrap(), b"hi");
        // AppleDouble не распаковывается.
        assert!(!out.join("._yamusic-rs.exe").exists());
        assert!(!out.join("__MACOSX").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn entry_path_stays_inside() {
        let dir = Path::new("/tmp/x");
        assert!(safe_entry_path(dir, "a/b.txt").is_some());
        assert!(safe_entry_path(dir, "../escape.txt").is_none());
        assert!(safe_entry_path(dir, "a/../../escape.txt").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_detection() {
        assert_eq!(app_bundle(Path::new("/Applications/Яндекс Музыка RS.app/Contents/MacOS/yamusic-rs")), Some(Path::new("/Applications/Яндекс Музыка RS.app")));
        // Не из бандла — cargo run.
        assert_eq!(app_bundle(Path::new("/code/yamusic-rs/target/release/yamusic-rs")), None);
        // Бандл без Contents.
        assert_eq!(app_bundle(Path::new("/x/A.app/MacOS/yamusic-rs")), None);
    }
}
