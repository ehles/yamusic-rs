//! macOS: окно через winit и objc (orderOut/makeKeyAndOrderFront), пути в ~/Library,
//! один экземпляр через unix-сокет, диалоги через osascript, автозапуск через LaunchAgent.

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::sel;
use objc2_foundation::NSRect;
use slint::winit_030::{winit, WinitWindowAccessor};
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// Как APP_ID у Linux: имя сокета и LaunchAgent.
const APP_ID: &str = "yamusic-rs";

const NIL: *mut AnyObject = std::ptr::null_mut();

/// NSWindow из Slint-окна: у winit берём указатель NSView, у него — окно.
fn ns_window(w: &slint::Window) -> *mut AnyObject {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let view = w
        .with_winit_window(|ww| match ww.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr(),
            _ => std::ptr::null_mut(),
        })
        .unwrap_or(std::ptr::null_mut());
    if view.is_null() {
        return NIL;
    }
    unsafe { msg_send![view as *mut AnyObject, window] }
}

/// Вывести приложение вперёд. activate появился в macOS 14, раньше был
/// activateIgnoringOtherApps (теперь deprecated, но рабочий).
fn activate_app() {
    let Some(app_class) = AnyClass::get(c"NSApplication") else { return };
    let app: *mut AnyObject = unsafe { msg_send![app_class, sharedApplication] };
    if app.is_null() {
        return;
    }
    unsafe {
        let has_activate: bool = msg_send![app, respondsToSelector: sel!(activate)];
        if has_activate {
            let _: () = msg_send![app, activate];
        } else {
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }
}

/// Окно уже создано: winit делает это не сразу после `show()`.
pub fn created(w: &slint::Window) -> bool {
    w.with_winit_window(|_| ()).is_some()
}

/// Один раз после создания окна. На macOS ничего не нужно: значок окна и в доке берётся
/// из .app-бандла, скругление углов и тень система делает сама.
pub fn prepare(_w: &slint::Window) {}

pub fn raise(w: &slint::Window) {
    let win = ns_window(w);
    if win.is_null() {
        return;
    }
    unsafe {
        let mini: bool = msg_send![win, isMiniaturized];
        if mini {
            let _: () = msg_send![win, deminiaturize: NIL];
        }
        let _: () = msg_send![win, makeKeyAndOrderFront: NIL];
    }
    activate_app();
}

pub fn is_shown(w: &slint::Window) -> bool {
    let win = ns_window(w);
    !win.is_null() && unsafe {
        let visible: bool = msg_send![win, isVisible];
        visible
    }
}

pub fn is_minimized(w: &slint::Window) -> bool {
    let win = ns_window(w);
    !win.is_null() && unsafe {
        let mini: bool = msg_send![win, isMiniaturized];
        mini
    }
}

/// Масштаб монитора, на котором окно: сам winit его знает (Retina и внешние мониторы).
pub fn scale(w: &slint::Window) -> f32 {
    w.with_winit_window(|ww| ww.scale_factor() as f32).unwrap_or(1.0)
}

/// Спрятать окно в трей: orderOut, не закрывая Slint-окно — цикл событий живёт.
pub fn hide(w: &slint::Window) {
    let win = ns_window(w);
    if !win.is_null() {
        unsafe {
            let _: () = msg_send![win, orderOut: NIL];
        }
    }
}

/// Рабочая область монитора с окном (без меню-бара и дока): visibleFrame экрана,
/// пересчитанный в координаты с началом в левом верхнем углу основного монитора.
pub fn work_area(w: &slint::Window) -> (i32, i32, i32, i32) {
    let win = ns_window(w);
    if win.is_null() {
        return (0, 0, 0, 0);
    }
    unsafe {
        let screen: *mut AnyObject = msg_send![win, screen];
        if screen.is_null() {
            return (0, 0, 0, 0);
        }
        let frame: NSRect = msg_send![screen, visibleFrame];
        let primary_h = primary_screen_height();
        let (l, r) = (frame.origin.x, frame.origin.x + frame.size.width);
        let (t, b) = (primary_h - frame.origin.y - frame.size.height, primary_h - frame.origin.y);
        (l as i32, t as i32, r as i32, b as i32)
    }
}

/// Высота основного монитора в точках: NSScreen держит глобальные координаты снизу вверх.
fn primary_screen_height() -> f64 {
    let Some(screens_class) = AnyClass::get(c"NSScreen") else { return 0.0 };
    let screens: *mut AnyObject = unsafe { msg_send![screens_class, screens] };
    if screens.is_null() {
        return 0.0;
    }
    let first: *mut AnyObject = unsafe { msg_send![screens, objectAtIndexedSubscript: 0usize] };
    if first.is_null() {
        return 0.0;
    }
    let frame: NSRect = unsafe { msg_send![first, frame] };
    frame.size.height
}

/// Запустить диалог osascript и вернуть выбранный путь. Диалог модальный.
fn dialog(script: String) -> Option<Option<String>> {
    let out = Command::new("osascript").args(["-e", &script]).stderr(Stdio::null()).output().ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some((out.status.success() && !path.is_empty()).then_some(path))
}

pub fn pick_image(_owner: &slint::Window, title: &str) -> Option<String> {
    let title = title.replace('\\', "\\\\").replace('"', "\\\"");
    dialog(format!("POSIX path of (choose file of type {{\"public.image\"}} with prompt \"{title}\")")).flatten()
}

pub fn pick_folder(_owner: &slint::Window, title: &str, start: &str) -> Option<String> {
    let title = title.replace('\\', "\\\\").replace('"', "\\\"");
    let start = start.replace('\\', "\\\\").replace('"', "\\\"");
    dialog(format!("POSIX path of (choose folder with prompt \"{title}\" default location POSIX file \"{start}\")")).flatten()
}

pub fn open_url(url: &str) {
    if let Ok(mut child) = Command::new("open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// Отдать системе неиспользуемую память: pressure relief на дефолтной malloc-зоне.
pub fn trim_memory() {
    extern "C" {
        fn malloc_default_zone() -> *mut std::ffi::c_void;
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize);
    }
    unsafe { malloc_zone_pressure_relief(malloc_default_zone(), usize::MAX) };
}

pub fn release_memory() {
    trim_memory();
}

/// Светлая ли тема macOS: ключ AppleInterfaceStyle существует только в тёмной теме.
pub fn system_light_theme() -> bool {
    !matches!(
        Command::new("defaults").args(["read", "-g", "AppleInterfaceStyle"]).stderr(Stdio::null()).output().ok(),
        Some(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "Dark"
    )
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn config_base() -> PathBuf {
    home().join("Library/Application Support")
}

pub fn cache_base() -> PathBuf {
    home().join("Library/Caches")
}

pub fn music_dir() -> PathBuf {
    home().join("Music")
}

/// Имя компьютера для списка устройств в аккаунте Яндекса.
pub fn hostname() -> Option<String> {
    let out = Command::new("scutil").args(["--get", "ComputerName"]).stderr(Stdio::null()).output().ok()?;
    (out.status.success()).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|n| !n.is_empty())
}

/// Скопировать текст в буфер обмена.
pub fn copy_text(text: &str) {
    use std::io::Write as _;
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn autostart_plist() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{APP_ID}.plist"))
}

pub fn autostart_enabled() -> bool {
    autostart_plist().exists()
}

/// LaunchAgent с RunAtLoad, сразу в трей — как автозапуск оригинала на Windows.
pub fn set_autostart(on: bool) -> bool {
    let path = autostart_plist();
    if !on {
        let _ = Command::new("launchctl").arg("unload").arg(&path).stdout(Stdio::null()).stderr(Stdio::null()).output();
        return std::fs::remove_file(&path).is_ok() || !path.exists();
    }
    let Some(exe) = std::env::current_exe().ok() else { return false };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{APP_ID}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--tray</string>
  </array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        exe.display()
    );
    path.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&path, plist).is_ok() && {
        let _ = Command::new("launchctl").arg("load").arg(&path).stdout(Stdio::null()).stderr(Stdio::null()).output();
        true
    }
}

pub fn refresh_autostart() {
    if autostart_enabled() {
        set_autostart(true);
    }
}

fn socket_path() -> PathBuf {
    match std::env::var_os("TMPDIR") {
        Some(dir) => Path::new(&dir).join(format!("{APP_ID}.sock")),
        None => std::env::temp_dir().join(format!("{APP_ID}-{}.sock", std::env::var("USER").unwrap_or_default())),
    }
}

static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

/// Занять сокет единственного экземпляра. Сокет остаётся на диске и после падения,
/// поэтому занятый, но молчащий файл считается брошенным.
pub fn claim_instance() -> bool {
    let path = socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                return false;
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(_) => return true,
            }
        }
        // Нет доступа к папке — работаем без проверки второго запуска.
        Err(_) => return true,
    };
    *LISTENER.lock().unwrap() = Some(listener);
    true
}

/// Второй запуск: попросить работающий экземпляр показать окно.
pub fn wake_existing() {
    use std::io::Write;
    if let Ok(mut s) = UnixStream::connect(socket_path()) {
        let _ = s.write_all(b"show");
    }
}

/// Звать `f` из фонового потока, когда второй запуск просит показать окно.
/// Пустое соединение — это проверка из `claim_instance`, на неё не реагируем.
pub fn on_wake(f: impl Fn() + Send + 'static) {
    let Some(listener) = LISTENER.lock().unwrap().take() else { return };
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 4];
            if s.read(&mut buf).is_ok_and(|n| n > 0) {
                f();
            }
        }
    });
}

pub fn cleanup() {
    crate::tray::remove();
    let _ = std::fs::remove_file(socket_path());
}
