//! Микро-фейд на стыке перемотки: затухание до нуля, seek, нарастание.
//! Убирает щелчок от разрыва волны при скачке позиции. Библиотечные
//! fadein/fadeout rodio не умеют срабатывать по seek, поэтому обёртка
//! с состоянием, общим между движком (запускает затухание) и аудиопотоком
//! (ведёт рампу и нарастание после успешного seek).

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Длительность каждого фейда: 10 мс достаточно, чтобы скрыть стык,
/// и достаточно коротко, чтобы не ощущалось как задержка.
const FADE: Duration = Duration::from_millis(10);

const IDLE: u8 = 0;
const OUT: u8 = 1;
const IN: u8 = 2;
const HELD: u8 = 3;

pub struct FadeState {
    mode: AtomicU8,
    gain: AtomicU32,
}

impl FadeState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { mode: AtomicU8::new(IDLE), gain: AtomicU32::new(1.0f32.to_bits()) })
    }

    /// Начать затухание (движок, перед перемоткой).
    pub fn start_out(&self) {
        self.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.mode.store(OUT, Ordering::Relaxed);
    }

    /// Волна затухла до нуля — перемотка пройдёт без щелчка.
    pub fn is_held(&self) -> bool {
        self.mode.load(Ordering::Relaxed) == HELD
    }

    /// Обычный звук без фейда (новый трек, неудачная перемотка).
    pub fn reset(&self) {
        self.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.mode.store(IDLE, Ordering::Relaxed);
    }

    fn start_in(&self) {
        self.gain.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.mode.store(IN, Ordering::Relaxed);
    }
}

pub struct SeekFade<S> {
    inner: S,
    state: Arc<FadeState>,
    ch: usize,
}

impl<S: Source> SeekFade<S> {
    pub fn new(inner: S, state: Arc<FadeState>) -> Self {
        Self { inner, state, ch: 0 }
    }
}

impl<S: Source> Iterator for SeekFade<S> {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let s = self.inner.next()?;
        let mut mode = self.state.mode.load(Ordering::Relaxed);
        if mode == IDLE {
            return Some(s);
        }
        let mut g = f32::from_bits(self.state.gain.load(Ordering::Relaxed));
        // Усиление меняем раз в кадр, чтобы каналы фейдились синхронно.
        if self.ch == 0 {
            let step = 1.0 / (FADE.as_secs_f32() * self.inner.sample_rate().get() as f32);
            match mode {
                OUT => {
                    g -= step;
                    if g <= 0.0 {
                        g = 0.0;
                        mode = HELD;
                    }
                }
                IN => {
                    g += step;
                    if g >= 1.0 {
                        g = 1.0;
                        mode = IDLE;
                    }
                }
                _ => {}
            }
            self.state.gain.store(g.to_bits(), Ordering::Relaxed);
            self.state.mode.store(mode, Ordering::Relaxed);
        }
        self.ch = (self.ch + 1) % self.inner.channels().get() as usize;
        Some(s * g)
    }
}

impl<S: Source> Source for SeekFade<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let res = self.inner.try_seek(pos);
        if res.is_ok() {
            // Стык прошёл при нулевом усилении — плавно нарастаем.
            self.state.start_in();
            self.ch = 0;
        } else {
            // Перемотка не удалась: старый звук продолжается без фейда.
            self.state.reset();
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;

    /// Бесконечный источник единиц: удобно проверять огибающую фейда.
    struct Tone;

    impl Iterator for Tone {
        type Item = Sample;
        fn next(&mut self) -> Option<Self::Item> {
            Some(1.0)
        }
    }

    impl Source for Tone {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> ChannelCount {
            NonZero::new(1).unwrap()
        }
        fn sample_rate(&self) -> SampleRate {
            NonZero::new(44100).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
        fn try_seek(&mut self, _: Duration) -> Result<(), SeekError> {
            Ok(())
        }
    }

    #[test]
    fn fade_out_holds_zero_then_seek_fades_in() {
        let state = FadeState::new();
        let mut f = SeekFade::new(Tone, state.clone());
        assert_eq!(f.next(), Some(1.0));

        state.start_out();
        let mut last = 1.0;
        for _ in 0..500 {
            let s = f.next().unwrap();
            assert!(s <= last + 1e-6, "затухание немонотонно: {s} > {last}");
            last = s;
        }
        assert!(state.is_held(), "фейд не дошёл до нуля");
        assert_eq!(f.next(), Some(0.0));

        f.try_seek(Duration::ZERO).unwrap();
        let mut last = 0.0;
        for _ in 0..500 {
            let s = f.next().unwrap();
            assert!(s >= last - 1e-6, "нарастание немонотонно: {s} < {last}");
            last = s;
        }
        assert_eq!(f.next(), Some(1.0));
    }

    #[test]
    fn failed_seek_restores_unity_gain() {
        struct NoSeek;
        impl Iterator for NoSeek {
            type Item = Sample;
            fn next(&mut self) -> Option<Self::Item> {
                Some(1.0)
            }
        }
        impl Source for NoSeek {
            fn current_span_len(&self) -> Option<usize> {
                None
            }
            fn channels(&self) -> ChannelCount {
                NonZero::new(1).unwrap()
            }
            fn sample_rate(&self) -> SampleRate {
                NonZero::new(44100).unwrap()
            }
            fn total_duration(&self) -> Option<Duration> {
                None
            }
        }

        let state = FadeState::new();
        let mut f = SeekFade::new(NoSeek, state.clone());
        state.start_out();
        assert!(f.try_seek(Duration::ZERO).is_err());
        assert_eq!(f.next(), Some(1.0), "после неудачного seek звук должен продолжиться без фейда");
    }
}
