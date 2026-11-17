//! PC에서 나는 소리(스피커·이어폰으로 나가는 소리) 녹음. MP4 바로 저장에 소리를 넣을 때 쓴다.
//!
//! 방식:
//! - WASAPI 루프백: 기본 출력 장치로 나가는 소리를 그대로 받는다
//! - 아무 소리도 안 나는 동안에는 Windows가 소리를 주지 않는다. 소리 묶음마다 붙어 오는 시각으로
//!   녹화 시간 안의 자리를 정하고, 빈 곳은 무음으로 채운다
//! - 일시정지한 동안의 소리는 버린다 (녹화 스레드가 `mark`로 알려 준다)
//! - 파일에는 16비트 스테레오를 헤더 없이 쌓는다. 녹화가 끝나면 ffmpeg가 영상에 합친다
//! - 녹화 중 기본 출력 장치가 바뀌면(이어폰 연결 등) 새 장치로 옮겨 받는다

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 시각 단위: 1초 = 10,000,000 (100나노초). 소리 묶음에 붙어 오는 시각과 같은 단위.
const TICKS: i64 = 10_000_000;
/// Windows에 부탁하는 소리 버퍼 길이 (1초). 받는 쪽이 잠깐 밀려도 소리가 끊기지 않는다.
const BUFFER: i64 = TICKS;
/// 소리 묶음에 붙는 표시: 무음 / 시각을 믿을 수 없음
const FLAG_SILENT: u32 = 0x2;
const FLAG_TIMESTAMP_ERROR: u32 = 0x4;

/// 지금 시각 (`TICKS` 단위). `AudioCapture::mark`에 넘긴다.
pub fn now() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    });
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    (c as i128 * TICKS as i128 / freq as i128) as i64
}

/// 녹음한 소리 파일 (16비트 스테레오, 헤더 없음). 다 쓰고 버리면 파일도 지운다.
pub struct AudioFile {
    pub path: PathBuf,
    /// 1초에 몇 묶음(샘플)인지
    pub rate: u32,
    /// 들어 있는 길이 (샘플 묶음 수)
    pub frames: u64,
    /// 녹음 중 생긴 문제 (소리 장치가 바뀜 등). 사용자에게 알린다.
    pub warning: Option<String>,
}

impl Drop for AudioFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Shared {
    stop: AtomicBool,
    /// 녹화가 돌기 시작한 시각(true)·멈춘 시각(false). 녹화 스레드가 넣고 소리 스레드가 꺼낸다.
    marks: Mutex<VecDeque<(i64, bool)>>,
    /// 끝낼 때 맞출 전체 길이
    total: Mutex<Option<Duration>>,
    warning: Mutex<Option<String>>,
}

pub struct AudioCapture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<u64>>>,
    path: PathBuf,
    rate: u32,
}

impl AudioCapture {
    /// 소리 받기를 시작한다. 장치가 없거나 열 수 없으면 바로 오류를 돌려준다.
    /// `mark(시각, true)`를 부르기 전의 소리는 버린다.
    pub fn start(path: &Path) -> Result<Self> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            marks: Mutex::new(VecDeque::new()),
            total: Mutex::new(None),
            warning: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        let (s, p) = (shared.clone(), path.to_path_buf());
        let thread = std::thread::Builder::new().name("opencam-audio".into()).spawn(move || capture_thread(&s, &p, tx))?;
        let rate = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(rate)) => rate,
            failed => {
                shared.stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                let _ = std::fs::remove_file(path);
                return Err(match failed {
                    Ok(Err(e)) => e,
                    _ => anyhow!("소리 장치가 응답하지 않습니다"),
                });
            }
        };
        Ok(AudioCapture { shared, thread: Some(thread), path: path.to_path_buf(), rate })
    }

    /// 녹화가 돌기 시작했거나(running) 멈췄음(일시정지)을 그 시각(`now()`)과 함께 알린다
    pub fn mark(&self, at: i64, running: bool) {
        self.shared.marks.lock().push_back((at, running));
    }

    /// 녹음을 끝내고 파일 길이를 영상 길이(total)에 맞춘다. 모자라면 무음을 붙이고 넘치면 자른다.
    pub fn finish(mut self, total: Duration) -> Result<AudioFile> {
        *self.shared.total.lock() = Some(total);
        self.shared.stop.store(true, Ordering::SeqCst);
        let joined = self.thread.take().map(|t| t.join());
        match joined {
            Some(Ok(Ok(frames))) => Ok(AudioFile {
                path: self.path.clone(),
                rate: self.rate,
                frames,
                warning: self.shared.warning.lock().take(),
            }),
            other => {
                let _ = std::fs::remove_file(&self.path);
                match other {
                    Some(Ok(Err(e))) => Err(e),
                    _ => bail!("소리 녹음이 비정상으로 끝났습니다"),
                }
            }
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        // finish 없이 버려졌으면 (녹화를 시작하지 못함) 조용히 정리한다
        if let Some(t) = self.thread.take() {
            self.shared.stop.store(true, Ordering::SeqCst);
            let _ = t.join();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------- 녹화 시간 계산 ----------

/// 녹화가 돌아간 구간만 더해 녹화 시간을 낸다 (일시정지 시간은 뺀다). 단위는 `TICKS`.
#[derive(Default)]
struct Timeline {
    /// 끝난 구간들의 합
    done: i64,
    /// 지금 돌고 있는 구간이 시작한 시각
    since: Option<i64>,
}

impl Timeline {
    fn mark(&mut self, at: i64, running: bool) {
        match (self.since, running) {
            (None, true) => self.since = Some(at),
            (Some(s), false) => {
                self.done += (at - s).max(0);
                self.since = None;
            }
            _ => {}
        }
    }

    /// 시각 at이 녹화 시간으로 언제인지. 녹화가 멈춰 있을 때의 시각이면 None.
    fn at(&self, at: i64) -> Option<i64> {
        self.since.filter(|s| at >= *s).map(|s| self.done + (at - s))
    }
}

// ---------- 소리 파일 ----------

/// 16비트 스테레오를 헤더 없이 쌓는 파일. 녹화 시간에 맞는 자리에 소리를 놓는다.
struct Track {
    out: BufWriter<File>,
    rate: u32,
    /// 지금까지 쓴 길이 (샘플 묶음 수)
    frames: u64,
}

impl Track {
    fn create(path: &Path, rate: u32) -> std::io::Result<Self> {
        Ok(Track { out: BufWriter::with_capacity(1 << 16, File::create(path)?), rate, frames: 0 })
    }

    fn silence(&mut self, frames: u64) -> std::io::Result<()> {
        let zeros = [0u8; 4096];
        let mut left = frames * 4;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            self.out.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        self.frames += frames;
        Ok(())
    }

    /// 녹화 시간 pos(`TICKS` 단위)에 소리(왼쪽·오른쪽 번갈아)를 놓는다.
    /// 파일 끝과 거의 이어지면 그대로 붙이고, 비었으면 무음을 채우고, 겹치면 겹친 만큼 버린다.
    fn put(&mut self, pos: i64, mut samples: &[i16]) -> std::io::Result<()> {
        let want = (pos.max(0) as i128 * self.rate as i128 / TICKS as i128) as i64;
        let gap = want - self.frames as i64;
        // 시각은 조금씩 흔들린다. 0.05초 안쪽 차이는 이어진 소리로 본다.
        let tol = (self.rate / 20) as i64;
        if gap > tol {
            self.silence(gap as u64)?;
        } else if gap < -tol {
            let skip = ((-gap) as usize).min(samples.len() / 2);
            samples = &samples[skip * 2..];
        }
        for v in samples {
            self.out.write_all(&v.to_le_bytes())?;
        }
        self.frames += (samples.len() / 2) as u64;
        Ok(())
    }

    /// 길이를 total에 맞추고 닫는다. 맞춘 길이를 돌려준다.
    fn finish(mut self, total: Duration) -> std::io::Result<u64> {
        let want = (total.as_secs_f64() * self.rate as f64).round() as u64;
        if self.frames < want {
            self.silence(want - self.frames)?;
        }
        self.out.flush()?;
        if self.frames > want {
            self.out.get_ref().set_len(want * 4)?;
        }
        Ok(want)
    }
}

// ---------- 소리 형식 ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sample {
    F32,
    I16,
    I24,
    I32,
}

/// 장치가 주는 소리 형식
#[derive(Clone, Copy, PartialEq, Debug)]
struct Format {
    sample: Sample,
    channels: usize,
    rate: u32,
}

impl Format {
    fn sample_bytes(&self) -> usize {
        match self.sample {
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::F32 | Sample::I32 => 4,
        }
    }

    /// 샘플 묶음 하나(모든 채널)의 바이트 수
    fn block(&self) -> usize {
        self.sample_bytes() * self.channels
    }
}

fn to16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// 장치 형식의 소리를 16비트 스테레오로 바꿔 out 뒤에 붙인다.
/// 1채널은 양쪽에 똑같이, 5.1·7.1은 가운데·뒤 스피커 소리를 양쪽에 섞는다.
fn to_stereo16(fmt: &Format, data: &[u8], out: &mut Vec<i16>) {
    let n = fmt.sample_bytes();
    let read = |frame: &[u8], ch: usize| -> f32 {
        let b = &frame[ch * n..];
        match fmt.sample {
            Sample::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Sample::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            Sample::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            Sample::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    };
    for frame in data.chunks_exact(fmt.block().max(1)) {
        let (l, r) = match fmt.channels {
            0 => (0.0, 0.0),
            1 => (read(frame, 0), read(frame, 0)),
            2..=5 => (read(frame, 0), read(frame, 1)),
            _ => {
                // 앞 왼쪽·앞 오른쪽·가운데·저음·뒤 왼쪽·뒤 오른쪽 순서
                let center = read(frame, 2) * 0.707;
                (read(frame, 0) + center + read(frame, 4) * 0.707, read(frame, 1) + center + read(frame, 5) * 0.707)
            }
        };
        out.push(to16(l));
        out.push(to16(r));
    }
}

/// Windows가 알려 준 장치의 기본 형식을 읽는다
unsafe fn mix_format(p: *const WAVEFORMATEX) -> Result<Format> {
    unsafe {
        let f = std::ptr::read_unaligned(p);
        let (tag, bits, channels, rate) = (f.wFormatTag, f.wBitsPerSample, f.nChannels, f.nSamplesPerSec);
        // 0xFFFE(확장 형식)이면 진짜 종류는 뒤쪽 SubFormat의 첫 숫자에 있다 (1 = 정수, 3 = 실수)
        let kind =
            if tag == 0xFFFE && f.cbSize >= 22 { std::ptr::read_unaligned((p as *const u8).add(24) as *const u32) } else { tag as u32 };
        let sample = match (kind, bits) {
            (3, 32) => Sample::F32,
            (1, 16) => Sample::I16,
            (1, 24) => Sample::I24,
            (1, 32) => Sample::I32,
            _ => bail!("지원하지 않는 소리 형식입니다 (종류 {kind}, {bits}비트)"),
        };
        if channels == 0 || rate == 0 {
            bail!("소리 형식을 읽을 수 없습니다");
        }
        Ok(Format { sample, channels: channels as usize, rate })
    }
}

// ---------- 장치 ----------

/// 열어 둔 출력 장치 하나
struct Session {
    _client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: Format,
    /// 장치 이름표. 기본 장치가 바뀌었는지 볼 때 쓴다.
    id: String,
}

unsafe fn default_device(en: &IMMDeviceEnumerator) -> windows::core::Result<(IMMDevice, String)> {
    unsafe {
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let p = dev.GetId()?;
        let id = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        Ok((dev, id))
    }
}

/// 기본 출력 장치를 루프백(나가는 소리 받기)으로 연다
unsafe fn open(en: &IMMDeviceEnumerator) -> Result<Session> {
    unsafe {
        let (dev, id) = default_device(en).context("소리 출력 장치를 찾을 수 없습니다")?;
        // 먼저 48000Hz 스테레오 16비트로 바꿔서 달라고 한다. 그러면 장치가 바뀌어도 형식이 같다.
        let want = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).context("소리 장치를 열 수 없습니다")?;
        let convert =
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let (client, fmt) = if client.Initialize(AUDCLNT_SHAREMODE_SHARED, convert, BUFFER, 0, &want, None).is_ok() {
            (client, Format { sample: Sample::I16, channels: 2, rate: 48000 })
        } else {
            // 바꿔 주지 않는 장치면 장치의 기본 형식 그대로 받아 직접 바꾼다
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).context("소리 장치를 열 수 없습니다")?;
            let p = client.GetMixFormat().context("소리 형식을 알 수 없습니다")?;
            let opened = mix_format(p).and_then(|fmt| {
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, BUFFER, 0, p, None)
                    .context("소리 받기를 시작할 수 없습니다")?;
                Ok(fmt)
            });
            CoTaskMemFree(Some(p as *const core::ffi::c_void));
            (client, opened?)
        };
        let capture: IAudioCaptureClient = client.GetService().context("소리 받기를 시작할 수 없습니다")?;
        client.Start().context("소리 받기를 시작할 수 없습니다")?;
        Ok(Session { _client: client, capture, fmt, id })
    }
}

/// 쌓여 있는 소리 묶음을 모두 꺼내 f(시각, 소리, 무음인지)에 넘긴다
unsafe fn drain(s: &Session, mut f: impl FnMut(i64, &[u8], bool)) -> windows::core::Result<()> {
    unsafe {
        loop {
            if s.capture.GetNextPacketSize()? == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let (mut frames, mut flags, mut stamp) = (0u32, 0u32, 0u64);
            s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut stamp))?;
            let silent = flags & FLAG_SILENT != 0 || data.is_null();
            // 시각이 없거나 지금과 동떨어져 있으면 지금 시각에서 이 묶음 길이만큼 앞으로 본다
            let t = now();
            let length = frames as i64 * TICKS / s.fmt.rate as i64;
            let at = stamp as i64;
            let at = if flags & FLAG_TIMESTAMP_ERROR != 0 || (t - at).abs() > 5 * TICKS { t - length } else { at };
            let bytes: &[u8] =
                if silent { &[] } else { std::slice::from_raw_parts(data, frames as usize * s.fmt.block()) };
            f(at, bytes, silent);
            s.capture.ReleaseBuffer(frames)?;
        }
    }
}

fn capture_thread(shared: &Shared, path: &Path, ready: mpsc::Sender<Result<u32>>) -> Result<u64> {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready.send(Err(anyhow!("소리 기능을 준비할 수 없습니다: {e}")));
            bail!("소리 기능 준비 실패");
        }
        let result = capture_loop(shared, path, ready);
        CoUninitialize();
        result
    }
}

unsafe fn capture_loop(shared: &Shared, path: &Path, ready: mpsc::Sender<Result<u32>>) -> Result<u64> {
    unsafe {
        let start = || -> Result<(IMMDeviceEnumerator, Session, Track)> {
            let en: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("소리 장치 목록을 열 수 없습니다")?;
            let session = open(&en)?;
            let track = Track::create(path, session.fmt.rate).context("소리 임시 파일을 만들 수 없습니다")?;
            Ok((en, session, track))
        };
        let (en, session, mut track) = match start() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.send(Err(e));
                bail!("소리 녹음 시작 실패");
            }
        };
        let rate = session.fmt.rate;
        let _ = ready.send(Ok(rate));

        let mut session = Some(session);
        let mut timeline = Timeline::default();
        let mut pcm: Vec<i16> = vec![];
        let mut checked = Instant::now();
        // 새 장치의 소리 빠르기가 달라 이어 받을 수 없게 됨
        let mut gave_up = false;
        let mut write_failed = false;

        loop {
            // 멈추라는 표시를 본 뒤에도 남은 소리를 한 번 더 꺼내고 끝낸다
            let stopping = shared.stop.load(Ordering::SeqCst);
            if let Some(s) = &session {
                let drained = drain(s, |at, data, silent| {
                    let mut marks = shared.marks.lock();
                    while marks.front().is_some_and(|m| m.0 <= at) {
                        if let Some((when, running)) = marks.pop_front() {
                            timeline.mark(when, running);
                        }
                    }
                    drop(marks);
                    if silent || write_failed {
                        return;
                    }
                    if let Some(pos) = timeline.at(at) {
                        pcm.clear();
                        to_stereo16(&s.fmt, data, &mut pcm);
                        write_failed = track.put(pos, &pcm).is_err();
                    }
                });
                if drained.is_err() {
                    // 장치가 사라졌다 (뽑힘, 꺼짐). 아래에서 새 기본 장치를 찾는다.
                    session = None;
                }
            }
            if stopping {
                break;
            }
            if !gave_up && checked.elapsed() >= Duration::from_secs(1) {
                checked = Instant::now();
                let changed = match &session {
                    None => true,
                    Some(s) => default_device(&en).map(|(_, id)| id != s.id).unwrap_or(false),
                };
                if changed {
                    match open(&en) {
                        Ok(new) if new.fmt.rate == rate => session = Some(new),
                        Ok(_) => {
                            session = None;
                            gave_up = true;
                        }
                        // 지금은 장치가 없다. 조금 뒤에 다시 찾는다.
                        Err(_) => session = None,
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        if write_failed {
            *shared.warning.lock() = Some("소리 임시 파일을 쓰지 못해 소리가 중간에 끊겼습니다".into());
        } else if gave_up || session.is_none() {
            *shared.warning.lock() = Some("녹화 중 소리 장치가 바뀌어 일부 구간에 소리가 없습니다".into());
        }
        let total = shared.total.lock().unwrap_or_default();
        track.finish(total).context("소리 임시 파일을 마무리하지 못했습니다")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("opencam_audio_{}_{name}", std::process::id()))
    }

    fn read16(path: &Path) -> Vec<i16> {
        std::fs::read(path).unwrap().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }

    #[test]
    fn timeline_skips_paused_time() {
        let mut t = Timeline::default();
        assert_eq!(t.at(5), None, "녹화 시작 전");
        t.mark(100, true);
        assert_eq!(t.at(90), None, "시작 시각보다 앞선 소리");
        assert_eq!(t.at(100), Some(0));
        assert_eq!(t.at(160), Some(60));
        t.mark(200, false);
        assert_eq!(t.at(250), None, "일시정지 중");
        t.mark(1000, true);
        assert_eq!(t.at(1000), Some(100), "일시정지 시간은 빠진다");
        assert_eq!(t.at(1030), Some(130));
        // 같은 표시가 두 번 와도 그대로
        t.mark(1010, true);
        assert_eq!(t.at(1030), Some(130));
    }

    #[test]
    fn track_fills_gaps_and_drops_overlap() {
        let path = temp("track.raw");
        // 1초에 1000묶음: 1묶음 = 10,000틱, 허용 오차 50묶음
        let mut track = Track::create(&path, 1000).unwrap();
        track.put(0, &[1, 1, 2, 2]).unwrap();
        // 거의 이어지는 소리(10묶음 차이)는 그대로 붙인다
        track.put(12 * 10_000, &[3, 3]).unwrap();
        assert_eq!(track.frames, 3);
        // 100묶음 자리에 오면 그 사이를 무음으로 채운다
        track.put(100 * 10_000, &[4, 4]).unwrap();
        assert_eq!(track.frames, 101);
        // 파일 끝(101)보다 60묶음 앞선 자리(41)에 온 소리는 겹친 만큼 버린다
        let late: Vec<i16> = (0..70).flat_map(|i| [100 + i, 100 + i]).collect();
        track.put(41 * 10_000, &late).unwrap();
        assert_eq!(track.frames, 111);
        // 전체 길이 0.2초(200묶음)에 맞추면 뒤는 무음
        assert_eq!(track.finish(Duration::from_millis(200)).unwrap(), 200);
        let s = read16(&path);
        assert_eq!(s.len(), 400);
        assert_eq!(&s[..6], &[1, 1, 2, 2, 3, 3]);
        assert!(s[6..200].iter().all(|v| *v == 0));
        assert_eq!(&s[200..204], &[4, 4, 160, 160], "겹친 60묶음은 버리고 61번째부터");
        assert_eq!(s[221], 169);
        assert!(s[222..].iter().all(|v| *v == 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_cuts_what_is_longer_than_the_video() {
        let path = temp("cut.raw");
        let mut track = Track::create(&path, 1000).unwrap();
        track.put(0, &[7; 600]).unwrap();
        assert_eq!(track.finish(Duration::from_millis(100)).unwrap(), 100);
        assert_eq!(read16(&path), vec![7; 200]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn device_formats_become_16_bit_stereo() {
        let mut out = vec![];
        // 실수 2채널: 범위를 넘으면 끝값으로
        let f = Format { sample: Sample::F32, channels: 2, rate: 48000 };
        let data: Vec<u8> = [0.5f32, -0.5, 2.0, -2.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        to_stereo16(&f, &data, &mut out);
        assert_eq!(out, [16384, -16384, 32767, -32767]);
        // 16비트 1채널: 양쪽에 똑같이
        out.clear();
        let f = Format { sample: Sample::I16, channels: 1, rate: 44100 };
        to_stereo16(&f, &1000i16.to_le_bytes(), &mut out);
        assert_eq!(out, [1000, 1000]);
        // 24비트 2채널: 음수 부호가 살아야 한다
        out.clear();
        let f = Format { sample: Sample::I24, channels: 2, rate: 48000 };
        to_stereo16(&f, &[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], &mut out);
        assert_eq!(out, [16384, -16384]);
        // 5.1: 가운데 소리가 양쪽에 섞인다
        out.clear();
        let f = Format { sample: Sample::F32, channels: 6, rate: 48000 };
        let data: Vec<u8> = [0.0f32, 0.0, 0.5, 0.9, 0.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        to_stereo16(&f, &data, &mut out);
        assert_eq!(out[0], out[1]);
        assert!((out[0] - 11583).abs() <= 1, "{}", out[0]);
    }

    #[test]
    fn extensible_format_is_read_from_its_sub_format() {
        // 흔한 장치 형식: 확장 형식(0xFFFE) 안에 32비트 실수, 48000Hz 2채널
        let mut raw = [0u8; 40];
        raw[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
        raw[2..4].copy_from_slice(&2u16.to_le_bytes());
        raw[4..8].copy_from_slice(&48000u32.to_le_bytes());
        raw[14..16].copy_from_slice(&32u16.to_le_bytes());
        raw[16..18].copy_from_slice(&22u16.to_le_bytes());
        raw[24..28].copy_from_slice(&3u32.to_le_bytes());
        let f = unsafe { mix_format(raw.as_ptr() as *const WAVEFORMATEX) }.unwrap();
        assert_eq!(f, Format { sample: Sample::F32, channels: 2, rate: 48000 });
        // 모르는 형식은 오류
        raw[24..28].copy_from_slice(&9u32.to_le_bytes());
        assert!(unsafe { mix_format(raw.as_ptr() as *const WAVEFORMATEX) }.is_err());
    }

    /// 이 PC의 실제 출력 장치에서 잠깐 받아 본다. 장치가 없으면(원격 접속 등) 건너뛴다.
    #[test]
    fn real_device_gives_a_file_of_the_asked_length() {
        let path = temp("real.raw");
        let cap = match AudioCapture::start(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("소리 장치가 없어 건너뜀: {e:#}");
                return;
            }
        };
        cap.mark(now(), true);
        std::thread::sleep(Duration::from_millis(300));
        cap.mark(now(), false);
        let file = cap.finish(Duration::from_millis(300)).unwrap();
        assert!(file.rate >= 8000, "{}", file.rate);
        assert_eq!(file.frames, (0.3 * file.rate as f64).round() as u64);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), file.frames * 4);
        assert_eq!(file.warning, None);
        drop(file);
        assert!(!path.exists(), "다 쓴 소리 파일은 지운다");
    }
}
