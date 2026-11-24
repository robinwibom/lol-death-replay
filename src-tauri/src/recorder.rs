//! Rolling screen recording: ffmpeg streams MPEG-TS into this process, which keeps the last stretch
//! of it in memory. Nothing touches the disk until a clip is saved. The game's sound is kept beside
//! it (audio.rs) and joined to the video only in saved clips.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use crate::audio;
use crate::audio_ring;
use crate::settings::Settings;
use crate::ts::{self, TS_HZ, TS_PACKET, VIDEO_PID};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Extra seconds kept beyond the longest clip, so there's always a keyframe (one a second) to start at.
const KEEP_MARGIN: f64 = 3.0;
const HW_ENCODERS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];
/// How the recording leaves ffmpeg: MPEG-TS on stdout, each packet passed on as soon as it's made.
const TO_PIPE: [&str; 6] = ["-an", "-flush_packets", "1", "-f", "mpegts", "pipe:1"];

/// Constant quality 27 on every encoder: about 38% smaller than 23, and hard to tell apart in
/// League (each step up is ~11% smaller). It sets both clip size and the memory the buffer takes.
fn encoder_args(encoder: &str) -> &'static [&'static str] {
    match encoder {
        "h264_nvenc" => &["-preset", "p4", "-rc", "vbr", "-cq", "27"],
        "h264_amf" => &["-quality", "speed", "-rc", "cqp", "-qp_i", "27", "-qp_p", "27"],
        "h264_qsv" => &["-preset", "veryfast", "-global_quality", "27"],
        "libx264" => &["-preset", "ultrafast", "-crf", "27", "-tune", "zerolatency"],
        _ => &[],
    }
}

/// The ffmpeg shipped next to the app, or the one on PATH when running from source.
pub fn ffmpeg_path() -> PathBuf {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ffmpeg.exe")));
    match bundled {
        Some(path) if path.exists() => path,
        _ => PathBuf::from("ffmpeg"),
    }
}

/// A Windows job that kills every process in it when this app's process ends, however it ends:
/// Quit, a crash, or Ctrl+C on `cargo tauri dev`. `Drop` doesn't run in any of those, and without
/// this the recording ffmpeg would keep running on its own. The handle is never closed on purpose:
/// Windows closes it when the process exits, and that's what triggers the kill.
fn kill_on_exit_job() -> Option<usize> {
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        (ok != 0).then_some(job as usize) // as usize: raw handles aren't Send, so they can't sit in a static
    })
}

fn tie_to_app(child: &Child) {
    let assigned = kill_on_exit_job()
        .is_some_and(|job| unsafe { AssignProcessToJobObject(job as _, child.as_raw_handle() as _) != 0 });
    if !assigned {
        crate::log("Couldn't tie ffmpeg to the app; it may keep running if the app is killed");
    }
}

/// Deletes what an earlier run could leave in the temp folder: segment folders from older versions,
/// which buffered on disk, and the sound of a clip being saved when the app was killed.
///
/// Safe to do unconditionally: the app is single-instance (a second launch only focuses the first),
/// so no other copy can be recording or saving a clip whose files these are.
pub fn remove_stale_segments() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(SOUND_FILE) {
            let _ = std::fs::remove_file(entry.path());
        } else if name.starts_with("lol_replay_") && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn ffmpeg(path: &Path) -> Command {
    let mut cmd = Command::new(path);
    cmd.creation_flags(CREATE_NO_WINDOW).arg("-hide_banner");
    cmd
}

fn encoder_works(ffmpeg_exe: &Path, encoder: &str) -> bool {
    ffmpeg(ffmpeg_exe)
        .args(["-loglevel", "error", "-f", "lavfi", "-i", "color=s=256x256:d=0.2"])
        .args(["-vf", "format=nv12", "-c:v", encoder, "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The first GPU encoder that works on this PC, falling back to the CPU (libx264).
pub fn pick_encoder(ffmpeg_exe: &Path) -> String {
    HW_ENCODERS
        .into_iter()
        .find(|enc| encoder_works(ffmpeg_exe, enc))
        .unwrap_or("libx264")
        .to_string()
}

/// Capture methods, best first. When one fails, the next takes over for the rest of the game.
const CAPTURES: [&str; 3] = ["gfxcapture", "ddagrab", "gdigrab"];

/// The capture method to switch to when `capture` fails, if any is left.
pub fn next_capture(capture: &str) -> Option<&'static str> {
    let i = CAPTURES.iter().position(|c| *c == capture)?;
    CAPTURES.get(i + 1).copied()
}

// ---- The in-memory buffer ----

/// A stretch of the stream from one PAT (the table a reader needs before anything else, which
/// ffmpeg sends about every 0.1s and right before each keyframe) up to the next one.
struct Chunk {
    at: Instant,
    /// Holds the start of a keyframe, so a clip can start with this chunk.
    key: bool,
    /// The timestamp of the first frame that starts in this chunk, and when it arrived here.
    frame: Option<(u64, Instant)>,
    bytes: Vec<u8>,
}

/// What a clip is cut from: the stream, and when its first frame was captured if that's known.
struct Cut {
    stream: Vec<u8>,
    first_frame_at: Option<Instant>,
}

/// The last `keep` of the MPEG-TS stream, cut into chunks that start where a reader can join in.
struct Ring {
    chunks: VecDeque<Chunk>,
    /// A packet cut off by the end of the last read, finished by the next one.
    partial: Vec<u8>,
    keep: Duration,
}

impl Ring {
    fn new(keep: Duration) -> Self {
        Self { chunks: VecDeque::new(), partial: Vec::new(), keep }
    }

    /// Adds what ffmpeg just wrote, then forgets chunks older than `keep`.
    fn push(&mut self, data: &[u8], now: Instant) {
        self.partial.extend_from_slice(data);
        let whole = self.partial.len() / TS_PACKET * TS_PACKET;
        for packet in self.partial[..whole].as_chunks::<TS_PACKET>().0 {
            let pid = ts::pid(packet);
            if pid == 0 || self.chunks.is_empty() {
                self.chunks.push_back(Chunk { at: now, key: false, frame: None, bytes: Vec::new() });
            }
            let chunk = self.chunks.back_mut().unwrap();
            if pid == VIDEO_PID {
                // An adaptation field with the random access indicator set: how ffmpeg marks keyframes.
                if packet[3] & 0x20 != 0 && packet[4] > 0 && packet[5] & 0x40 != 0 {
                    chunk.key = true;
                }
                if chunk.frame.is_none() {
                    chunk.frame = ts::frame_pts(packet).map(|pts| (pts, now));
                }
            }
            chunk.bytes.extend_from_slice(packet);
        }
        self.partial.drain(..whole);
        while self.chunks.front().is_some_and(|c| now.duration_since(c.at) > self.keep) {
            self.chunks.pop_front();
        }
    }

    /// When the frame with timestamp 0 was captured, and so when any frame was: its timestamp later.
    ///
    /// Frames are stamped from the capture's first frame, a moment ffmpeg doesn't report, but each
    /// arrives here a little after it was captured (converting, encoding, the pipe). So of all the
    /// frames kept, the one that came through fastest gives the closest answer: arrival minus
    /// timestamp is smallest for it. That's within a few milliseconds of the real capture time,
    /// and as the oldest frames are forgotten the answer follows any slow drift between clocks.
    fn capture_origin(&self) -> Option<Instant> {
        self.chunks
            .iter()
            .filter_map(|c| c.frame)
            .filter_map(|(pts, at)| at.checked_sub(Duration::from_secs_f64(pts as f64 / TS_HZ)))
            .min()
    }

    /// The stream from the last keyframe at least `seconds` before `now` (or the oldest one kept)
    /// to the end. Starting a little early is fine; starting after a keyframe wouldn't decode.
    fn since_keyframe_before(&self, seconds: f64, now: Instant) -> Cut {
        let from = now.checked_sub(Duration::from_secs_f64(seconds)).unwrap_or(now);
        let start = self
            .chunks
            .iter()
            .rposition(|c| c.key && c.at <= from)
            .or_else(|| self.chunks.iter().position(|c| c.key))
            .unwrap_or(self.chunks.len());
        let first_pts = self.chunks.range(start..).find_map(|c| c.frame).map(|(pts, _)| pts);
        let first_frame_at = self
            .capture_origin()
            .zip(first_pts)
            .map(|(origin, pts)| origin + Duration::from_secs_f64(pts as f64 / TS_HZ));
        Cut {
            stream: self.chunks.range(start..).flat_map(|c| c.bytes.iter().copied()).collect(),
            first_frame_at,
        }
    }
}

/// Feeds everything ffmpeg writes into `ring` until the stream ends.
fn fill(ring: &Mutex<Ring>, mut from: impl Read) {
    let mut buf = vec![0u8; 64 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf) {
        ring.lock().unwrap().push(&buf[..n], Instant::now());
    }
}

/// Keeps the last `max` bytes of what ffmpeg says, for when it stops unexpectedly.
fn keep_tail(mut from: impl Read, into: Arc<Mutex<Vec<u8>>>, max: usize) {
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = from.read(&mut buf) {
        let mut tail = into.lock().unwrap();
        tail.extend_from_slice(&buf[..n]);
        let extra = tail.len().saturating_sub(max);
        tail.drain(..extra);
    }
}

pub struct Recorder {
    ffmpeg: PathBuf,
    /// The capture method actually in use: gfxcapture becomes ddagrab when the game window isn't found.
    pub capture: &'static str,
    child: Mutex<Option<Child>>,
    ring: Arc<Mutex<Ring>>,
    errors: Arc<Mutex<Vec<u8>>>,
    /// The game's sound, unless it's turned off.
    sound: Option<audio::Capture>,
}

impl Recorder {
    pub fn start(ffmpeg_exe: &Path, s: &Settings, encoder: &str) -> Result<Self, String> {
        let window = if s.capture == "gfxcapture" { crate::win::league_window() } else { None };
        if s.capture == "gfxcapture" && window.is_none() {
            crate::log("Game window not found - recording the whole monitor instead");
        }
        let (capture, input) = match (s.capture.as_str(), window) {
            // Older and slower, but works when ddagrab gives black frames.
            ("gdigrab", _) => ("gdigrab", vec![
                "-f".into(), "gdigrab".into(), "-framerate".into(), s.fps.to_string(),
                "-draw_mouse".into(), "1".into(), "-i".into(), "desktop".into(),
            ]),
            // Windows Graphics Capture of the game window alone, on whichever monitor it is, so
            // nothing on top of it (popups, this app's windows) gets recorded. width/height=-2
            // round its size down to even numbers, which the encoders need.
            ("gfxcapture", Some(hwnd)) => ("gfxcapture", vec![
                "-f".into(), "lavfi".into(), "-i".into(),
                format!("gfxcapture=hwnd={hwnd}:max_framerate={}:width=-2:height=-2,hwdownload,format=bgra", s.fps),
            ]),
            // Desktop Duplication API: fast, handles borderless and most fullscreen modes.
            _ => ("ddagrab", vec![
                "-f".into(), "lavfi".into(), "-i".into(),
                format!("ddagrab=output_idx={}:framerate={}:draw_mouse=1,hwdownload,format=bgra", s.monitor, s.fps),
            ]),
        };
        let pix = if encoder == "libx264" { "yuv420p" } else { "nv12" };
        // Never upscale: 1440p on a 1080p screen records at 1080p (ih = the captured height).
        let scale = if s.height > 0 {
            format!("scale=-2:'min({},ih)':flags=fast_bilinear,", s.height)
        } else {
            String::new()
        };
        // Room for the longest clip a death can ask for. It's memory (about 0.8 MB a second at
        // 1080p30), so the full minute is only kept with "Include the whole fight" on.
        let keep = Duration::from_secs_f64(s.max_before_death() + s.after + KEEP_MARGIN);

        let mut child = ffmpeg(ffmpeg_exe)
            .args(["-loglevel", "warning"])
            .args(&input)
            .args(["-vf", &format!("{scale}format={pix}"), "-c:v", encoder])
            .args(encoder_args(encoder))
            .args(["-g", &s.fps.to_string()]) // a keyframe every second
            .args(TO_PIPE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't start ffmpeg ({}): {e}", ffmpeg_exe.display()))?;
        tie_to_app(&child);

        let ring = Arc::new(Mutex::new(Ring::new(keep)));
        let stdout = child.stdout.take().unwrap();
        let to_ring = ring.clone();
        std::thread::spawn(move || fill(&to_ring, stdout));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().unwrap();
        let to_errors = errors.clone();
        std::thread::spawn(move || keep_tail(stderr, to_errors, 1500));
        let sound = s.record_sound.then(|| audio::Capture::start(crate::win::league_process(), keep));

        crate::log(&format!(
            "Recording started ({capture}, {encoder}, {} fps, {}p, {}, keeping the last {:.0}s in memory)",
            s.fps,
            s.height,
            if sound.is_some() { "with sound" } else { "no sound" },
            keep.as_secs_f64()
        ));
        Ok(Self { ffmpeg: ffmpeg_exe.to_path_buf(), capture, child: Mutex::new(Some(child)), ring, errors, sound })
    }

    /// Whether the sound being recorded is all the speakers' sound, not just the game's (voice chat
    /// may be in it). False when there's no sound.
    pub fn all_sound(&self) -> bool {
        self.sound.as_ref().is_some_and(|s| s.all_sound())
    }

    pub fn alive(&self) -> bool {
        match self.child.lock().unwrap().as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            // "q" lets ffmpeg stop cleanly.
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(b"q").and_then(|_| stdin.flush());
            }
            for _ in 0..50 {
                if !matches!(child.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            crate::log("Recording stopped");
        }
    }

    pub fn error_tail(&self) -> String {
        String::from_utf8_lossy(&self.errors.lock().unwrap()).into_owned()
    }

    /// Saves about the last `seconds` as an .mp4, from the keyframe just before (so up to a second more).
    pub fn save_last(&self, seconds: f64, out: &Path) -> Result<(), String> {
        let cut = self.ring.lock().unwrap().since_keyframe_before(seconds, Instant::now());
        let sound = match (&self.sound, cut.first_frame_at) {
            (Some(sound), Some(starts)) => Some(sound.since(starts)),
            (Some(_), None) => {
                crate::log("Couldn't tell when the clip starts - saving it without sound");
                None
            }
            (None, _) => None,
        };
        // Empty when nothing was heard yet: no sound track rather than a silent one.
        save_stream(&self.ffmpeg, cut.stream, sound.filter(|s| !s.is_empty()).as_deref(), out)
    }
}

/// How the files holding a clip's sound while it's saved are named, in the temp folder.
const SOUND_FILE: &str = "lol_replay_sound_";

/// A file deleted when this is dropped.
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes `sound` where ffmpeg can read it as raw PCM. A file rather than a second pipe, since the
/// video already comes in on ffmpeg's stdin and feeding two pipes would need another thread.
fn sound_file(sound: &[i16]) -> Result<TempFile, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let file = TempFile(std::env::temp_dir().join(format!("{SOUND_FILE}{}_{n}.pcm", std::process::id())));
    let bytes: Vec<u8> = sound.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&file.0, bytes).map_err(|e| format!("couldn't write the clip's sound: {e}"))?;
    Ok(file)
}

/// Turns MPEG-TS bytes into an .mp4 without re-encoding the video, adding `sound` (heard from the
/// video's first frame on) as AAC.
fn save_stream(ffmpeg_exe: &Path, stream: Vec<u8>, sound: Option<&[i16]>, out: &Path) -> Result<(), String> {
    if stream.is_empty() {
        return Err("no recording yet".into());
    }
    let sound = sound.map(sound_file).transpose()?;
    let mut cmd = ffmpeg(ffmpeg_exe);
    cmd.args(["-loglevel", "error", "-y", "-f", "mpegts", "-i", "pipe:0"]);
    if let Some(file) = &sound {
        let (rate, channels) = (audio_ring::RATE.to_string(), audio_ring::CHANNELS.to_string());
        cmd.args(["-f", "s16le", "-ar", &rate, "-ac", &channels, "-i"]).arg(&file.0);
        // Each input's timestamps start from 0, so the sound's first sample meets the first frame.
        cmd.args(["-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "128k"]);
    }
    let mut child = cmd
        .args(["-c:v", "copy", "-movflags", "+faststart"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Written from another thread, so ffmpeg can't get stuck on a full stderr while we write.
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&stream));
    let result = child.wait_with_output().map_err(|e| e.to_string())?;
    let written = writer.join().map_err(|_| "couldn't hand the clip to ffmpeg".to_string())?;
    if !result.status.success() {
        let err = String::from_utf8_lossy(&result.stderr);
        return Err(err.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect());
    }
    written.map_err(|e| e.to_string())
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_falls_back_in_order() {
        assert_eq!(next_capture("gfxcapture"), Some("ddagrab"));
        assert_eq!(next_capture("ddagrab"), Some("gdigrab"));
        assert_eq!(next_capture("gdigrab"), None);
    }

    use crate::ts::test_support::{packet, with_pts, Kind::*};

    #[test]
    fn ring_keeps_recent_chunks_starting_at_keyframes() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut ring = Ring::new(Duration::from_secs(5));
        // Every second: PAT + keyframe + video. Every half second between: PAT + video.
        for i in 0..10 {
            let s = f64::from(i);
            ring.push(&[packet(Pat), packet(Key), packet(Video)].concat(), at(s));
            ring.push(&[packet(Pat), packet(Video)].concat(), at(s + 0.5));
        }
        // Chunks more than 5s older than the newest (9.5) are gone, so 4.5 is the oldest left.
        assert_eq!(ring.chunks.front().unwrap().at, at(4.5));

        // The last 2s from 9.5: the keyframe at 7.0 is the last one at or before 7.5, so the chunks
        // from 7, 7.5, 8, 8.5, 9 and 9.5: three of 3 packets and three of 2.
        let last = ring.since_keyframe_before(2.0, at(9.5)).stream;
        assert_eq!(last.len(), 15 * TS_PACKET);
        assert_eq!(&last[..TS_PACKET], &packet(Pat)[..]); // starts at the PAT before the keyframe
        assert_eq!(&last[TS_PACKET..2 * TS_PACKET], &packet(Key)[..]);

        // More than is kept: from the oldest keyframe (5.0), skipping 4.5 which has none.
        assert_eq!(ring.since_keyframe_before(60.0, at(9.5)).stream.len(),5 * 5 * TS_PACKET); // 5 seconds of 5 packets
    }

    #[test]
    fn works_out_when_frames_were_captured() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut ring = Ring::new(Duration::from_secs(5));
        // Frames 1/30 s apart, from a capture that started at t0 + 5 ms, arriving 25, 5 and 20 ms late.
        ring.push(&[packet(Pat), with_pts(packet(Key), 90_000)].concat(), at(1.030));
        ring.push(&[packet(Pat), with_pts(packet(Video), 93_000)].concat(), at(1.005 + 1.0 / 30.0 + 0.005));
        ring.push(&[packet(Pat), with_pts(packet(Key), 96_000)].concat(), at(1.005 + 2.0 / 30.0 + 0.020));
        // The fastest frame says the capture started at 5 + 5 = 10 ms: off by its 5 ms of latency.
        let origin = ring.capture_origin().unwrap();
        assert!((origin.duration_since(t0).as_secs_f64() - 0.010).abs() < 1e-6);
        // A clip from the last keyframe starts at its timestamp after that.
        let cut = ring.since_keyframe_before(0.0, at(1.1));
        assert_eq!(cut.stream.len(), 2 * TS_PACKET);
        let starts = cut.first_frame_at.unwrap().duration_since(t0).as_secs_f64();
        assert!((starts - (0.010 + 96_000.0 / TS_HZ)).abs() < 1e-6, "{starts}");
    }

    #[test]
    fn ring_joins_packets_split_across_reads() {
        let mut ring = Ring::new(Duration::from_secs(5));
        let data = [packet(Pat), packet(Key)].concat();
        let now = Instant::now();
        ring.push(&data[..100], now);
        ring.push(&data[100..300], now);
        ring.push(&data[300..], now);
        assert_eq!(ring.since_keyframe_before(0.0, now).stream, data);
    }

    /// Records a few seconds through the real ffmpeg, the way a game is, and saves the end of it.
    /// Run with `cargo test -- --ignored` when changing how recording or saving works.
    #[test]
    #[ignore]
    fn saves_a_playable_clip_from_memory() {
        let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries/ffmpeg-x86_64-pc-windows-msvc.exe");
        let mut child = ffmpeg(&exe)
            .args(["-loglevel", "error", "-re", "-f", "lavfi", "-i", "color=c=red:s=640x360:r=30:d=4"])
            .args(["-vf", "format=yuv420p", "-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
            .args(TO_PIPE)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let ring = Mutex::new(Ring::new(Duration::from_secs(10)));
        fill(&ring, child.stdout.take().unwrap());
        child.wait().unwrap();
        let ring = ring.into_inner().unwrap();

        let out = std::env::temp_dir().join(format!("ring_test_{}.mp4", std::process::id()));
        save_stream(&exe, ring.since_keyframe_before(2.0, Instant::now()).stream, None, &out).unwrap();
        // ffmpeg reads it back without errors; its length is 2s plus up to a second to the keyframe.
        let probe = ffmpeg(&exe)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-f", "null", "-"]) // decodes every frame
            .output()
            .unwrap();
        assert!(probe.status.success() && probe.stderr.is_empty(), "{}", String::from_utf8_lossy(&probe.stderr));
        let frames = String::from_utf8_lossy(
            &ffmpeg(&exe).args(["-i"]).arg(&out).args(["-c", "copy", "-f", "null", "-"]).output().unwrap().stderr,
        )
        .to_string();
        assert!(!crate::mp4::has_sound(&out));
        std::fs::remove_file(&out).unwrap();
        let count: u32 = frames.rsplit("frame=").next().unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert!((60..=95).contains(&count), "{count} frames");

        // The same with 3 seconds of a tone beside it: an AAC track in the .mp4.
        let tone: Vec<i16> = (0..audio_ring::RATE as usize * 3)
            .flat_map(|i| [(3000.0 * (i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin()) as i16; 2])
            .collect();
        save_stream(&exe, ring.since_keyframe_before(2.0, Instant::now()).stream, Some(&tone), &out).unwrap();
        let info = ffmpeg(&exe).arg("-i").arg(&out).output().unwrap();
        assert!(crate::mp4::has_sound(&out));
        std::fs::remove_file(&out).unwrap();
        let info = String::from_utf8_lossy(&info.stderr);
        assert!(info.contains("Audio: aac") && info.contains("48000 Hz, stereo"), "{info}");
    }
}
