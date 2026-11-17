//! PC에서 나는 소리(스피커·이어폰으로 나가는 소리) 녹음. MP4 바로 저장에 소리를 넣을 때 쓴다.
//!
//! 방식:
//! - WASAPI 루프백: 기본 출력 장치로 나가는 소리를 그대로 받는다
//! - 받은 소리는 16비트 스테레오로 ffmpeg에 흘려보내 녹화하는 동안 AAC 파일로 만들어 둔다.
//!   녹화가 끝나면 그 파일을 영상에 그대로 붙이기만 하면 된다 (`export::finish_mp4`)
//! - 아무 소리도 안 나는 동안에는 Windows가 소리를 주지 않는다. 소리 묶음마다 붙어 오는 시각으로
//!   녹화 시간 안의 자리를 정하고, 빈 곳은 무음으로 채운다
//! - 일시정지한 동안의 소리는 버린다 (녹화 스레드가 `mark`로 알려 준다)
//! - 녹화 중 기본 출력 장치가 바뀌면(이어폰 연결 등) 새 장치로 옮겨 받는다

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole,
    eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 시각 단위: 1초 = 10,000,000 (100나노초). 소리 묶음에 붙어 오는 시각과 같은 단위.
const TICKS: i64 = 10_000_000;
/// Windows에 부탁하는 소리 버퍼 길이 (1초). 받는 쪽이 잠깐 밀려도 소리가 끊기지 않는다.
const BUFFER: i64 = TICKS;
/// 소리를 꺼내러 가는 간격. 버퍼가 1초라 넉넉하고, 자리는 묶음의 시각으로 정하므로 정확도와 상관없다.
const POLL: Duration = Duration::from_millis(20);
/// 소리 묶음은 조금 늦게 도착한다. 이만큼 지난 시각까지는 올 소리가 다 왔다고 본다.
const SETTLED: i64 = TICKS / 5;

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

/// 녹음한 소리 파일 (AAC). 다 쓰고 버리면 파일도 지운다.
pub struct AudioFile {
    pub path: PathBuf,
    /// 들어 있는 길이 (샘플 묶음 수). 0이면 넣을 소리가 없다.
    pub frames: u64,
    /// 녹음 중 생긴 문제 (소리 장치가 바뀜 등). 사용자에게 알린다.
    pub warning: Option<String>,
}

impl Drop for AudioFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 녹화가 돌기 시작한 시각(true)·멈춘 시각(false) 목록. 녹화 스레드가 넣고 소리 스레드가 꺼낸다.
type Marks = Mutex<VecDeque<(i64, bool)>>;

struct Shared {
    stop: AtomicBool,
    marks: Marks,
    /// 끝낼 때 맞출 전체 길이
    total: Mutex<Option<Duration>>,
}

/// 소리 스레드가 끝나며 돌려주는 것: 담은 길이(샘플 묶음 수)와 알릴 문제
type Captured = (u64, Option<String>);

pub struct AudioCapture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<Captured>>>,
    path: PathBuf,
}

impl AudioCapture {
    /// 소리 받기를 시작한다. 장치가 없거나 열 수 없으면 바로 오류를 돌려준다.
    /// `mark(시각, true)`를 부르기 전의 소리는 버린다.
    pub fn start(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let shared =
            Arc::new(Shared { stop: AtomicBool::new(false), marks: Mutex::new(VecDeque::new()), total: Mutex::new(None) });
        let (tx, rx) = mpsc::channel();
        let (s, p) = (shared.clone(), path.to_path_buf());
        let thread = std::thread::Builder::new().name("opencam-audio".into()).spawn(move || capture_thread(&s, &p, tx))?;
        // 여기서 오류로 나가면 Drop이 스레드를 멈추고 파일을 지운다
        let capture = AudioCapture { shared, thread: Some(thread), path: path.to_path_buf() };
        // 보통 0.1초 안에 열린다. 녹화 시작이 오래 멈추지 않게 2초만 기다린다.
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("소리 장치가 응답하지 않습니다")),
        }
    }

    /// 녹화가 돌기 시작했거나(running) 멈췄음(일시정지)을 그 시각(`now()`)과 함께 알린다
    pub fn mark(&self, at: i64, running: bool) {
        self.shared.marks.lock().push_back((at, running));
    }

    /// 녹음을 끝낸다. 소리가 영상 길이(total)보다 짧으면 무음을 붙인다.
    /// (조금 넘치는 것은 영상에 넣을 때 영상 길이에서 자른다.)
    pub fn finish(mut self, total: Duration) -> Result<AudioFile> {
        *self.shared.total.lock() = Some(total);
        self.shared.stop.store(true, Ordering::SeqCst);
        // 여기서부터 파일은 AudioFile이 맡는다 (오류로 나가도 지워진다)
        let mut file = AudioFile { path: self.path.clone(), frames: 0, warning: None };
        let thread = self.thread.take().ok_or_else(|| anyhow!("소리 녹음이 이미 끝났습니다"))?;
        (file.frames, file.warning) = thread.join().map_err(|_| anyhow!("소리 녹음이 비정상으로 끝났습니다"))??;
        Ok(file)
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

    /// 시각 upto까지의 표시를 꺼내 반영한다
    fn apply(&mut self, marks: &Marks, upto: i64) {
        let mut marks = marks.lock();
        while let Some(&(at, running)) = marks.front().filter(|m| m.0 <= upto) {
            marks.pop_front();
            self.mark(at, running);
        }
    }

    /// 시각 at이 녹화 시간으로 언제인지. 녹화가 멈춰 있을 때의 시각이면 None.
    fn at(&self, at: i64) -> Option<i64> {
        self.since.filter(|s| at >= *s).map(|s| self.done + (at - s))
    }
}

// ---------- 소리 줄기 ----------

/// 16비트 스테레오(왼쪽·오른쪽 번갈아, 4바이트가 한 묶음)를 차례로 흘려보낸다.
/// 녹화 시간에 맞는 자리에 소리를 놓는다. 한 번 보낸 것은 되돌릴 수 없다(뒤로만 간다).
struct Track<W: Write> {
    out: W,
    rate: u32,
    /// 지금까지 보낸 길이 (샘플 묶음 수)
    frames: u64,
}

impl<W: Write> Track<W> {
    fn new(out: W, rate: u32) -> Self {
        Track { out, rate, frames: 0 }
    }

    /// 녹화 시간 pos(`TICKS` 단위)가 지금까지 보낸 것보다 몇 묶음 뒤인지 (앞이면 음수)
    fn gap(&self, pos: i64) -> i64 {
        (pos.max(0) as i128 * self.rate as i128 / TICKS as i128) as i64 - self.frames as i64
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

    /// 녹화 시간 pos에 소리를 놓는다.
    /// 지금까지 보낸 것과 거의 이어지면 그대로 붙이고, 비었으면 무음을 채우고, 겹치면 겹친 만큼 버린다.
    fn put(&mut self, pos: i64, mut sound: &[u8]) -> std::io::Result<()> {
        let gap = self.gap(pos);
        // 시각은 조금씩 흔들린다. 0.05초 안쪽 차이는 이어진 소리로 본다.
        let tol = (self.rate / 20) as i64;
        if gap > tol {
            self.silence(gap as u64)?;
        } else if gap < -tol {
            let skip = ((-gap) as usize).min(sound.len() / 4);
            sound = &sound[skip * 4..];
        }
        self.out.write_all(sound)?;
        self.frames += (sound.len() / 4) as u64;
        Ok(())
    }

    /// 소리가 오지 않는 동안에도 녹화 시간 pos까지 무음을 채워 둔다 (0.5초 넘게 뒤처졌을 때만).
    /// 이렇게 해 두지 않으면 조용한 녹화는 끝날 때 무음을 한꺼번에 만드느라 오래 기다린다.
    fn catch_up(&mut self, pos: i64) -> std::io::Result<()> {
        let gap = self.gap(pos);
        if gap > (self.rate / 2) as i64 {
            self.silence(gap as u64)?;
        }
        Ok(())
    }

    /// 길이가 total보다 짧으면 무음을 붙이고, 모아 둔 것을 모두 내보낸다
    fn pad_to(&mut self, total: Duration) -> std::io::Result<()> {
        let want = (total.as_secs_f64() * self.rate as f64).round() as u64;
        if self.frames < want {
            self.silence(want - self.frames)?;
        }
        self.out.flush()
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

    /// 이미 16비트 스테레오라 바꿀 것이 없는지
    fn is_stereo16(&self) -> bool {
        self.sample == Sample::I16 && self.channels == 2
    }
}

fn to16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// 장치 형식의 소리를 16비트 스테레오로 바꿔 out 뒤에 붙인다.
/// 1채널은 양쪽에 똑같이, 5.1·7.1은 가운데·뒤 스피커 소리를 양쪽에 섞는다.
fn to_stereo16(fmt: &Format, data: &[u8], out: &mut Vec<u8>) {
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
        out.extend_from_slice(&to16(l).to_le_bytes());
        out.extend_from_slice(&to16(r).to_le_bytes());
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
            _ => return Err(anyhow!("지원하지 않는 소리 형식입니다 (종류 {kind}, {bits}비트)")),
        };
        if channels == 0 || rate == 0 {
            return Err(anyhow!("소리 형식을 읽을 수 없습니다"));
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
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            // 시각이 없거나 지금과 동떨어져 있으면 지금 시각에서 이 묶음 길이만큼 앞으로 본다
            let t = now();
            let length = frames as i64 * TICKS / s.fmt.rate as i64;
            let at = stamp as i64;
            let unsure = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 || (t - at).abs() > 5 * TICKS;
            let bytes: &[u8] =
                if silent { &[] } else { std::slice::from_raw_parts(data, frames as usize * s.fmt.block()) };
            f(if unsure { t - length } else { at }, bytes, silent);
            s.capture.ReleaseBuffer(frames)?;
        }
    }
}

fn capture_thread(shared: &Shared, path: &Path, ready: mpsc::Sender<Result<()>>) -> Result<Captured> {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready.send(Err(anyhow!("소리 기능을 준비할 수 없습니다: {e}")));
            return Ok((0, None));
        }
        let result = capture_loop(shared, path, ready);
        CoUninitialize();
        result
    }
}

unsafe fn capture_loop(shared: &Shared, path: &Path, ready: mpsc::Sender<Result<()>>) -> Result<Captured> {
    unsafe {
        let start = || -> Result<_> {
            let en: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("소리 장치 목록을 열 수 없습니다")?;
            let session = open(&en)?;
            // 받은 소리를 그때그때 AAC로 바꿔 줄 ffmpeg
            let mut encoder = crate::export::aac_encoder(path, session.fmt.rate)?;
            let input = encoder.stdin.take().ok_or_else(|| anyhow!("소리 변환을 시작할 수 없습니다"))?;
            Ok((en, session, encoder, input))
        };
        let (en, session, mut encoder, input) = match start() {
            Ok(v) => v,
            Err(e) => {
                // 시작하지 못한 까닭은 start()가 기다리는 쪽으로 보낸다
                let _ = ready.send(Err(e));
                return Ok((0, None));
            }
        };
        let rate = session.fmt.rate;
        let mut track = Track::new(BufWriter::with_capacity(1 << 16, input), rate);
        let _ = ready.send(Ok(()));

        let mut session = Some(session);
        let mut timeline = Timeline::default();
        let mut converted: Vec<u8> = vec![];
        let mut checked = Instant::now();
        // 새 장치의 소리 빠르기가 달라 이어 받을 수 없게 됨
        let mut gave_up = false;
        let mut write_failed = false;

        loop {
            // 멈추라는 표시를 본 뒤에도 남은 소리를 한 번 더 꺼내고 끝낸다
            let stopping = shared.stop.load(Ordering::SeqCst);
            if let Some(s) = &session {
                let drained = drain(s, |at, data, silent| {
                    timeline.apply(&shared.marks, at);
                    if silent || write_failed {
                        return;
                    }
                    if let Some(pos) = timeline.at(at) {
                        let sound = if s.fmt.is_stereo16() {
                            data
                        } else {
                            converted.clear();
                            to_stereo16(&s.fmt, data, &mut converted);
                            &converted[..]
                        };
                        write_failed = track.put(pos, sound).is_err();
                    }
                });
                if drained.is_err() {
                    // 장치가 사라졌다 (뽑힘, 꺼짐). 아래에서 새 기본 장치를 찾는다.
                    session = None;
                }
            }
            // 조용해서 소리가 오지 않는 동안: 조금 지난 시각까지의 표시를 반영하고 무음을 채워 둔다
            let settled = now() - SETTLED;
            timeline.apply(&shared.marks, settled);
            if !write_failed && let Some(pos) = timeline.at(settled) {
                write_failed = track.catch_up(pos).is_err();
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
            std::thread::sleep(POLL);
        }

        let total = shared.total.lock().unwrap_or_default();
        write_failed = write_failed || track.pad_to(total).is_err();
        let frames = track.frames;
        // 입력을 닫아야 ffmpeg가 파일을 마무리하고 끝난다
        drop(track);
        let encoded = encoder.wait().map(|s| s.success()).unwrap_or(false);
        if !encoded {
            return Err(anyhow!("소리를 변환하지 못했습니다"));
        }
        let warning = if write_failed {
            Some("소리가 중간에 끊겼습니다".to_string())
        } else if session.is_none() {
            Some("녹화 중 소리 장치가 바뀌어 일부 구간에 소리가 없습니다".to_string())
        } else {
            None
        };
        Ok((frames, warning))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16비트 값들을 바이트로
    fn bytes(v: &[i16]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
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
    fn marks_are_applied_only_up_to_the_given_time() {
        let marks: Marks = Mutex::new(VecDeque::from([(100, true), (200, false), (300, true)]));
        let mut t = Timeline::default();
        t.apply(&marks, 150);
        assert_eq!(t.at(150), Some(50));
        assert_eq!(marks.lock().len(), 2, "아직 오지 않은 시각의 표시는 남겨 둔다");
        t.apply(&marks, 250);
        assert_eq!(t.at(250), None);
        t.apply(&marks, 400);
        assert_eq!(t.at(400), Some(200));
        assert!(marks.lock().is_empty());
    }

    #[test]
    fn track_fills_gaps_and_drops_overlap() {
        // 1초에 1000묶음: 1묶음 = 10,000틱, 허용 오차 50묶음
        let mut track = Track::new(Vec::new(), 1000);
        track.put(0, &bytes(&[1, 1, 2, 2])).unwrap();
        // 거의 이어지는 소리(10묶음 차이)는 그대로 붙인다
        track.put(12 * 10_000, &bytes(&[3, 3])).unwrap();
        assert_eq!(track.frames, 3);
        // 100묶음 자리에 오면 그 사이를 무음으로 채운다
        track.put(100 * 10_000, &bytes(&[4, 4])).unwrap();
        assert_eq!(track.frames, 101);
        // 지금까지 보낸 끝(101)보다 60묶음 앞선 자리(41)에 온 소리는 겹친 만큼 버린다
        let late: Vec<i16> = (0..70).flat_map(|i| [100 + i, 100 + i]).collect();
        track.put(41 * 10_000, &bytes(&late)).unwrap();
        assert_eq!(track.frames, 111);
        // 전체 길이 0.2초(200묶음)에 맞추면 뒤는 무음
        track.pad_to(Duration::from_millis(200)).unwrap();
        assert_eq!(track.frames, 200);
        let s = samples(&track.out);
        assert_eq!(s.len(), 400);
        assert_eq!(&s[..6], &[1, 1, 2, 2, 3, 3]);
        assert!(s[6..200].iter().all(|v| *v == 0));
        assert_eq!(&s[200..204], &[4, 4, 160, 160], "겹친 60묶음은 버리고 61번째부터");
        assert_eq!(s[221], 169);
        assert!(s[222..].iter().all(|v| *v == 0));
        // 이미 더 길면 그대로 둔다 (넘치는 것은 영상에 넣을 때 자른다)
        track.pad_to(Duration::from_millis(100)).unwrap();
        assert_eq!(track.frames, 200);
    }

    #[test]
    fn quiet_stretches_are_filled_while_recording() {
        let mut track = Track::new(Vec::new(), 1000);
        // 0.4초 뒤처진 것은 그냥 둔다 (곧 올 소리가 제자리에 놓이게)
        track.catch_up(400 * 10_000).unwrap();
        assert_eq!(track.frames, 0);
        // 0.5초 넘게 뒤처지면 그 시각까지 무음
        track.catch_up(900 * 10_000).unwrap();
        assert_eq!(track.frames, 900);
        // 그 뒤에 온 소리는 제자리(1000)에
        track.put(1000 * 10_000, &bytes(&[5, 5])).unwrap();
        assert_eq!(track.frames, 1001);
        assert_eq!(&samples(&track.out)[2000..], &[5, 5]);
    }

    #[test]
    fn device_formats_become_16_bit_stereo() {
        let convert = |f: Format, data: &[u8]| {
            let mut out = vec![];
            to_stereo16(&f, data, &mut out);
            samples(&out)
        };
        // 실수 2채널: 범위를 넘으면 끝값으로
        let data: Vec<u8> = [0.5f32, -0.5, 2.0, -2.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let f = Format { sample: Sample::F32, channels: 2, rate: 48000 };
        assert!(!f.is_stereo16());
        assert_eq!(convert(f, &data), [16384, -16384, 32767, -32767]);
        // 16비트 1채널: 양쪽에 똑같이
        let f = Format { sample: Sample::I16, channels: 1, rate: 44100 };
        assert!(!f.is_stereo16());
        assert_eq!(convert(f, &1000i16.to_le_bytes()), [1000, 1000]);
        // 24비트 2채널: 음수 부호가 살아야 한다
        let f = Format { sample: Sample::I24, channels: 2, rate: 48000 };
        assert_eq!(convert(f, &[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0]), [16384, -16384]);
        // 5.1: 가운데 소리가 양쪽에 섞인다
        let data: Vec<u8> = [0.0f32, 0.0, 0.5, 0.9, 0.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let out = convert(Format { sample: Sample::F32, channels: 6, rate: 48000 }, &data);
        assert_eq!(out[0], out[1]);
        assert!((out[0] - 11583).abs() <= 1, "{}", out[0]);
        // 16비트 2채널은 바꿀 것이 없다
        assert!(Format { sample: Sample::I16, channels: 2, rate: 48000 }.is_stereo16());
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

    /// 이 PC의 실제 출력 장치에서 잠깐 받아 본다. ffmpeg나 장치가 없으면(원격 접속 등) 건너뛴다.
    #[test]
    fn real_device_gives_a_sound_file_of_the_asked_length() {
        if crate::export::find_ffmpeg().is_none() {
            return;
        }
        let path = std::env::temp_dir().join(format!("opencam_audio_{}_real.tmp", std::process::id()));
        let cap = match AudioCapture::start(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("소리 장치가 없어 건너뜀: {e:#}");
                return;
            }
        };
        cap.mark(now(), true);
        std::thread::sleep(Duration::from_millis(600));
        cap.mark(now(), false);
        let file = cap.finish(Duration::from_millis(600)).unwrap();
        // 0.6초는 채우고, 넘쳐도 조금만
        assert!(file.frames >= 8000 * 6 / 10, "{}", file.frames);
        assert_eq!(file.warning, None);
        let (has_audio, secs) = crate::export::probe_audio(&path);
        assert!(has_audio);
        assert!((0.55..0.8).contains(&secs), "{secs}초");
        drop(file);
        assert!(!path.exists(), "다 쓴 소리 파일은 지운다");
    }

    #[test]
    fn a_capture_that_never_finished_leaves_no_file() {
        if crate::export::find_ffmpeg().is_none() {
            return;
        }
        let path = std::env::temp_dir().join(format!("opencam_audio_{}_dropped.tmp", std::process::id()));
        let Ok(cap) = AudioCapture::start(&path) else {
            return;
        };
        drop(cap);
        assert!(!path.exists());
    }
}
