//! Game sound: WASAPI loopback capture into an in-memory ring, like the video's.
//!
//! The sound stays out of ffmpeg while recording. ffmpeg's screen capture stamps frames relative to
//! its own first frame, a moment this app never learns, so it couldn't line a second live input up
//! with the video. Instead every block of sound here is stamped with when it was heard, the video
//! ring works out when each frame was captured (recorder.rs), and the two meet when a clip is saved.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::{E_FAIL, HANDLE, WAIT_FAILED};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IAgileObject, IAgileObject_Impl, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

/// The format sound is kept in: 48 kHz, stereo, 16-bit. Windows converts to it from whatever the
/// game or the speakers use.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// How far the sound may drift from the clock before it's put back in step: padded with silence
/// when it falls behind (loopback sends nothing while nothing plays), trimmed when it runs ahead
/// (the sound card's clock is a little fast). Small enough that nobody hears it out of sync.
const SLACK: u64 = RATE as u64 / 50; // 20 ms

// ---- The in-memory buffer ----

/// The last `keep` of the sound, one sample per channel per frame, frame `n` heard at
/// `origin + n / RATE`. Gaps are silence, so the position of a sample is its time.
pub struct AudioRing {
    origin: Instant,
    /// Frame number of the first sample kept.
    first: u64,
    samples: VecDeque<i16>,
    keep: u64,
}

impl AudioRing {
    pub fn new(origin: Instant, keep: Duration) -> Self {
        Self { origin, first: 0, samples: VecDeque::new(), keep: frames_in(keep) }
    }

    fn frame_at(&self, t: Instant) -> u64 {
        frames_in(t.saturating_duration_since(self.origin))
    }

    fn end(&self) -> u64 {
        self.first + (self.samples.len() / CHANNELS) as u64
    }

    /// Adds `data` (interleaved stereo) whose first frame was heard at `at`, then forgets what's
    /// older than `keep`.
    pub fn push(&mut self, data: &[i16], at: Instant) {
        let want = self.frame_at(at);
        let mut data = data;
        if self.samples.is_empty() || want > self.end() + self.keep {
            // The first sound, or the first after a silence longer than everything kept.
            self.samples.clear();
            self.first = want;
        } else if want > self.end() + SLACK {
            let gap = (want - self.end()) as usize * CHANNELS;
            self.samples.extend(std::iter::repeat_n(0, gap));
        } else if want + SLACK < self.end() {
            let overlap = ((self.end() - want) as usize * CHANNELS).min(data.len());
            data = &data[overlap..];
        }
        self.samples.extend(data);
        let frames = (self.samples.len() / CHANNELS) as u64;
        if frames > self.keep {
            let extra = frames - self.keep;
            self.samples.drain(..extra as usize * CHANNELS);
            self.first += extra;
        }
    }

    /// The sound from `from` to the end, with silence in front for any part of it not kept.
    pub fn since(&self, from: Instant) -> Vec<i16> {
        let start = self.frame_at(from);
        if self.samples.is_empty() {
            return Vec::new();
        }
        if start < self.first {
            let silence = (self.first - start).min(self.keep) as usize * CHANNELS;
            let mut out = vec![0; silence];
            out.extend(&self.samples);
            return out;
        }
        let skip = ((start - self.first) as usize * CHANNELS).min(self.samples.len());
        self.samples.range(skip..).copied().collect()
    }
}

fn frames_in(d: Duration) -> u64 {
    (d.as_secs_f64() * f64::from(RATE)).round() as u64
}

// ---- Capture ----

/// Records sound in the background until dropped: the game's alone when Windows can (Windows 10
/// 2004 and newer), so voice chat and music stay out; everything the speakers play otherwise.
pub struct Capture {
    ring: Arc<Mutex<AudioRing>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// `game` is the game's process id, if it's known.
    pub fn start(game: Option<u32>, keep: Duration) -> Self {
        let ring = Arc::new(Mutex::new(AudioRing::new(Instant::now(), keep)));
        let stop = Arc::new(AtomicBool::new(false));
        let (to_ring, to_stop) = (ring.clone(), stop.clone());
        let thread = std::thread::spawn(move || capture_until(game, &to_ring, &to_stop));
        Self { ring, stop, thread: Some(thread) }
    }

    /// The sound from `from` up to now.
    pub fn since(&self, from: Instant) -> Vec<i16> {
        self.ring.lock().unwrap().since(from)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join(); // within a wait (0.1s) of the flag
        }
    }
}

/// Captures until `stop`, starting over after errors (the speakers changed, say). Falls back from
/// the game's sound to all sound, for good, the first time the game's can't be opened.
fn capture_until(game: Option<u32>, ring: &Mutex<AudioRing>, stop: &AtomicBool) {
    let _com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok().then_some(ComGuard);
    let mut game = game;
    if game.is_none() {
        crate::log("Game process not found - recording all sound instead of just the game's");
    }
    let mut last_error = String::new();
    while !stop.load(Ordering::Relaxed) {
        let opened = match game {
            Some(pid) => unsafe { open_game(pid) }.map(|c| (c, "the game's sound only")).or_else(|e| {
                crate::log(&format!("Couldn't record just the game's sound ({e}) - recording all sound instead"));
                game = None;
                unsafe { open_speakers() }.map(|c| (c, "all sound"))
            }),
            None => unsafe { open_speakers() }.map(|c| (c, "all sound")),
        };
        let result = opened.and_then(|(client, what)| {
            if last_error.is_empty() {
                crate::log(&format!("Recording {what}"));
            }
            unsafe { pump(&client, ring, stop) }
        });
        if let Err(e) = result {
            let e = e.to_string();
            if e != last_error {
                crate::log(&format!("Sound recording stopped ({e}) - retrying"));
                last_error = e;
            }
            for _ in 0..10 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn format() -> WAVEFORMATEX {
    let block = (CHANNELS * 2) as u16;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Shared-mode loopback in our format, Windows converting to it, woken by an event.
const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK
    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
/// The capture buffer, in 100 ns units: 0.2 s, far more than the 10 ms between wake-ups.
const BUFFER: i64 = 2_000_000;

/// Tells the activation below it's done. Agile, so Windows may call it from any thread.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(Mutex<mpsc::Sender<()>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.lock().unwrap().send(());
        Ok(())
    }
}
impl IAgileObject_Impl for Activated_Impl {}

/// A PROPVARIANT holding a VT_BLOB, laid out the same. Built by hand because the windows crate's
/// PROPVARIANT frees its blob when dropped, and this blob lives on the stack.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

/// The game's sound (and its child processes'), through Windows' per-process loopback.
unsafe fn open_game(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let variant = BlobVariant {
        vt: VT_BLOB.0,
        reserved: [0; 3],
        size: size_of_val(&params) as u32,
        data: (&raw const params).cast(),
    };
    const { assert!(size_of::<BlobVariant>() == size_of::<PROPVARIANT>()) };
    let (done, activated) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(done)).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const variant).cast()),
            &handler,
        )
    }?;
    activated
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| windows::core::Error::new(E_FAIL, "Windows didn't answer"))?;
    let (mut hr, mut client) = (HRESULT(0), None);
    unsafe { op.GetActivateResult(&mut hr, &mut client) }?;
    hr.ok()?;
    let client: IAudioClient = client.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()?;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, STREAM_FLAGS, BUFFER, 0, &format(), None) }?;
    Ok(client)
}

/// Everything the default speakers or headphones play.
unsafe fn open_speakers() -> Result<IAudioClient> {
    let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let speakers = unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    let client: IAudioClient = unsafe { speakers.Activate(CLSCTX_ALL, None) }?;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, STREAM_FLAGS, BUFFER, 0, &format(), None) }?;
    Ok(client)
}

/// Moves sound from `client` into `ring` until `stop` or an error.
unsafe fn pump(client: &IAudioClient, ring: &Mutex<AudioRing>, stop: &AtomicBool) -> Result<()> {
    let event = unsafe { CreateEventW(None, false, false, None) }?;
    let _close = HandleGuard(event);
    unsafe { client.SetEventHandle(event) }?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }?;
    unsafe { client.Start() }?;
    let clock = QpcClock::new();
    let result = (|| {
        while !stop.load(Ordering::Relaxed) {
            // Loopback may not signal at all while nothing plays, so wake up regularly anyway.
            if unsafe { WaitForSingleObject(event, 100) } == WAIT_FAILED {
                return Err(windows::core::Error::from_thread());
            }
            while unsafe { capture.GetNextPacketSize() }? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }?;
                let len = frames as usize * CHANNELS;
                let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    vec![0; len]
                } else {
                    unsafe { std::slice::from_raw_parts(data.cast::<i16>(), len) }.to_vec()
                };
                unsafe { capture.ReleaseBuffer(frames) }?;
                let heard = clock.instant(qpc, frames);
                ring.lock().unwrap().push(&samples, heard);
            }
        }
        Ok(())
    })();
    let _ = unsafe { client.Stop() };
    result
}

struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Turns the performance-counter time Windows stamps each packet with into an `Instant` (which is
/// that same counter on Windows, but can't be built from a raw reading).
struct QpcClock {
    freq: i64,
}

impl QpcClock {
    fn new() -> Self {
        let mut freq = 0;
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        Self { freq: freq.max(1) }
    }

    /// When the first of a packet's `frames` was heard. `qpc` is in 100 ns units, as WASAPI gives it;
    /// without a usable one, the packet is taken to have just finished.
    fn instant(&self, qpc: u64, frames: u32) -> Instant {
        let now = Instant::now();
        let mut counter = 0;
        let _ = unsafe { QueryPerformanceCounter(&mut counter) };
        let now_100ns = (i128::from(counter) * 10_000_000 / i128::from(self.freq)) as u64;
        match now_100ns.checked_sub(qpc) {
            Some(ago) if qpc > 0 && ago < 10_000_000 => now - Duration::from_nanos(ago * 100),
            _ => now - Duration::from_secs_f64(f64::from(frames) / f64::from(RATE)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = RATE as u64 / 1000; // frames per millisecond

    fn tone(frames: u64, value: i16) -> Vec<i16> {
        vec![value; frames as usize * CHANNELS]
    }

    #[test]
    fn keeps_sound_at_its_time() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut ring = AudioRing::new(t0, Duration::from_secs(1));
        ring.push(&tone(10 * MS, 1), at(100));
        ring.push(&tone(10 * MS, 2), at(110));
        // From 105 ms: the second half of the first packet, then the second.
        let out = ring.since(at(105));
        assert_eq!(out.len(), 15 * MS as usize * CHANNELS);
        assert_eq!(out[0], 1);
        assert_eq!(out[5 * MS as usize * CHANNELS], 2);
        // From before anything was heard: silence first.
        let out = ring.since(at(90));
        assert_eq!(out.len(), 30 * MS as usize * CHANNELS);
        assert_eq!((out[0], out[10 * MS as usize * CHANNELS]), (0, 1));
    }

    #[test]
    fn fills_gaps_with_silence_and_trims_overlaps() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut ring = AudioRing::new(t0, Duration::from_secs(1));
        ring.push(&tone(10 * MS, 1), at(0));
        // Nothing played for 90 ms (loopback sends nothing then): silence in between.
        ring.push(&tone(10 * MS, 2), at(100));
        assert_eq!(ring.end(), 110 * MS);
        assert_eq!(ring.since(at(50))[0], 0);
        assert_eq!(ring.since(at(100))[0], 2);
        // A packet 30 ms early (the sound card's clock ran fast): its overlap goes.
        ring.push(&tone(40 * MS, 3), at(80));
        assert_eq!(ring.end(), 120 * MS);
        assert_eq!(ring.since(at(110))[0], 3);
        // Within the slack, packets are just appended: a few ms of jitter isn't a gap.
        ring.push(&tone(10 * MS, 4), at(125));
        assert_eq!(ring.end(), 130 * MS);
    }

    /// Plays a quiet tone from this process and records it back through per-process loopback, the
    /// way the game's sound is. Makes a sound for a moment. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn records_a_process_sound() {
        let playing = Arc::new(AtomicBool::new(true));
        let still = playing.clone();
        let player = std::thread::spawn(move || unsafe { play_tone(&still) });
        std::thread::sleep(Duration::from_millis(300)); // the tone has started
        let capture = Capture::start(Some(std::process::id()), Duration::from_secs(5));
        let from = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let sound = capture.since(from);
        drop(capture);
        playing.store(false, Ordering::Relaxed);
        player.join().unwrap().unwrap();

        let seconds = sound.len() as f64 / (CHANNELS as f64 * f64::from(RATE));
        assert!((1.9..=2.1).contains(&seconds), "{seconds}s of sound");
        let loud = sound.iter().filter(|s| s.unsigned_abs() > 500).count() as f64 / sound.len() as f64;
        assert!(loud > 0.5, "only {:.0}% of samples carry the tone", loud * 100.0);
    }

    /// A 440 Hz tone at about 6% volume on the default speakers until `playing` turns false.
    unsafe fn play_tone(playing: &AtomicBool) -> Result<()> {
        let _com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
        let client: IAudioClient =
            unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None) }?;
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &format(), None) }?;
        let render: IAudioRenderClient = unsafe { client.GetService() }?;
        let size = unsafe { client.GetBufferSize() }?;
        unsafe { client.Start() }?;
        let mut n = 0u64;
        while playing.load(Ordering::Relaxed) {
            let free = size - unsafe { client.GetCurrentPadding() }?;
            let data = unsafe { render.GetBuffer(free) }?.cast::<i16>();
            for i in 0..free as usize {
                let v = (2000.0 * (n as f64 * 440.0 * std::f64::consts::TAU / f64::from(RATE)).sin()) as i16;
                unsafe { *data.add(i * 2) = v };
                unsafe { *data.add(i * 2 + 1) = v };
                n += 1;
            }
            unsafe { render.ReleaseBuffer(free, 0) }?;
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { client.Stop() }
    }

    #[test]
    fn forgets_what_is_older_than_keep() {
        let t0 = Instant::now();
        let mut ring = AudioRing::new(t0, Duration::from_millis(100));
        for i in 0..30 {
            ring.push(&tone(10 * MS, i), t0 + Duration::from_millis(10 * u64::from(i as u16)));
        }
        assert_eq!(ring.samples.len(), 100 * MS as usize * CHANNELS);
        assert_eq!(ring.first, 200 * MS);
        // Asking for more than is kept gives at most `keep` of silence in front.
        assert_eq!(ring.since(t0).len(), 200 * MS as usize * CHANNELS);
    }
}
