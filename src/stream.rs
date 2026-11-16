//! Потоковый HTTP-источник для rodio: Range-запросы вместо полной загрузки файла.
//! Веб-плеер оригинала начинает играть после первого буфера, а не ждёт весь mp3 —
//! здесь то же: первый кусок 256 КБ, дальше докачка по мере чтения. Поддерживает
//! Seek (перемотка шлёт отдельный Range-запрос), поэтому годится для symphonia.
//! Если сервер не умеет Range (ответ 200 на запрос куска) — забираем весь файл сразу.
//!
//! Чтение с аудиопотока не должно блокироваться сетью, иначе будут заикания.
//! Поэтому (1) следующий кусок качается заранее в фоновом потоке, а (2) перед
//! перемоткой движок заранее скачивает целевой кусок в слот `primed` — тогда
//! `Sink::try_seek`, который rodio применяет прямо в аудиоколбэке, читает из памяти.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// Размер куска: хватает на несколько секунд звука, качается быстро.
const CHUNK: u64 = 256 * 1024;
/// Кусок предзагрузки для перемотки — вдвое меньше CHUNK: холодная перемотка
/// ждёт меньше сети, а дальше обычный префетч (AHEAD) продолжает докачку.
const PRIME: u64 = 128 * 1024;
/// За сколько до конца куска начинаем качать следующий (~2.7 с звука при 192 кбит/с).
const AHEAD: u64 = 64 * 1024;

/// Кусок файла: с какой позиции и байты.
struct Chunk {
    start: u64,
    bytes: Vec<u8>,
}

type AheadChunk = (u64, Mutex<Receiver<(u64, Result<Vec<u8>, String>)>>);

/// Общее состояние потока, доступное движку: предзагрузка куска перед перемоткой.
/// Поля, кроме `len`, снаружи не нужны — движок зовёт только [`Shared::prime`].
pub struct Shared {
    client: reqwest::blocking::Client,
    url: String,
    pub len: u64,
    ranged: bool,
    chunk: Mutex<Chunk>,
    /// Кусок, заранее скачанный для перемотки: `read` заберёт его вместо сети.
    primed: Mutex<Option<Chunk>>,
}

fn fetch_range(client: &reqwest::blocking::Client, url: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .map_err(|e| format!("сеть: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(|e| format!("сеть: {e}"))
}

impl Shared {
    /// Обеспечить кусок, покрывающий байт `byte` (для перемотки). Если данные уже
    /// в памяти — ничего не качаем, и `sink.try_seek` применится сразу; иначе
    /// докачиваем целевой кусок в слот предзагрузки.
    pub fn prime(&self, byte: u64) -> Result<(), String> {
        if !self.ranged || self.len == 0 {
            return Ok(());
        }
        let byte = byte.min(self.len - 1);
        // Coarse seek приземляется на ~2.9 КБ раньше линейной оценки (запас на
        // кадр) и читает несколько кадров вперёд — берём запас с обеих сторон.
        let start = byte.saturating_sub(8192);
        let need_end = (byte + 8192).min(self.len);
        let (cstart, cend) = self.chunk_bounds();
        if start >= cstart && need_end <= cend {
            return Ok(());
        }
        if let Some(c) = self.primed.lock().unwrap().as_ref() {
            if start >= c.start && need_end <= c.start + c.bytes.len() as u64 {
                return Ok(());
            }
        }
        let end = (start + PRIME - 1).min(self.len - 1);
        let bytes = fetch_range(&self.client, &self.url, start, end)?;
        *self.primed.lock().unwrap() = Some(Chunk { start, bytes });
        Ok(())
    }

    /// Забрать предзагруженный кусок, если он покрывает `pos`.
    fn take_primed(&self, pos: u64) -> Option<Chunk> {
        let mut p = self.primed.lock().unwrap();
        match p.as_ref() {
            Some(c) if pos >= c.start && pos < c.start + c.bytes.len() as u64 => p.take(),
            _ => None,
        }
    }

    fn set_chunk(&self, chunk: Chunk) {
        *self.chunk.lock().unwrap() = chunk;
    }

    /// Начало и конец текущего куска.
    fn chunk_bounds(&self) -> (u64, u64) {
        let c = self.chunk.lock().unwrap();
        (c.start, c.start + c.bytes.len() as u64)
    }

    fn fetch_chunk(&self, start: u64) -> Result<(), String> {
        if !self.ranged {
            return Ok(());
        }
        let end = (start + CHUNK - 1).min(self.len.saturating_sub(1));
        let bytes = fetch_range(&self.client, &self.url, start, end)?;
        self.set_chunk(Chunk { start, bytes });
        Ok(())
    }
}

pub struct HttpStream {
    shared: Arc<Shared>,
    pos: u64,
    /// Заранее качаемый следующий кусок: (начало, результат из фонового потока).
    /// Mutex нужен, чтобы источник был Sync (требование rodio).
    ahead: Option<AheadChunk>,
}

impl HttpStream {
    pub fn open(client: reqwest::blocking::Client, url: String) -> Result<Self, String> {
        let resp = client
            .get(&url)
            .header("Range", format!("bytes=0-{}", CHUNK - 1))
            .send()
            .map_err(|e| format!("сеть: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let ranged = status == reqwest::StatusCode::PARTIAL_CONTENT;
        let len = if ranged {
            // Content-Range: bytes 0-262143/13252440 — полная длина после слэша.
            resp.headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.rsplit('/').next())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("нет длины в Content-Range")?
        } else {
            resp.content_length().unwrap_or(0)
        };
        let bytes = resp.bytes().map_err(|e| format!("сеть: {e}"))?.to_vec();
        let len = if ranged { len } else { bytes.len() as u64 };
        let shared = Arc::new(Shared {
            client,
            url,
            len,
            ranged,
            chunk: Mutex::new(Chunk { start: 0, bytes }),
            primed: Mutex::new(None),
        });
        Ok(HttpStream { shared, pos: 0, ahead: None })
    }

    pub fn url(&self) -> &str {
        &self.shared.url
    }

    /// Состояние для предзагрузки куска перед перемоткой.
    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Запустить фоновую докачку следующего куска, когда подходим к концу текущего.
    fn maybe_start_ahead(&mut self) {
        if !self.shared.ranged || self.ahead.is_some() {
            return;
        }
        let (_, end) = self.shared.chunk_bounds();
        if end >= self.shared.len || self.pos + AHEAD < end {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let client = self.shared.client.clone();
        let url = self.shared.url.clone();
        let len = self.shared.len;
        std::thread::spawn(move || {
            let stop = (end + CHUNK - 1).min(len - 1);
            let res = fetch_range(&client, &url, end, stop);
            let _ = tx.send((end, res));
        });
        self.ahead = Some((end, Mutex::new(rx)));
    }

    /// Взять заранее скачанный кусок, если он начинается там, где нужно.
    fn take_ahead(&mut self, want_start: u64) -> Option<Vec<u8>> {
        let (start, rx) = self.ahead.take()?;
        if start != want_start {
            return None;
        }
        let rx = rx.into_inner().ok()?;
        match rx.recv() {
            Ok((_, Ok(bytes))) => Some(bytes),
            _ => None,
        }
    }

    /// Обеспечить кусок, покрывающий `pos`, когда текущий его не покрывает:
    /// заранее скачанный следующий (только при непрерывном чтении), затем
    /// предзагруженный для перемотки (в любую сторону), иначе — сетевой запрос.
    fn ensure_chunk(&mut self) -> io::Result<()> {
        let (_, cend) = self.shared.chunk_bounds();
        if self.pos == cend {
            if let Some(bytes) = self.take_ahead(cend) {
                self.shared.set_chunk(Chunk { start: cend, bytes });
                return Ok(());
            }
        }
        if let Some(chunk) = self.shared.take_primed(self.pos) {
            self.shared.set_chunk(chunk);
            return Ok(());
        }
        self.ahead = None;
        self.shared.fetch_chunk(self.pos).map_err(io::Error::other)
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.shared.len || buf.is_empty() {
            return Ok(0);
        }
        let (cstart, cend) = self.shared.chunk_bounds();
        if self.pos >= cend || self.pos < cstart {
            self.ensure_chunk()?;
        }
        let n = {
            let c = self.shared.chunk.lock().unwrap();
            let off = (self.pos - c.start) as usize;
            if off >= c.bytes.len() {
                return Ok(0);
            }
            let avail = &c.bytes[off..];
            let n = buf.len().min(avail.len()).min((self.shared.len - self.pos) as usize);
            buf[..n].copy_from_slice(&avail[..n]);
            n
        };
        self.pos += n as u64;
        self.maybe_start_ahead();
        Ok(n)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, s: SeekFrom) -> io::Result<u64> {
        let new = match s {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.shared.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek до начала файла"));
        }
        let new = new as u64;
        if new != self.pos {
            // Префетч старого региона после перемотки устарел: пока он Some,
            // maybe_start_ahead не качает новый регион, и на границе primed-куска
            // fetch уходит в аудиопоток (underrun и клик через пару секунд после seek).
            self.ahead = None;
        }
        self.pos = new;
        Ok(self.pos)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::net::TcpListener;

    /// Детерминированные байты длиной `len`.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Мини-сервер с поддержкой Range: 206 + Content-Range, как CDN Яндекса.
    /// Слушает 127.0.0.1 — внешняя сеть не нужна. Возвращает URL и счётчик запросов.
    pub(crate) fn serve(data: Vec<u8>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/track", listener.local_addr().expect("addr"));
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let Ok(mut reader) = sock.try_clone().map(BufReader::new) else { continue };
                let (mut start, mut end) = (0usize, data.len().saturating_sub(1));
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let l = line.trim_end().to_ascii_lowercase();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(r) = l.strip_prefix("range: bytes=") {
                        let mut it = r.split('-');
                        start = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                        end = it.next().and_then(|v| v.parse().ok()).unwrap_or(data.len() - 1);
                    }
                }
                let end = end.min(data.len() - 1);
                let body = &data[start..=end];
                let _ = write!(
                    sock,
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n",
                    total = data.len(),
                    n = body.len()
                );
                let _ = sock.write_all(body);
                let _ = sock.flush();
            }
        });
        (url, hits)
    }

    /// Перемотка вперёд, когда следующий кусок уже качается заранее, не должна
    /// давать EOF: раньше read возвращал Ok(0) → трек обрывался.
    #[test]
    fn seek_forward_past_pending_prefetch_keeps_reading() {
        let len = 2 * 1024 * 1024 + 300_000;
        let data = pattern(len);
        let (url, _hits) = serve(data.clone());
        let client = reqwest::blocking::Client::new();
        let mut s = HttpStream::open(client, url).expect("open");

        // Читаем до срабатывания префетча (за AHEAD до конца первого куска).
        let mut buf = [0u8; 8192];
        let mut read = 0usize;
        while read < (CHUNK - AHEAD / 2) as usize {
            let n = s.read(&mut buf).expect("read");
            assert!(n > 0, "неожиданный EOF при последовательном чтении");
            assert_eq!(&buf[..n], &data[read..read + n]);
            read += n;
        }

        // Перемотка за пределы куска, который качается заранее ([CHUNK, 2*CHUNK)).
        let target = 2 * CHUNK + 1000;
        s.seek(SeekFrom::Start(target)).expect("seek");
        let n = s.read(&mut buf).expect("read after seek");
        assert!(n > 0, "после перемотки чтение вернуло EOF");
        assert_eq!(&buf[..n], &data[target as usize..target as usize + n]);
    }

    /// Ключевой инвариант перемотки: если движок заранее скачал целевой кусок
    /// (`prime`), повторный seek в него не должен идти в сеть — иначе финальный
    /// `Sink::try_seek` на аудиопотоке снова качал бы данные.
    #[test]
    fn primed_chunk_is_served_without_refetch() {        use std::sync::atomic::Ordering;
        let len = 2 * 1024 * 1024 + 300_000;
        let data = pattern(len);
        let (url, hits) = serve(data.clone());
        let client = reqwest::blocking::Client::new();
        let mut s = HttpStream::open(client, url).expect("open");
        let mut buf = [0u8; 4096];

        let at = 700_000u64;
        s.shared().prime(at).expect("prime");
        let before = hits.load(Ordering::SeqCst);

        s.seek(SeekFrom::Start(at)).expect("seek");
        let n = s.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], &data[at as usize..at as usize + n]);
        assert_eq!(hits.load(Ordering::SeqCst), before, "предзагруженный кусок не использован — сходили в сеть");
    }

    /// Регрессия на клики после перемотки: префетч старого региона (ahead)
    /// не должен блокировать префетч нового. Иначе на границе primed-куска
    /// (через пару секунд после seek) fetch уходит в аудиопоток → underrun/клик.
    #[test]
    fn seek_invalidates_stale_prefetch() {
        use std::sync::atomic::Ordering;
        let len = (4 * CHUNK + 100_000) as usize;
        let data = pattern(len);
        let (url, hits) = serve(data.clone());
        let client = reqwest::blocking::Client::new();
        let mut s = HttpStream::open(client, url).expect("open");
        let shared = s.shared();
        let mut buf = [0u8; 8192];
        // Префетч асинхронный: ждём счётчик, а не спим фиксированно.
        let wait = |want: usize| {
            let t0 = std::time::Instant::now();
            while hits.load(Ordering::SeqCst) < want && t0.elapsed().as_secs() < 5 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };

        // Последовательное воспроизведение до срабатывания префетча следующего куска.
        let mut read = 0usize;
        while read < (CHUNK - AHEAD / 2) as usize {
            let n = s.read(&mut buf).expect("read");
            read += n;
        }
        wait(2);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "префетч следующего куска не стартовал");

        // Перемотка как в движке: целевой кусок предзагружен, позиция переставлена.
        let target = 3 * CHUNK + 5000;
        shared.prime(target).expect("prime");
        s.seek(SeekFrom::Start(target)).expect("seek");
        let after_prime = hits.load(Ordering::SeqCst);

        // Дочитываем до окна префетча нового региона: докачка должна стартовать
        // здесь (в фоне), а не на границе куска в аудиопотоке.
        let window = target - 8192 + PRIME - AHEAD + 1000;
        while s.stream_position().expect("pos") < window {
            assert!(s.read(&mut buf).expect("read") > 0);
        }
        wait(after_prime + 1);
        assert_eq!(hits.load(Ordering::SeqCst), after_prime + 1, "устаревший ahead заблокировал префетч нового региона");

        // Граница куска обслуживается префетчем: сетевых чтений на read-пути нет.
        let end = target - 8192 + PRIME;
        while s.stream_position().expect("pos") < end + 4096 {
            assert!(s.read(&mut buf).expect("read") > 0);
        }
        assert_eq!(hits.load(Ordering::SeqCst), after_prime + 1, "fetch на границе куска в read-пути");
    }
}
