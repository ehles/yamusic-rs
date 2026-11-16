//! Аудио-движок: очередь, загрузка и предзагрузка треков, фидбек волне, воспроизведение.
//! Живёт в своём потоке, UI шлёт ему команды и читает `Shared`.

use crate::api::{Account, Api, Batch, Res, Track, WAVE};
use crate::config::log;
use crate::eq::{Eq, EqParams};
use crate::fade::{FadeState, SeekFade};
use crate::stream::HttpStream;
// rodio 0.22: Sink переименован в Player, OutputStream — в MixerDeviceSink.
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player as Sink};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Default, Debug)]
pub enum Status {
    #[default]
    LoggedOut,
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListKind {
    Playlist,
    Album,
    Artist,
    Likes,
    Tracks,
}

/// Что сейчас играет.
#[derive(Clone, PartialEq, Debug, Default)]
pub enum Source {
    #[default]
    Wave,
    /// Волна по станции: `artist:ID`, `track:ID`, `album:ID`, `playlist:UID_KIND`.
    Seed { station: String, header: String, title: String },
    List { kind: ListKind, id: String, title: String },
}

impl Source {
    pub fn station(&self) -> Option<&str> {
        match self {
            Source::Wave => Some(WAVE),
            Source::Seed { station, .. } => Some(station),
            Source::List { .. } => None,
        }
    }
}

/// Что играть, когда список закончится.
#[derive(Clone, Debug)]
pub enum After {
    Loop,
    Wave(Source),
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Default)]
pub struct Shared {
    pub status: Status,
    pub uid: String,
    pub login: String,
    pub has_plus: bool,
    pub track: Option<Track>,
    pub source: Source,
    pub upcoming: Vec<Track>,
    /// Версия очереди: upcoming пересобирается только при её изменении (без churn каждые 200 мс).
    pub queue_ver: u64,
    pub playing: bool,
    pub loading: bool,
    pub position: f32,
    pub duration: f32,
    pub volume: f32,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub error: Option<(String, Instant)>,
}

pub enum Cmd {
    Login(String),
    Logout,
    PlayPause,
    /// Снять с паузы (если уже играет — ничего не делать).
    Resume,
    /// Поставить на паузу (если уже на паузе — ничего не делать).
    /// Нужен командам медиа-клавиш macOS (на других платформах не создаётся).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Pause,
    Next,
    Prev,
    Seek(f32),
    Volume(f32),
    LikeTrack(Track),
    Dislike,
    Wave(Source),
    PlayList { source: Source, tracks: Arc<Vec<Track>>, start: usize, after: After },
    /// Перейти к треку из очереди (индекс в `upcoming`).
    Jump(usize),
    Repeat(Repeat),
    Shuffle(bool),
    /// «Играть следующим» (`next`) или «Добавить в конец очереди».
    Enqueue { track: Track, next: bool },
}

enum Msg {
    Cmd(Cmd),
    Account(Res<Account>),
    Likes(Res<Vec<String>>),
    Batch { res: Res<Batch>, replace: bool, gen: u64 },
    Audio { gen: u64, res: Res<HttpStream> },
    Prefetch { id: String, res: Res<HttpStream> },
    Error(String),
}

type Notify = Arc<dyn Fn() + Send + Sync>;
type Job = Box<dyn FnOnce() + Send>;
pub type Likes = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
pub struct Player {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    pub eq: Arc<EqParams>,
    /// Цель последнего запроса перемотки: движок отбрасывает устаревшие запросы.
    latest_seek: Arc<AtomicU32>,
}

impl Player {
    pub fn spawn(notify: Notify) -> Player {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared { volume: 0.6, ..Default::default() }));
        let likes: Likes = Default::default();
        let eq: Arc<EqParams> = Default::default();
        let latest_seek = Arc::new(AtomicU32::new(0));
        let p = Player { tx: tx.clone(), shared: shared.clone(), likes: likes.clone(), eq: eq.clone(), latest_seek: latest_seek.clone() };
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || Engine::new(tx, shared, likes, notify, eq, latest_seek).run(rx))
            .expect("audio thread");
        p
    }

    pub fn send(&self, c: Cmd) {
        if let Cmd::Seek(s) = c {
            self.latest_seek.store(s.max(0.0).to_bits(), Ordering::Relaxed);
        }
        let _ = self.tx.send(Msg::Cmd(c));
    }

    pub fn state(&self) -> Shared {
        self.shared.lock().unwrap().clone()
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.likes.lock().unwrap().contains(id)
    }

}

/// Перемешивание без лишних зависимостей (xorshift от времени).
pub fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

enum End {
    Finished,
    Skip,
    Dislike,
}

struct ListPlay {
    all: Arc<Vec<Track>>,
    after: After,
}

struct Engine {
    tx: Sender<Msg>,
    bg: Sender<Job>,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    notify: Notify,
    eq: Arc<EqParams>,
    audio: Option<(MixerDeviceSink, Sink)>,
    /// Состояние микро-фейда на стыке перемотки (общее с аудиопотоком).
    fade: Arc<FadeState>,
    /// Цель последнего запроса перемотки (общее с обёрткой Player).
    latest_seek: Arc<AtomicU32>,
    /// Цель перемотки, ещё не применённой к звуку: sync() держит её в UI,
    /// чтобы ползунок не отскакивал к старой позиции и к целям очереди seek'ов.
    seek_pending: Option<f32>,
    api: Option<Api>,
    uid: String,
    source: Source,
    list: Option<ListPlay>,
    queue: VecDeque<Track>,
    history: Vec<Track>,
    current: Option<Track>,
    /// Адрес mp3 текущего трека — для повтора (Repeat::One) без повторного поиска ссылки.
    current_url: Option<String>,
    batch_id: String,
    /// Сессия ротора текущей волны; radioStarted уходит в неё, когда она появится.
    session: String,
    radio_started_pending: bool,
    /// Пропуски и дослушанные треки, которые уйдут со следующим запросом треков волны.
    deferred: Vec<serde_json::Value>,
    /// Сколько пакетов подряд состояло только из уже сыгранных треков.
    stale_batches: u32,
    fetching: bool,
    batch_gen: u64,
    retry_at: Option<Instant>,
    gen: u64,
    /// Общее состояние текущего HttpStream: для предзагрузки куска при перемотке.
    current_shared: Option<Arc<crate::stream::Shared>>,
    loaded: bool,
    paused: bool,
    volume: f32,
    repeat: Repeat,
    shuffle: bool,
    queue_ver: u64,
    prefetch: Option<(String, HttpStream)>,
    prefetching: Option<String>,
    errors_in_row: u32,
    was_playing: bool,
}

impl Engine {
    fn new(tx: Sender<Msg>, shared: Arc<Mutex<Shared>>, likes: Likes, notify: Notify, eq: Arc<EqParams>, latest_seek: Arc<AtomicU32>) -> Self {
        // Фидбек и лайки — по порядку в отдельном потоке, чтобы не блокировать плеер.
        let (bg, bg_rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("bg".into())
            .spawn(move || {
                for job in bg_rx {
                    job();
                }
            })
            .expect("bg thread");
        Engine {
            tx,
            bg,
            shared,
            likes,
            notify,
            eq,
            audio: None,
            api: None,
            uid: String::new(),
            source: Source::Wave,
            list: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            current: None,
            current_url: None,
            batch_id: String::new(),
            session: String::new(),
            radio_started_pending: false,
            deferred: Vec::new(),
            stale_batches: 0,
            fetching: false,
            batch_gen: 0,
            retry_at: None,
            gen: 0,
            current_shared: None,
            fade: FadeState::new(),
            latest_seek,
            seek_pending: None,
            loaded: false,
            paused: true,
            volume: 0.6,
            repeat: Repeat::Off,
            shuffle: false,
            queue_ver: 0,
            prefetch: None,
            prefetching: None,
            errors_in_row: 0,
            was_playing: false,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(msg) => {
                    self.handle(msg);
                    self.sync();
                    (self.notify)();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    // ---------- вспомогательное ----------

    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn sink(&self) -> Option<&Sink> {
        self.audio.as_ref().map(|(_, s)| s)
    }

    fn ensure_audio(&mut self) -> bool {
        if self.audio.is_none() {
            match DeviceSinkBuilder::open_default_sink() {
                Ok(mut stream) => {
                    stream.log_on_drop(false);
                    let sink = Sink::connect_new(stream.mixer());
                    self.audio = Some((stream, sink));
                }
                Err(e) => log(format!("audio device: {e}")),
            }
        }
        self.audio.is_some()
    }

    fn pos(&self) -> f32 {
        self.sink().map(|s| s.get_pos().as_secs_f32()).unwrap_or(0.0)
    }

    fn error(&self, e: impl Into<String>) {
        let e = e.into();
        log(format!("error: {e}"));
        self.sh().error = Some((e, Instant::now()));
        (self.notify)();
    }

    fn feedback(&mut self, kind: &'static str, track: Option<String>, played: Option<f64>) {
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        if self.session.is_empty() && kind == "radioStarted" {
            self.radio_started_pending = true;
            return;
        }
        let batch = self.batch_id.clone();
        let session = self.session.clone();
        if session.is_empty() {
            let _ = self.bg.send(Box::new(move || {
                if let Err(e) = api.feedback(&station, kind, track.as_deref(), played, &batch) {
                    log(format!("feedback {kind}: {e}"));
                }
            }));
            return;
        }
        let from = format!("desktop_win-radio-{}-default", station.replace(':', "-"));
        let body = crate::api::session_event(kind, track.as_deref(), played, &batch, &from);
        if matches!(kind, "skip" | "dislike" | "trackFinished") {
            self.deferred.push(body);
            return;
        }
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = api.session_feedback(&session, &body) {
                log(format!("feedback {kind}: {e}"));
            }
        }));
    }

    /// Отложенные фидбеки, которые не ушли с запросом треков, отправляются отдельно.
    /// `parallel` — отдельным потоком, а не очередью bg: при смене станции они не должны
    /// задерживать запрос треков новой сессии (старой сессии они уже не влияют на подбор).
    fn flush_feedback(&mut self, parallel: bool) {
        if self.deferred.is_empty() {
            return;
        }
        let list = std::mem::take(&mut self.deferred);
        let (Some(api), false) = (self.api.clone(), self.session.is_empty()) else { return };
        let session = self.session.clone();
        let job = move || {
            for body in list {
                if let Err(e) = api.session_feedback(&session, &body) {
                    log(format!("feedback: {e}"));
                }
            }
        };
        if parallel {
            thread::spawn(job);
        } else {
            let _ = self.bg.send(Box::new(job));
        }
    }

    fn stop_audio(&mut self) {
        if let Some(s) = self.sink() {
            s.clear();
        }
        self.loaded = false;
        self.seek_pending = None;
    }

    fn clear_queue(&mut self) {
        self.flush_feedback(false);
        self.stale_batches = 0;
        self.queue.clear();
        self.queue_ver += 1;
        self.prefetch = None;
        self.prefetching = None;
        self.fetching = false;
        self.batch_gen += 1;
        self.retry_at = None;
        self.batch_id.clear();
        self.session.clear();
    }

    fn reset(&mut self) {
        self.stop_audio();
        self.gen += 1;
        self.clear_queue();
        self.history.clear();
        self.current = None;
        self.current_url = None;
        self.list = None;
        self.source = Source::Wave;
        let mut s = self.sh();
        s.track = None;
        s.source = Source::Wave;
        s.loading = false;
    }

    fn set_source(&mut self, src: Source) {
        self.source = src.clone();
        self.sh().source = src;
    }

    /// Состояние движка → Shared для UI.
    fn sync(&mut self) {
        let playing = self.loaded && !self.paused && self.sink().map(|s| !s.is_paused()).unwrap_or(false);
        let pos = match self.seek_pending {
            Some(p) => p,
            None => if self.loaded { self.pos() } else { 0.0 },
        };
        let mut s = self.shared.lock().unwrap();
        s.playing = playing;
        s.position = pos;
        s.volume = self.volume;
        s.repeat = self.repeat;
        s.shuffle = self.shuffle;
        if s.queue_ver != self.queue_ver {
            s.queue_ver = self.queue_ver;
            s.upcoming = self.queue.iter().take(30).cloned().collect();
        }
    }

    // ---------- главный цикл ----------

    fn tick(&mut self) {
        if let Some(at) = self.retry_at {
            if Instant::now() >= at {
                self.retry_at = None;
                self.fetch_batch();
            }
        }
        if self.loaded && !self.paused && self.sink().map(|s| s.empty()).unwrap_or(false) {
            if self.repeat == Repeat::One && self.replay() {
                return;
            }
            self.advance(End::Finished);
            self.sync();
            (self.notify)();
            return;
        }
        self.sync();
        if self.loaded && self.prefetch.is_none() && self.prefetching.is_none() {
            self.prefetch_next();
        }
        let playing = self.sh().playing;
        if playing != self.was_playing {
            self.was_playing = playing;
            (self.notify)();
        }
    }

    fn replay(&mut self) -> bool {
        // Повтор открываем в фоне по уже известному адресу: не блокируем аудиодвижок
        // сетью, как при обычной загрузке (результат придёт сообщением Msg::Audio).
        let (Some(api), Some(url)) = (self.api.clone(), self.current_url.clone()) else { return false };
        if self.current.is_none() {
            return false;
        }
        self.gen += 1;
        self.loaded = false;
        self.seek_pending = None;
        {
            let mut s = self.sh();
            s.position = 0.0;
            s.loading = true;
        }
        let gen = self.gen;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = api.reopen_stream(&url);
            let _ = tx.send(Msg::Audio { gen, res });
        });
        true
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(c) => self.cmd(c),
            Msg::Account(res) => match res {
                Ok(acc) => {
                    self.uid = acc.uid.clone();
                    {
                        let mut s = self.sh();
                        s.status = Status::Ready;
                        s.uid = acc.uid.clone();
                        s.login = acc.login;
                        s.has_plus = acc.has_plus;
                        s.error = None;
                    }
                    self.feedback("radioStarted", None, None);
                    self.fetch_batch();
                    if let Some(api) = self.api.clone() {
                        let tx = self.tx.clone();
                        let uid = acc.uid;
                        thread::spawn(move || {
                            let _ = tx.send(Msg::Likes(api.liked_ids(&uid)));
                        });
                    }
                }
                Err(e) => {
                    log(format!("account: {e}"));
                    self.api = None;
                    self.sh().status = Status::Failed(e);
                }
            },
            Msg::Likes(res) => match res {
                Ok(ids) => self.likes.lock().unwrap().extend(ids),
                Err(e) => log(format!("likes: {e}")),
            },
            Msg::Batch { res, replace, gen } => {
                if gen != self.batch_gen {
                    return;
                }
                self.fetching = false;
                match res {
                    Ok(b) => self.on_batch(b, replace),
                    Err(e) => {
                        self.error(format!("Не удалось получить треки волны: {e}"));
                        self.retry_at = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
            }
            Msg::Audio { gen, res } => {
                if gen == self.gen {
                    match res {
                        Ok(b) => self.play_loaded(b),
                        Err(e) => self.on_load_error(e),
                    }
                }
            }
            Msg::Prefetch { id, res } => {
                if self.prefetching.as_deref() == Some(id.as_str()) {
                    self.prefetching = None;
                }
                let waiting = !self.loaded && self.current.as_ref().map(|t| t.id == id).unwrap_or(false);
                match res {
                    Ok(b) if waiting => self.play_loaded(b),
                    Ok(b) => self.prefetch = Some((id, b)),
                    Err(e) => {
                        log(format!("prefetch {id}: {e}"));
                        if waiting {
                            self.start_download();
                        }
                    }
                }
            }
            Msg::Error(e) => self.error(e),
        }
    }

    fn cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Login(token) => {
                self.reset();
                let api = Api::new(&token);
                self.api = Some(api.clone());
                {
                    let mut s = self.sh();
                    s.status = Status::Connecting;
                    s.error = None;
                }
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let _ = tx.send(Msg::Account(api.account()));
                });
            }
            Cmd::Logout => {
                self.reset();
                self.api = None;
                self.uid.clear();
                self.likes.lock().unwrap().clear();
                let mut s = self.sh();
                s.status = Status::LoggedOut;
                s.login.clear();
                s.uid.clear();
            }
            Cmd::Resume => {
                self.paused = false;
                if let Some(s) = self.sink() {
                    s.play();
                }
            }
            Cmd::Pause => {
                self.paused = true;
                if let Some(s) = self.sink() {
                    s.pause();
                }
            }
            Cmd::PlayPause => {
                self.paused = !self.paused;
                if let Some(s) = self.sink() {
                    if self.paused {
                        s.pause()
                    } else {
                        s.play()
                    }
                }
            }
            Cmd::Next => {
                self.errors_in_row = 0;
                if self.current.is_some() {
                    self.advance(End::Skip);
                }
            }
            Cmd::Prev => self.prev(),
            Cmd::Seek(sec) => {
                // rodio применяет seek в аудиоколбэке. Coarse seek (rodio 0.22)
                // читает лишь пару кадров около цели, но и они не должны ходить
                // в сеть — поэтому кусок докачиваем здесь, в движке. Если он уже
                // в памяти — перемотка мгновенная.
                if !self.loaded {
                    return;
                }
                let pos = sec.max(0.0);
                // UI показывает цель сразу и держит её в sync(), пока seek не
                // применён: иначе ползунок отскакивает к старой позиции, а при
                // частых кликах — к целям очереди перемоток.
                self.seek_pending = Some(pos);
                self.sh().position = pos;
                (self.notify)();
                // В очереди уже более новая перемотка — не дёргаем звук к устаревшей цели.
                if self.latest_seek.load(Ordering::Relaxed) != pos.to_bits() {
                    return;
                }
                if let Some(shared) = self.current_shared.clone() {
                    let dur = self.sh().duration;
                    let len = shared.len;
                    if dur > 0.0 && len > 0 {
                        let byte = (len as f64 * (pos as f64 / dur as f64)) as u64;
                        if let Err(e) = shared.prime(byte) {
                            log(format!("seek: кусок не скачался: {e}"));
                        }
                    }
                }
                // Щелчок от разрыва волны гасим микро-фейдом: затухание, seek
                // при нулевом усилении, нарастание (SeekFade в try_seek).
                let fade = self.fade.clone();
                let can_fade = !self.paused && self.sink().map(|s| !s.empty()).unwrap_or(false);
                if can_fade {
                    fade.start_out();
                    for _ in 0..60 {
                        if fade.is_held() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                if let Some(s) = self.sink() {
                    if let Err(e) = s.try_seek(Duration::from_secs_f32(pos)) {
                        log(format!("seek: {e:?}"));
                    }
                }
                // Seek мог не примениться (пустой sink) — не оставляем тишину.
                if fade.is_held() {
                    fade.reset();
                }
                self.seek_pending = None;
            }
            Cmd::Volume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                if let Some(s) = self.sink() {
                    s.set_volume(self.volume * self.volume);
                }
            }
            Cmd::LikeTrack(t) => self.toggle_like(t),
            Cmd::Dislike => {
                if let (Some(api), Some(t)) = (self.api.clone(), self.current.as_ref()) {
                    let id = t.full_id();
                    let uid = self.uid.clone();
                    let tx = self.tx.clone();
                    let _ = self.bg.send(Box::new(move || {
                        if let Err(e) = api.dislike(&uid, &id) {
                            let _ = tx.send(Msg::Error(format!("Дизлайк не сохранился: {e}")));
                        }
                    }));
                }
                self.advance(End::Dislike);
            }
            Cmd::Wave(src) => self.start_wave(src, true),
            Cmd::PlayList { source, tracks, start, after } => {
                if tracks.is_empty() {
                    return;
                }
                if let (Some(cur), true) = (self.current.as_ref(), self.loaded) {
                    self.feedback("skip", Some(cur.full_id()), Some(self.pos() as f64));
                }
                self.clear_queue();
                let start = start.min(tracks.len() - 1);
                let mut order: Vec<Track> = tracks.iter().cloned().collect();
                order.rotate_left(start);
                let first = order.remove(0);
                if self.shuffle {
                    shuffle(&mut order);
                }
                self.queue.push_back(first);
                self.queue.extend(order);
                self.list = Some(ListPlay { all: tracks, after });
                self.set_source(source);
                self.queue_ver += 1;
                self.advance(End::Skip);
            }
            Cmd::Jump(i) => {
                if i >= self.queue.len() {
                    return;
                }
                let skipped: Vec<Track> = self.queue.drain(..i).collect();
                if let Some(cur) = self.current.take() {
                    self.history.push(cur);
                }
                self.history.extend(skipped);
                self.trim_history();
                self.stop_audio();
                self.play_from_queue();
            }
            Cmd::Repeat(r) => self.repeat = r,
            Cmd::Enqueue { track, next } => {
                if next {
                    self.queue.push_front(track);
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                } else {
                    self.queue.push_back(track);
                }
                self.queue_ver += 1;
            }
            Cmd::Shuffle(on) => {
                self.shuffle = on;
                if self.list.is_some() {
                    let mut v: Vec<Track> = self.queue.drain(..).collect();
                    if on {
                        shuffle(&mut v);
                    } else if let (Some(l), Some(cur)) = (&self.list, &self.current) {
                        let i = l.all.iter().position(|t| t.id == cur.id).map(|i| i + 1).unwrap_or(0);
                        v = l.all[i.min(l.all.len())..].to_vec();
                    }
                    self.queue.extend(v);
                    self.queue_ver += 1;
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                }
            }
        }
    }

    fn toggle_like(&mut self, t: Track) {
        let Some(api) = self.api.clone() else { return };
        let remove = {
            let mut l = self.likes.lock().unwrap();
            let had = l.remove(&t.id);
            if !had {
                l.insert(t.id.clone());
            }
            had
        };
        let (uid, id, tx) = (self.uid.clone(), t.full_id(), self.tx.clone());
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = api.like(&uid, &id, remove) {
                let _ = tx.send(Msg::Error(format!("Лайк не сохранился: {e}")));
            }
        }));
    }

    // ---------- очередь ----------

    fn start_wave(&mut self, src: Source, fetch: bool) {
        // Смена станции реагирует сразу, как оригинал: старый трек замолкает, UI
        // показывает новую станцию и загрузку, не дожидаясь ответа сервера.
        if let Some(cur) = self.current.take() {
            if self.loaded {
                self.feedback("skip", Some(cur.full_id()), Some(self.pos() as f64));
            }
            self.history.push(cur);
            self.trim_history();
        }
        self.stop_audio();
        // Фидбеки старой сессии — параллельно, чтобы не задерживать запрос новых треков.
        self.flush_feedback(true);
        self.clear_queue();
        self.list = None;
        self.set_source(src);
        {
            let mut s = self.sh();
            s.track = None;
            s.loading = true;
        }
        self.feedback("radioStarted", None, None);
        if fetch {
            self.fetch_batch_with(true);
        }
    }

    fn fetch_batch(&mut self) {
        self.fetch_batch_with(false);
    }

    fn fetch_batch_with(&mut self, replace: bool) {
        if self.fetching || self.list.is_some() {
            return;
        }
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        self.fetching = true;
        self.retry_at = None;
        // Недавние треки (как queue оригинала): последние из истории, текущий и очередь — «id:альбом».
        let queue: Vec<String> = self
            .history
            .iter()
            .rev()
            .take(5)
            .rev()
            .chain(self.current.iter())
            .chain(self.queue.iter())
            .map(|t| t.full_id())
            .collect();
        let session = self.session.clone();
        let feedbacks = std::mem::take(&mut self.deferred);
        let tx = self.tx.clone();
        let gen = self.batch_gen;
        let job = move || {
            let res = api.wave_session(&station, Some(&session), &queue, &feedbacks);
            let _ = tx.send(Msg::Batch { res, replace, gen });
        };
        if replace {
            // Смена станции: запрос уходит сразу, не дожидаясь очереди фидбеков.
            thread::spawn(job);
        } else {
            // В очереди фонового потока: запрос уйдёт после уже отправленных фидбеков.
            let _ = self.bg.send(Box::new(job));
        }
    }

    fn on_batch(&mut self, b: Batch, replace: bool) {
        self.batch_id = b.batch_id;
        if !b.session.is_empty() && b.session != self.session {
            self.session = b.session;
            if std::mem::take(&mut self.radio_started_pending) {
                self.feedback("radioStarted", None, None);
            }
        }
        self.likes.lock().unwrap().extend(b.liked);
        if replace {
            self.queue.clear();
            self.prefetch = None;
        }
        let queued = |t: &Track| {
            self.queue.iter().any(|q| q.id == t.id) || (!replace && self.current.as_ref().is_some_and(|c| c.id == t.id))
        };
        // Сервер иногда возвращает недавно сыгранные треки — их пропускаем. Если весь пакет
        // из повторов, просим ещё (до двух раз подряд), а потом берём как есть, чтобы не замолчать.
        let check_history = !replace || self.source == Source::Wave;
        let skip = |t: &Track| queued(t) || (check_history && self.history.iter().any(|h| h.id == t.id));
        let fresh: Vec<Track> = b.tracks.iter().filter(|t| !skip(t)).cloned().collect();
        let tracks = if !fresh.is_empty() || b.tracks.iter().all(&queued) {
            fresh
        } else if self.stale_batches < 2 {
            self.stale_batches += 1;
            self.fetch_batch_with(replace);
            return;
        } else {
            b.tracks.iter().filter(|t| !queued(t)).cloned().collect()
        };
        self.stale_batches = 0;
        self.queue.extend(tracks);
        self.queue_ver += 1;
        if replace {
            self.advance(End::Skip);
        } else if self.current.is_none() {
            self.play_from_queue();
        } else if self.loaded {
            self.prefetch_next();
        }
    }

    fn trim_history(&mut self) {
        if self.history.len() > 50 {
            let n = self.history.len() - 50;
            self.history.drain(..n);
        }
    }

    fn advance(&mut self, end: End) {
        if let Some(cur) = self.current.take() {
            if self.loaded {
                let played = self.pos() as f64;
                let kind = match end {
                    End::Finished => "trackFinished",
                    End::Skip => "skip",
                    End::Dislike => "dislike",
                };
                self.feedback(kind, Some(cur.full_id()), Some(played));
            }
            self.history.push(cur);
            self.trim_history();
        }
        self.stop_audio();
        self.play_from_queue();
        self.flush_feedback(false);
    }

    /// Список закончился: по кругу (исполнитель) или волна по нему (плейлист, альбом).
    fn list_finished(&mut self) {
        let Some(l) = self.list.take() else { return };
        match l.after {
            After::Loop => {
                if self.shuffle {
                    let mut all = (*l.all).clone();
                    shuffle(&mut all);
                    self.queue.extend(all);
                } else {
                    self.queue.extend(l.all.iter().cloned());
                }
                self.queue_ver += 1;
                self.list = Some(l);
            }
            After::Wave(src) => {
                self.flush_feedback(false);
                self.set_source(src);
                self.session.clear();
                self.feedback("radioStarted", None, None);
            }
        }
    }

    fn play_from_queue(&mut self) {
        if self.queue.is_empty() && self.list.is_some() {
            match (&self.list, self.repeat) {
                (Some(l), Repeat::All) => {
                    self.queue.extend(l.all.iter().cloned());
                }
                _ => self.list_finished(),
            }
        }
        match self.queue.pop_front() {
            Some(t) => {
                self.current = Some(t);
                self.start_current();
            }
            None => {
                self.current = None;
                self.current_url = None;
                self.gen += 1;
                let mut s = self.sh();
                s.track = None;
                s.loading = true;
            }
        }
        self.queue_ver += 1;
        if self.queue.len() < 2 {
            self.fetch_batch();
        }
    }

    fn prev(&mut self) {
        if self.loaded && self.pos() > 4.0 {
            if let Some(s) = self.sink() {
                let _ = s.try_seek(Duration::ZERO);
            }
            return;
        }
        if let Some(p) = self.history.pop() {
            if let Some(c) = self.current.take() {
                self.queue.push_front(c);
                self.queue_ver += 1;
            }
            self.stop_audio();
            self.current = Some(p);
            self.start_current();
        } else if self.loaded {
            if let Some(s) = self.sink() {
                let _ = s.try_seek(Duration::ZERO);
            }
        }
    }

    // ---------- загрузка ----------

    fn start_current(&mut self) {
        self.gen += 1;
        self.loaded = false;
        self.current_url = None;
        self.seek_pending = None;
        let Some(t) = self.current.clone() else { return };
        {
            let mut s = self.sh();
            s.track = Some(t.clone());
            s.position = 0.0;
            s.duration = t.duration_ms as f32 / 1000.0;
            s.loading = true;
        }
        if self.prefetch.as_ref().map(|(id, _)| *id == t.id).unwrap_or(false) {
            let (_, b) = self.prefetch.take().unwrap();
            self.play_loaded(b);
            return;
        }
        if self.prefetching.as_deref() == Some(t.id.as_str()) {
            return;
        }
        self.start_download();
    }

    fn start_download(&mut self) {
        let (Some(api), Some(t)) = (self.api.clone(), self.current.clone()) else { return };
        let gen = self.gen;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = api.open_track_stream(&t);
            let _ = tx.send(Msg::Audio { gen, res });
        });
    }

    fn prefetch_next(&mut self) {
        // Следующий трек открываем под конец текущего: в памяти только первый кусок.
        let near_end = {
            let s = self.shared.lock().unwrap();
            s.duration > 0.0 && s.duration - s.position <= 45.0
        };
        if !near_end {
            return;
        }
        let Some(next) = self.queue.front().cloned() else { return };
        let have = self.prefetch.as_ref().map(|(id, _)| *id == next.id).unwrap_or(false);
        if have || self.prefetching.is_some() {
            return;
        }
        let Some(api) = self.api.clone() else { return };
        self.prefetching = Some(next.id.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = api.open_track_stream(&next);
            let _ = tx.send(Msg::Prefetch { id: next.id, res });
        });
    }

    fn play_loaded(&mut self, stream: HttpStream) {
        let Some(t) = self.current.clone() else { return };
        if !self.ensure_audio() {
            self.error("Не найдено аудиоустройство");
            self.sh().loading = false;
            return;
        }
        let url = stream.url().to_string();
        let shared = stream.shared();
        let len = shared.len;
        // Coarse seek: прыжок к цели вместо покадрового скана всего промежутка
        // (Accurate в rodio 0.20 качал его прямо в аудиоколбэке — щелчки и секунды
        // ожидания). byte_len нужен и для точной длительности MP3.
        let src = match Decoder::builder().with_data(stream).with_byte_len(len).with_coarse_seek(true).build() {
            Ok(s) => s,
            Err(e) => {
                self.on_load_error(format!("не удалось декодировать: {e}"));
                return;
            }
        };
        self.fade.reset();
        self.seek_pending = None;
        let src = SeekFade::new(Eq::new(src, self.eq.clone()), self.fade.clone());
        let vol = self.volume * self.volume;
        let paused = self.paused;
        if let Some(sink) = self.sink() {
            sink.clear();
            sink.set_volume(vol);
            sink.append(src);
            if paused {
                sink.pause()
            } else {
                sink.play()
            }
        }
        self.current_url = Some(url);
        self.current_shared = Some(shared);
        self.loaded = true;
        self.errors_in_row = 0;
        self.sh().loading = false;
        self.feedback("trackStarted", Some(t.full_id()), None);
        self.prefetch_next();
    }

    fn on_load_error(&mut self, e: String) {
        self.errors_in_row += 1;
        let name = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
        self.error(format!("«{name}» не загрузился: {e}"));
        if self.errors_in_row < 5 {
            self.current = None;
            self.stop_audio();
            self.play_from_queue();
        } else {
            self.sh().loading = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source as _;

    /// Сетевой тест: ссылка на поток → потоковое чтение → декодирование (без токена — 30-сек превью).
    #[test]
    #[ignore]
    fn stream_and_decode() {
        let api = Api::new("");
        let url = api.stream_url("57703").expect("stream url");
        let stream = api.reopen_stream(&url).expect("open stream");
        let len = stream.shared().len;
        let dec = Decoder::builder().with_data(stream).with_byte_len(len).build().expect("decode");
        let samples = dec.take(44100).count();
        assert!(samples > 1000);
    }

    /// Регрессия на корневую причину щелчков и тормозов при перемотке:
    /// rodio 0.20 использовал SeekMode::Accurate, и symphonia парсила каждый
    /// MP3-кадр от текущей позиции до цели, качая все куски промежутка прямо
    /// в аудиоколбэке. Coarse seek прыгает сразу к цели — после предзагрузки
    /// целевого куска перемотка не должна делать ни одного сетевого запроса.
    #[test]
    fn coarse_seek_does_not_stream_the_gap() {
        use std::sync::atomic::Ordering;
        // 4000 тихих MPEG1-кадров (~105 с, 1.6 МБ) — несколько кусков HttpStream.
        // Заголовок FF FB 90 C0: MPEG1 Layer3, 128 кбит/с, 44.1 кГц, моно;
        // нулевые side info и main data декодируются в тишину.
        let mut data = Vec::new();
        for _ in 0..4000 {
            let mut frame = vec![0u8; 417];
            frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
            data.extend_from_slice(&frame);
        }
        let (url, hits) = crate::stream::tests::serve(data);
        let stream = HttpStream::open(reqwest::blocking::Client::new(), url).expect("open");
        let shared = stream.shared();
        let len = shared.len;
        let mut dec = Decoder::builder().with_data(stream).with_byte_len(len).with_coarse_seek(true).build().expect("decoder");
        let dur = dec.total_duration().expect("duration").as_secs_f32();
        assert!(dur > 100.0, "синтетический mp3 не распознан: {dur}");
        let _ = dec.by_ref().take(4410).count(); // 0.1 с с начала
        let before = hits.load(Ordering::SeqCst);

        // Перемотка вперёд, в самый конец: целевой кусок докачан заранее,
        // как это делает движок в Cmd::Seek. Сам seek — ни одного запроса.
        let target = dur * 0.9;
        shared.prime((len as f64 * target as f64 / dur as f64) as u64).expect("prime");
        dec.try_seek(Duration::from_secs_f32(target)).expect("seek");
        let played = dec.by_ref().take(44100).count();
        assert!(played > 40000, "после seek звук не пошёл");
        assert_eq!(hits.load(Ordering::SeqCst), before + 1, "seek скачал промежуток (Accurate-скан?)");

        // Обратная перемотка — тоже без скачивания промежутка.
        let back = dur * 0.2;
        shared.prime((len as f64 * back as f64 / dur as f64) as u64).expect("prime");
        dec.try_seek(Duration::from_secs_f32(back)).expect("seek back");
        let _ = dec.by_ref().take(4410).count();
        assert_eq!(hits.load(Ordering::SeqCst), before + 2, "обратный seek скачал промежуток");
    }
}
