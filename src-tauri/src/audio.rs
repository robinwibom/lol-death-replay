//! Game sound: WASAPI loopback capture into an in-memory ring, like the video's.
//!
//! The sound stays out of ffmpeg while recording. ffmpeg's screen capture stamps frames relative to
//! its own first frame, a moment this app never learns, so it couldn't line a second live input up
//! with the video. Instead every block of sound here is stamped with when it was heard, the video
//! ring works out when each frame was captured (recorder.rs), and the two meet when a clip is saved.

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

use crate::audio_ring::{AudioRing, CHANNELS, RATE};

// ---- Capture ----

/// What the capture thread shares with its owner: the ring it fills, the flag that stops it, and
/// whether the sound now is everything the speakers play rather than just the game's.
struct Shared {
    ring: Mutex<AudioRing>,
    stop: AtomicBool,
    all_sound: AtomicBool,
}

/// Records sound in the background until dropped: the game's alone when Windows can (Windows 10
/// 2004 and newer), so voice chat and music stay out; everything the speakers play otherwise.
pub struct Capture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// `game` is the game's process id, if it's known; otherwise it's looked for while recording.
    pub fn start(game: Option<u32>, keep: Duration) -> Self {
        let shared = Arc::new(Shared {
            ring: Mutex::new(AudioRing::new(Instant::now(), keep)),
            stop: AtomicBool::new(false),
            all_sound: AtomicBool::new(false),
        });
        let to_thread = shared.clone();
        let thread = std::thread::spawn(move || capture_until(game, &to_thread));
        Self { shared, thread: Some(thread) }
    }

    /// The sound from `from` up to now.
    pub fn since(&self, from: Instant) -> Vec<i16> {
        self.shared.ring.lock().unwrap().since(from)
    }

    /// Whether the sound now is everything the speakers play (the game's couldn't be recorded alone).
    pub fn all_sound(&self) -> bool {
        self.shared.all_sound.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join(); // within a wait (0.1s) of the flag
        }
    }
}

/// How long to keep looking for the game's process while recording all sound, and how often. Its
/// window can appear a while after the game is first reported.
const GAME_SEARCH: Duration = Duration::from_secs(120);
const GAME_LOOK: Duration = Duration::from_secs(1);

/// Captures until `stop`, starting over after errors (the speakers changed, say). Records all sound
/// until the game's process turns up (looked for for `GAME_SEARCH`), then the game's alone. Falls back
/// to all sound, for good, if the game's can't be opened.
fn capture_until(game: Option<u32>, shared: &Shared) {
    let _com = init_com();
    let mut game = game;
    let mut search_until = Some(Instant::now() + GAME_SEARCH);
    if game.is_none() {
        crate::log("Game process not found yet - recording all sound until it is");
    }
    let mut last_error = String::new();
    while !shared.stop.load(Ordering::Relaxed) {
        if game.is_none() && search_until.is_some_and(|t| Instant::now() < t) {
            game = crate::win::league_process();
        }
        let result = open_capture(&mut game, &mut search_until).and_then(|(client, what)| {
            if last_error.is_empty() {
                crate::log(&format!("Recording {what}"));
            }
            shared.all_sound.store(game.is_none(), Ordering::Relaxed);
            // While recording all sound, check now and then whether the game's process has appeared;
            // when it has, `pump` returns and the loop switches to the game's sound alone.
            let mut next_look = Instant::now() + GAME_LOOK;
            let mut game_appeared = || {
                let searching = game.is_none() && search_until.is_some_and(|t| Instant::now() < t);
                if !searching || Instant::now() < next_look {
                    return false;
                }
                next_look = Instant::now() + GAME_LOOK;
                // Keep the process found, so the loop opens its sound without looking again.
                game = crate::win::league_process();
                game.is_some()
            };
            unsafe { pump(&client, shared, &mut game_appeared) }
        });
        if let Err(e) = result {
            let e = e.to_string();
            if e != last_error {
                crate::log(&format!("Sound recording stopped ({e}) - retrying"));
                last_error = e;
            }
            for _ in 0..10 {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            // Recovered, so a later error is news again, and the mode is logged when it reopens.
            last_error.clear();
        }
    }
}

/// Opens the game's sound when there is a game and Windows can give it. Otherwise, or when that
/// fails (logged), opens all sound and forgets the game for good, along with the search for it.
/// Returns the client and what it records.
fn open_capture(game: &mut Option<u32>, search_until: &mut Option<Instant>) -> Result<(IAudioClient, &'static str)> {
    if let Some(pid) = *game {
        match unsafe { open_game(pid) } {
            Ok(client) => return Ok((client, "the game's sound only")),
            Err(e) => {
                crate::log(&format!("Couldn't record just the game's sound ({e}) - recording all sound instead"));
                *game = None;
                *search_until = None;
            }
        }
    }
    unsafe { open_speakers() }.map(|client| (client, "all sound"))
}

/// Initialises COM on this thread if it can be; the guard uninitialises it when dropped.
fn init_com() -> Option<ComGuard> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok().then_some(ComGuard)
}

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// The format the capture is opened in: `RATE` and `CHANNELS`, 16-bit PCM.
fn capture_format() -> WAVEFORMATEX {
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
struct ActivationDone(Mutex<mpsc::Sender<()>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.lock().unwrap().send(());
        Ok(())
    }
}
impl IAgileObject_Impl for ActivationDone_Impl {}

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
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(Mutex::new(done)).into();
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
    unsafe { initialize(&client) }?;
    Ok(client)
}

/// Everything the default speakers or headphones play.
unsafe fn open_speakers() -> Result<IAudioClient> {
    let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let speakers = unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    let client: IAudioClient = unsafe { speakers.Activate(CLSCTX_ALL, None) }?;
    unsafe { initialize(&client) }?;
    Ok(client)
}

/// Initialises `client` for the loopback capture in `capture_format`, both game and speakers alike.
unsafe fn initialize(client: &IAudioClient) -> Result<()> {
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, STREAM_FLAGS, BUFFER, 0, &capture_format(), None) }
}

/// Moves sound from `client` into the shared ring until `stop`, an error, or `done()` says to switch
/// streams.
unsafe fn pump(client: &IAudioClient, shared: &Shared, done: &mut dyn FnMut() -> bool) -> Result<()> {
    let event = unsafe { CreateEventW(None, false, false, None) }?;
    let _close = HandleGuard(event);
    unsafe { client.SetEventHandle(event) }?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }?;
    unsafe { client.Start() }?;
    let clock = QpcClock::new();
    let result = (|| {
        while !shared.stop.load(Ordering::Relaxed) && !done() {
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
                let heard = clock.packet_heard_at(qpc, frames);
                shared.ring.lock().unwrap().push(&samples, heard);
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
    fn packet_heard_at(&self, qpc: u64, frames: u32) -> Instant {
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
        let _com = init_com();
        let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
        let client: IAudioClient =
            unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None) }?;
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &capture_format(), None) }?;
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
}
