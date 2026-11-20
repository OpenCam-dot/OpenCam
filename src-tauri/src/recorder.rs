//! 녹화 스레드. FPS 간격으로 화면을 받아 프레임 목록에 쌓는다.
//!
//! - "화면 변화 없으면 프레임 합치기": 새 화면이 직전과 완전히 같으면 직전 프레임 시간만 늘린다
//! - 프레임 시간은 실제로 흐른 시간으로 정한다(캡처가 밀려도 재생 속도가 실제와 같게)

use crate::audio::{self, AudioCapture};
use crate::capture::{Capturer, CursorOptions, Rect};
use crate::export::Mp4Stream;
use crate::frame::Frame;
use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct RecordOptions {
    pub region: Rect,
    pub fps: u32,
    pub skip_same_frame: bool,
    pub max_ms: Option<u64>,
    pub max_bytes: usize,
    pub cursor: CursorOptions,
    /// 녹화하면서 MP4로 바로 저장
    pub direct: Option<Mp4Stream>,
    /// 바로 저장하는 MP4에 넣을 PC 소리 (direct와 함께 준다)
    pub audio: Option<AudioCapture>,
    /// 바로 저장 중 메모리 한도에 닿으면 편집용 프레임만 그만 모으고 녹화는 계속
    pub drop_frames_after_limit: bool,
}

/// 녹화 결과
pub struct RecordResult {
    pub frames: Vec<Frame>,
    /// MP4 바로 저장 결과 (저장한 파일 또는 오류)
    pub direct: Option<Result<PathBuf, String>>,
    /// 소리를 넣다가 생긴 문제 (MP4는 저장됐지만 소리가 없거나 일부만 있음)
    pub audio_warning: Option<String>,
    /// 메모리 한도 때문에 편집용 프레임을 중간부터 버렸으면 true
    pub dropped: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecStatus {
    pub recording: bool,
    pub paused: bool,
    pub frames: usize,
    pub elapsed_ms: u64,
    pub memory_mb: u64,
    pub stop_reason: Option<String>,
}

struct Shared {
    frames: Mutex<Vec<Frame>>,
    latest: Mutex<Option<(u32, u32, Arc<Vec<u8>>)>>,
    status: Mutex<RecStatus>,
    stop: AtomicBool,
    paused: AtomicBool,
    region: Mutex<Option<Rect>>,
    direct_result: Mutex<Option<Result<PathBuf, String>>>,
    audio_warning: Mutex<Option<String>>,
    dropped: AtomicBool,
}

pub struct Recorder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start(opts: RecordOptions, on_status: impl Fn(RecStatus) + Send + 'static) -> anyhow::Result<Self> {
        // 캡처 준비가 실패하면 바로 알려준다
        let capturer = Capturer::new(opts.region)?;
        Self::spawn(capturer, opts, on_status)
    }

    fn spawn(
        screen: impl Screen + Send + 'static,
        opts: RecordOptions,
        on_status: impl Fn(RecStatus) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            frames: Mutex::new(vec![]),
            latest: Mutex::new(None),
            status: Mutex::new(RecStatus {
                recording: true,
                paused: false,
                frames: 0,
                elapsed_ms: 0,
                memory_mb: 0,
                stop_reason: None,
            }),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            region: Mutex::new(None),
            direct_result: Mutex::new(None),
            audio_warning: Mutex::new(None),
            dropped: AtomicBool::new(false),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("opencam-recorder".into())
            .spawn(move || run(screen, opts, s, on_status))?;
        Ok(Recorder { shared, thread: Some(thread) })
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    /// 녹화 중 녹화 틀이 움직였을 때
    pub fn move_region(&self, region: Rect) {
        *self.shared.region.lock() = Some(region);
    }

    pub fn status(&self) -> RecStatus {
        self.shared.status.lock().clone()
    }

    /// 편집 프레임 수집을 중단했더라도 마지막으로 캡처한 화면을 반환한다.
    pub fn latest_image(&self) -> Option<(u32, u32, Arc<Vec<u8>>)> {
        self.shared.latest.lock().clone()
    }

    /// 녹화를 끝내고 결과를 돌려준다
    pub fn stop(mut self) -> RecordResult {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        RecordResult {
            frames: std::mem::take(&mut *self.shared.frames.lock()),
            direct: self.shared.direct_result.lock().take(),
            audio_warning: self.shared.audio_warning.lock().take(),
            dropped: self.shared.dropped.load(Ordering::SeqCst),
        }
    }
}

/// 녹화할 화면. 실제로는 `Capturer`이고, 테스트에서는 가짜 화면을 넣는다.
trait Screen {
    fn region(&self) -> Rect;
    fn set_region(&mut self, region: Rect) -> anyhow::Result<()>;
    fn grab(&mut self, cursor: &CursorOptions) -> anyhow::Result<Vec<u8>>;
}

impl Screen for Capturer {
    fn region(&self) -> Rect {
        Capturer::region(self)
    }
    fn set_region(&mut self, region: Rect) -> anyhow::Result<()> {
        Capturer::set_region(self, region)
    }
    fn grab(&mut self, cursor: &CursorOptions) -> anyhow::Result<Vec<u8>> {
        Capturer::grab(self, cursor)
    }
}

/// 옮겨 갈 영역 to의 크기를 지금 영역 now의 크기로 맞춘다 (가운데 자리는 그대로)
fn keep_size(to: Rect, now: Rect) -> Rect {
    if to.w == now.w && to.h == now.h {
        return to;
    }
    Rect { x: to.x + (to.w - now.w) / 2, y: to.y + (to.h - now.h) / 2, w: now.w, h: now.h }
}

/// 직전 프레임이 화면에 보인 시간만큼 delay를 늘린다
fn extend_last(s: &Shared, dt: Duration, carry: &mut Duration) {
    if let Some(prev) = s.frames.lock().last_mut() {
        add_time(&mut prev.delay_ms, dt, carry);
    }
}

/// 흐른 시간 dt를 1ms 단위인 delay에 더한다. 1ms가 안 되는 자투리는 버리지 않고 carry에 남겨 다음에 더한다.
/// 캡처할 때마다 자투리를 버리면 60FPS(한 칸 16.7ms)에서 16ms씩만 쌓여, 편집 창의 녹화가 실제보다 3~4% 짧아진다.
fn add_time(delay_ms: &mut u32, dt: Duration, carry: &mut Duration) {
    *carry += dt;
    let ms = carry.as_millis() as u32;
    *delay_ms += ms;
    *carry -= Duration::from_millis(ms as u64);
}

fn run(mut cap: impl Screen, mut opts: RecordOptions, s: Arc<Shared>, on_status: impl Fn(RecStatus)) {
    let fps = opts.fps.clamp(1, 100);
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut last_px: Option<Arc<Vec<u8>>> = None;
    let direct = opts.direct.take();
    // MP4에 넣을 소리. 녹화 시간이 흐르기 시작한 때와 멈춘 때를 알려 줘 영상과 맞춘다.
    let audio = opts.audio.take();
    // MP4에 넣은 프레임 수. 녹화 시간 × FPS만큼 되도록 맞춰 넣는다.
    let mut written = 0u64;
    // 메모리 한도 뒤로는 편집용 프레임을 모으지 않는다 (바로 저장일 때)
    let mut keep = true;
    // 마지막 캡처 시각. 그 뒤로 흐른 시간은 마지막 프레임이 보인 시간이다.
    let mut last_tick: Option<Instant> = None;
    let mut recorded = Duration::ZERO; // 일시정지 시간을 뺀 녹화 시간
    let mut carry = Duration::ZERO; // 편집용 프레임 시간에 아직 못 넣은 1ms 미만의 자투리
    let mut bytes = 0usize;
    let mut last_report = Instant::now() - Duration::from_secs(1);
    let mut stop_reason = None;
    // 편집용 마지막 프레임이 아직 지금 화면인가. 편집용 프레임을 그만 모은 뒤로는 그 프레임 시간을 늘리지 않는다.
    let mut extend = true;

    while !s.stop.load(Ordering::SeqCst) {
        // 녹화 도중에는 크기를 바꾸지 않고 자리만 옮긴다. 배율이 다른 모니터로 틀을 옮기면 같은 틀이라도
        // 픽셀 크기가 달라지는데, 그대로 따라가면 MP4에는 크기가 다른 화면이 섞여 찢어지고
        // 편집용 프레임도 크기가 섞여 저장할 때 앱이 꺼졌다.
        let requested = s.region.lock().take();
        if let Some(r) = requested.map(|r| keep_size(r, cap.region()))
            && r != cap.region()
            && let Err(e) = cap.set_region(r)
        {
            stop_reason = Some(format!("녹화 영역 변경 실패: {e}"));
            break;
        }

        if s.paused.load(Ordering::SeqCst) {
            // 일시정지 직전까지의 시간만 넣고, 쉬는 시간은 프레임 시간에 넣지 않는다
            if let Some(t) = last_tick.take() {
                let dt = t.elapsed();
                if extend {
                    extend_last(&s, dt, &mut carry);
                }
                recorded += dt;
                if let Some(a) = &audio {
                    a.mark(audio::now(), false);
                }
            }
            std::thread::sleep(Duration::from_millis(15));
            next = Instant::now();
            report(&s, &on_status, &mut last_report, true, recorded, bytes);
            continue;
        }

        let now = Instant::now();
        if now < next {
            std::thread::sleep((next - now).min(Duration::from_millis(5)));
            continue;
        }
        // 이번 캡처부터 녹화 시간이 흐른다(처음, 또는 일시정지가 풀린 뒤). 소리에게는 캡처하기 전에 알린다.
        // 캡처는 길면 1초까지 걸리는데, 그 뒤에 알리면 그동안 온 소리가 버려져 앞부분이 무음이 된다.
        if let (Some(a), None) = (&audio, last_tick) {
            a.mark(audio::now(), true);
        }
        next += interval;
        if next < now {
            next = now + interval; // 밀렸으면 따라잡지 말고 다음 칸부터
        }

        let px = match cap.grab(&opts.cursor) {
            Ok(px) => Arc::new(px),
            Err(e) => {
                stop_reason = Some(format!("화면 캡처 실패: {e}"));
                break;
            }
        };
        let r = cap.region();
        *s.latest.lock() = Some((r.w as u32, r.h as u32, px.clone()));
        if let Some(t) = last_tick.replace(now) {
            let dt = now.duration_since(t);
            if extend {
                extend_last(&s, dt, &mut carry);
            }
            recorded += dt;
        }
        if let Some(d) = &direct {
            if d.failed() {
                stop_reason = Some("MP4를 더 저장할 수 없어 녹화를 멈췄습니다 (디스크 공간을 확인해 주세요)".into());
                break;
            }
            let target = (recorded.as_secs_f64() * fps as f64).round() as u64 + 1;
            let copies = target.saturating_sub(written) as u32;
            // 캡처가 밀려 여러 칸을 한꺼번에 채울 때, 지난 칸들은 그때 보이던 앞 화면으로 채우고 지금 화면은 한 칸만 넣는다.
            // 모두 지금 화면으로 채우면 화면이 실제보다 일찍 바뀐 것처럼 저장된다.
            match &last_px {
                Some(prev) if copies > 1 => {
                    d.push(prev.clone(), copies - 1);
                    d.push(px.clone(), 1);
                }
                _ => d.push(px.clone(), copies),
            }
            written = written.max(target);
        }
        let same = opts.skip_same_frame && last_px.as_ref().is_some_and(|l| **l == *px);
        if !same && keep {
            // 새 프레임은 시간 0에서 시작해서, 다음 캡처까지 흐른 시간만큼 늘어난다
            let f = Frame::from_rgba(r.w as u32, r.h as u32, &px, 0);
            bytes += f.packed_bytes();
            s.frames.lock().push(f);
        }
        if !same {
            last_px = Some(px);
        }
        // 이번 화면까지는 편집용 프레임에 담겼다 (아래에서 한도에 닿아도 다음 캡처까지는 보인 시간이다)
        extend = keep;

        if let Some(max) = opts.max_ms
            && recorded.as_millis() as u64 >= max
        {
            stop_reason = Some("최대 녹화 시간이 지나 녹화를 멈췄습니다".into());
            break;
        }
        if keep && bytes >= opts.max_bytes {
            if direct.is_some() && opts.drop_frames_after_limit {
                // MP4에는 계속 저장하고, 편집용 프레임만 더 모으지 않는다
                keep = false;
                s.dropped.store(true, Ordering::SeqCst);
            } else {
                stop_reason =
                    Some("메모리가 부족해 녹화를 멈췄습니다 (설정에서 편집 메모리 한도를 바꿀 수 있습니다)".into());
                break;
            }
        }
        report(&s, &on_status, &mut last_report, false, recorded, bytes);
    }

    // 소리에게 녹화 시간이 여기서 멈췄다고 알린다
    if let (Some(a), Some(_)) = (&audio, last_tick) {
        a.mark(audio::now(), false);
    }
    // 마지막 프레임: 멈춘 시점까지 보인 시간, 최소 한 칸
    if let Some(t) = last_tick {
        let dt = t.elapsed();
        if extend {
            extend_last(&s, dt, &mut carry);
        }
        recorded += dt;
    }
    if let Some(d) = direct {
        // 마지막 화면을 멈춘 시점까지 채우고 파일을 마무리한다
        let saved = if let Some(px) = &last_px {
            let target = (recorded.as_secs_f64() * fps as f64).round() as u64;
            d.push(px.clone(), target.saturating_sub(written) as u32);
            written = written.max(target);
            d.finish().map_err(|e| format!("{e:#}"))
        } else {
            // 화면을 한 장도 못 받았다(첫 캡처부터 실패 등): 빈 MP4를 남기지 않는다 (버리면 파일을 지운다)
            drop(d);
            Err("녹화된 화면이 없어 MP4를 만들지 않았습니다".to_string())
        };
        if let Some(a) = audio {
            // 영상 길이 = 넣은 프레임 수 ÷ FPS
            let total = Duration::from_secs_f64(written as f64 / fps as f64);
            *s.audio_warning.lock() = finish_sound(a, &saved, total);
        }
        *s.direct_result.lock() = Some(saved);
    }
    if let Some(last) = s.frames.lock().last_mut() {
        last.delay_ms = last.delay_ms.max(interval.as_millis() as u32);
    }

    let mut st = s.status.lock();
    st.recording = false;
    st.paused = false;
    st.stop_reason = stop_reason;
    st.frames = s.frames.lock().len();
    on_status(st.clone());
}

/// 소리 길이를 영상 길이(total)에 맞춰 마무리하고 MP4에 넣는다. 문제가 있었으면 사용자에게 알릴 말을 돌려준다.
/// 소리와 함께 녹화한 MP4는 마무리(faststart)를 미뤄 두었으므로, 소리를 못 넣어도 여기서 마무리한다.
fn finish_sound(audio: AudioCapture, saved: &Result<PathBuf, String>, total: Duration) -> Option<String> {
    let mut file = match audio.finish(total) {
        Ok(file) => file,
        Err(e) => {
            if let Ok(path) = saved {
                let _ = crate::export::finish_mp4(path, None, total);
            }
            return Some(format!("소리를 녹음하지 못했습니다 ({e:#})"));
        }
    };
    // 영상을 저장하지 못했으면 할 일이 없다
    let path = saved.as_ref().ok()?;
    let sound = (file.frames > 0).then_some(file.path.as_path());
    match crate::export::finish_mp4(path, sound, total) {
        Ok(()) => file.warning.take(),
        Err(e) => {
            // 소리 없이라도 파일 마무리는 해 둔다 (실패해도 MP4는 재생된다)
            let _ = crate::export::finish_mp4(path, None, total);
            Some(format!("소리를 영상에 넣지 못했습니다 ({e:#})"))
        }
    }
}

fn report(
    s: &Shared,
    on_status: &impl Fn(RecStatus),
    last: &mut Instant,
    paused: bool,
    recorded: Duration,
    bytes: usize,
) {
    if last.elapsed() < Duration::from_millis(200) {
        return;
    }
    *last = Instant::now();
    let st = {
        let mut st = s.status.lock();
        st.paused = paused;
        st.frames = s.frames.lock().len();
        st.elapsed_ms = recorded.as_millis() as u64;
        st.memory_mb = (bytes / (1024 * 1024)) as u64;
        st.clone()
    };
    on_status(st);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 캡처할 때마다 색이 바뀌는 4×4 가짜 화면
    struct FakeScreen(u8);

    impl Screen for FakeScreen {
        fn region(&self) -> Rect {
            Rect { x: 0, y: 0, w: 4, h: 4 }
        }
        fn set_region(&mut self, _: Rect) -> anyhow::Result<()> {
            Ok(())
        }
        fn grab(&mut self, _: &CursorOptions) -> anyhow::Result<Vec<u8>> {
            self.0 = self.0.wrapping_add(1);
            Ok(vec![self.0; 64])
        }
    }

    /// 60FPS로 10초: 한 칸(16.667ms)씩 600번 더하면 10초가 돼야 한다.
    /// 예전에는 매번 16ms로 잘라 넣어 9.6초가 됐다.
    #[test]
    fn frame_time_keeps_the_part_shorter_than_a_millisecond() {
        let (mut delay, mut carry) = (0u32, Duration::ZERO);
        for _ in 0..600 {
            add_time(&mut delay, Duration::from_micros(16_667), &mut carry);
        }
        assert!((9_999..=10_001).contains(&delay), "{delay}ms");
        assert!(carry < Duration::from_millis(1));
    }

    #[test]
    fn moving_while_recording_keeps_the_size() {
        let now = Rect { x: 0, y: 0, w: 800, h: 600 };
        // 크기가 같으면 그대로 옮긴다
        let same = Rect { x: 100, y: 50, w: 800, h: 600 };
        assert_eq!(keep_size(same, now), same);
        // 배율이 1.5배인 모니터로 옮겨 픽셀 크기가 커져도, 가운데에 원래 크기로
        let bigger = Rect { x: 2000, y: 100, w: 1200, h: 900 };
        assert_eq!(keep_size(bigger, now), Rect { x: 2200, y: 250, w: 800, h: 600 });
    }

    #[test]
    fn frames_kept_before_memory_limit_do_not_absorb_the_rest_of_direct_recording() {
        if crate::export::find_ffmpeg().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("opencam_direct_limit_{}", std::process::id()));
        let opts = RecordOptions {
            region: Rect { x: 0, y: 0, w: 4, h: 4 },
            fps: 50,
            skip_same_frame: false,
            max_ms: Some(600),
            max_bytes: 1, // 첫 프레임에서 바로 한도
            cursor: CursorOptions {
                draw: false,
                highlight: false,
                highlight_color: 0,
                click_fx: false,
                left_color: 0,
                right_color: 0,
            },
            direct: Some(Mp4Stream::start(&dir.join("d.mp4"), 4, 4, 50, 80, true).unwrap()),
            audio: None,
            drop_frames_after_limit: true,
        };
        let rec = Recorder::spawn(FakeScreen(0), opts, |_| {}).unwrap();
        // 최대 녹화 시간으로 스스로 멈출 때까지 기다린다
        let t = Instant::now();
        while rec.status().recording && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = rec.stop();
        assert!(result.dropped);
        assert!(matches!(result.direct, Some(Ok(_))));
        assert_eq!(result.frames.len(), 1);
        // 예전에는 편집용 프레임을 그만 모은 뒤의 녹화 시간(약 0.6초)이 모두 이 프레임에 더해졌다
        assert!(result.frames[0].delay_ms < 300, "{}ms", result.frames[0].delay_ms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 소리와 함께 바로 저장: 일시정지를 섞어도 MP4에 소리가 들어가고 길이가 영상과 같다.
    /// ffmpeg나 소리 장치가 없으면 건너뛴다.
    #[test]
    fn direct_recording_with_sound_has_an_audio_track_as_long_as_the_video() {
        if crate::export::find_ffmpeg().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("opencam_direct_audio_{}", std::process::id()));
        let direct = Mp4Stream::start(&dir.join("d.mp4"), 4, 4, 50, 80, false).unwrap();
        let Ok(sound) = AudioCapture::start(&dir.join("d.audio.tmp")) else {
            eprintln!("소리 장치가 없어 건너뜀");
            let _ = direct.finish();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let opts = RecordOptions {
            region: Rect { x: 0, y: 0, w: 4, h: 4 },
            fps: 50,
            skip_same_frame: false,
            max_ms: None,
            max_bytes: usize::MAX,
            cursor: CursorOptions {
                draw: false,
                highlight: false,
                highlight_color: 0,
                click_fx: false,
                left_color: 0,
                right_color: 0,
            },
            direct: Some(direct),
            audio: Some(sound),
            drop_frames_after_limit: true,
        };
        let rec = Recorder::spawn(FakeScreen(0), opts, |_| {}).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        rec.set_paused(true);
        std::thread::sleep(Duration::from_millis(400));
        rec.set_paused(false);
        std::thread::sleep(Duration::from_millis(500));
        let result = rec.stop();
        assert_eq!(result.audio_warning, None);
        let path = result.direct.unwrap().unwrap();
        let (has_audio, secs) = crate::export::probe_audio(&path);
        assert!(has_audio);
        // 일시정지 0.4초는 빠지고 약 1초
        assert!((0.8..1.3).contains(&secs), "{secs}초");
        assert!(!dir.join("d.audio.tmp").exists(), "소리 임시 파일을 남기지 않는다");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
