//! Значок в меню-баре macOS (NSStatusItem через крейт tray-icon): клик открывает меню,
//! как у нативных трей-приложений. Медиаклавиши — через MPRemoteCommandCenter.
//! Windows-аналог — tray.rs; типы Event и MenuState совпадают, gui.rs работает с обоими.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use std::cell::RefCell;
use std::rc::Rc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Toggle,
    /// Приходит от второго запуска на Windows (через окно трея); на macOS wake идёт через сокет.
    #[allow(dead_code)]
    Show,
    PlayPause,
    Prev,
    Next,
    Updates,
    Terms,
    Recommendations,
    Support,
    Quit,
}

/// Что показать в меню: окно свёрнуто или спрятано, играет ли трек, есть ли соседние.
pub struct MenuState {
    pub minimized: bool,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
}

/// Токен добавленной медиакоманды: команда держит handler сама,
/// но Retained храним, чтобы быть уверенными в времени жизни.
type MediaTarget = Retained<AnyObject>;

struct Tray {
    _icon: tray_icon::TrayIcon,
    toggle: MenuItem,
    play: MenuItem,
    prev: MenuItem,
    next: MenuItem,
    on_event: Rc<dyn Fn(Event)>,
    state: Box<dyn Fn() -> MenuState>,
    timer: slint::Timer,
    _media: Vec<MediaTarget>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn tr(text: &str) -> String {
    crate::i18n::tr(text)
}

/// Иконка для меню-бара: готовый 256.png из packaging, ужатый до 18×18.
/// Картинка вшита через include_bytes!, так что .app ничего не ищет на диске.
fn icon() -> Option<tray_icon::Icon> {
    use image::imageops::FilterType;
    static PNG: &[u8] = include_bytes!("../packaging/icons/256.png");
    let img = image::load_from_memory(PNG).ok()?;
    let rgba = image::imageops::resize(&img.to_rgba8(), 18, 18, FilterType::Lanczos3).into_raw();
    tray_icon::Icon::from_rgba(rgba, 18, 18).ok()
}

pub fn install(on_event: impl Fn(Event) + 'static, state: impl Fn() -> MenuState + 'static) {
    let Some(icon) = icon() else {
        crate::config::log("иконка трея не собралась");
        return;
    };
    let menu = Menu::new();
    let toggle = MenuItem::with_id("toggle", tr("Развернуть"), true, None);
    let updates = MenuItem::with_id("updates", tr("Проверить обновления"), true, None);
    let play = MenuItem::with_id("playpause", tr("Воспроизведение"), true, None);
    let prev = MenuItem::with_id("prev", tr("Предыдущая песня"), true, None);
    let next = MenuItem::with_id("next", tr("Следующая песня"), true, None);
    let terms = MenuItem::with_id("terms", tr("Пользовательское соглашение"), true, None);
    let recs = MenuItem::with_id("recs", tr("Правила рекомендаций"), true, None);
    let support = MenuItem::with_id("support", tr("Чат с поддержкой"), true, None);
    let quit = MenuItem::with_id("quit", tr("Закрыть приложение"), true, None);
    let about = Submenu::with_id("about", tr("О приложении"), true);
    let _ = about.append(&terms);
    let _ = about.append(&recs);
    let _ = about.append(&support);
    let items: [&dyn tray_icon::menu::IsMenuItem; 7] = [&toggle, &updates, &play, &prev, &next, &about, &quit];
    for item in items {
        let _ = menu.append(item);
    }
    let _ = menu.insert(&PredefinedMenuItem::separator(), 2);
    let _ = menu.insert(&PredefinedMenuItem::separator(), 5);
    let _ = menu.insert(&PredefinedMenuItem::separator(), 9);

    let tray_icon = match tray_icon::TrayIconBuilder::new()
        .with_tooltip(env!("APP_NAME"))
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            crate::config::log(format!("трей не создался: {e}"));
            return;
        }
    };

    let media = install_media_keys();

    let tray = Tray {
        _icon: tray_icon,
        toggle,
        play,
        prev,
        next,
        on_event: Rc::new(on_event),
        state: Box::new(state),
        timer: slint::Timer::default(),
        _media: media,
    };
    tray.timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(250), poll);
    TRAY.with(|t| *t.borrow_mut() = Some(tray));
}

pub fn remove() {
    TRAY.with(|t| {
        if let Some(tray) = t.borrow_mut().take() {
            tray.timer.stop();
            let _ = tray._icon.set_visible(false);
            // Drop TrayIcon убирает значок из меню-бара.
        }
    });
}

fn emit(e: Event) {
    let _ = slint::invoke_from_event_loop(move || {
        let f = TRAY.with(|t| t.borrow().as_ref().map(|t| t.on_event.clone()));
        if let Some(f) = f {
            f(e);
        }
    });
}

fn poll() {
    // События меню muda кладёт в глобальный канал — вычитываем его.
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        let e = match ev.id.as_ref() {
            "toggle" => Some(Event::Toggle),
            "updates" => Some(Event::Updates),
            "playpause" => Some(Event::PlayPause),
            "prev" => Some(Event::Prev),
            "next" => Some(Event::Next),
            "terms" => Some(Event::Terms),
            "recs" => Some(Event::Recommendations),
            "support" => Some(Event::Support),
            "quit" => Some(Event::Quit),
            _ => None,
        };
        if let Some(e) = e {
            let f = TRAY.with(|t| t.borrow().as_ref().map(|t| t.on_event.clone()));
            if let Some(f) = f {
                f(e);
            }
        }
    }
    // Подписи и доступность пунктов — по текущему состоянию плеера и окна.
    let tray = TRAY.with(|t| {
        t.borrow().as_ref().map(|t| (t.toggle.clone(), t.play.clone(), t.prev.clone(), t.next.clone()))
    });
    let Some((toggle, play, prev, next)) = tray else { return };
    let s = TRAY.with(|t| t.borrow().as_ref().map(|t| (t.state)()));
    let Some(s) = s else { return };
    toggle.set_text(if s.minimized { tr("Развернуть") } else { tr("Свернуть") });
    play.set_text(if s.playing { tr("Пауза") } else { tr("Воспроизведение") });
    prev.set_enabled(s.can_prev);
    next.set_enabled(s.can_next);
}

/// Глобальные медиаклавиши: системный Now Playing (кнопки на клавиатуре, меню звука).
/// Роль, которую на Windows играет RegisterHotKey, а на Linux — MPRIS.
fn install_media_keys() -> Vec<MediaTarget> {
    use block2::RcBlock;
    use objc2_media_player::{MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus};

    let mut targets = Vec::new();
    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let map = unsafe {
        [
            (center.togglePlayPauseCommand(), Event::PlayPause),
            (center.nextTrackCommand(), Event::Next),
            (center.previousTrackCommand(), Event::Prev),
        ]
    };
    for (command, event) in map {
        let block = RcBlock::new(move |_e: std::ptr::NonNull<MPRemoteCommandEvent>| -> MPRemoteCommandHandlerStatus {
            emit(event);
            MPRemoteCommandHandlerStatus::Success
        });
        let target = unsafe { command.addTargetWithHandler(&block) };
        targets.push(target);
    }
    targets
}
