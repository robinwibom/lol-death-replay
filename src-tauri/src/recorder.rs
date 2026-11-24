//! Rolling screen recording: ffmpeg streams MPEG-TS into this process, which keeps the last stretch
//! of it in memory. Nothing touches the disk until a clip is saved.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use crate::settings::Settings;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Extra seconds kept beyond the longest clip, so there's always a keyframe (one a second) to start at.
const KEEP_MARGIN: f64 = 3.0;
const HW_ENCODERS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];

fn encoder_args(encoder: &str) -> &'static [&'static str] {
    match encoder {
        "h264_nvenc" => &["-preset", "p4", "-rc", "vbr", "-cq", "23"],
        "h264_amf" => &["-quality", "speed", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23"],
        "h264_qsv" => &["-preset", "veryfast", "-global_quality", "23"],
        "libx264" => &["-preset", "ultrafast", "-crf", "23", "-tune", "zerolatency"],
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

/// Deletes the segment folders that older versions, which buffered on disk, could leave in the temp folder.
pub fn remove_stale_segments() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with("lol_replay_") && entry.path().is_dir() {
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

const TS_PACKET: usize = 188;

/// A stretch of the stream from one PAT (the table a reader needs before anything else, which
/// ffmpeg sends about every 0.1s and right before each keyframe) up to the next one.
struct Chunk {
    at: Instant,
    /// Holds the start of a keyframe, so a clip can start with this chunk.
    key: bool,
    bytes: Vec<u8>,
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
            let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
            if pid == 0 || self.chunks.is_empty() {
                self.chunks.push_back(Chunk { at: now, key: false, bytes: Vec::new() });
            }
            let chunk = self.chunks.back_mut().unwrap();
            // An adaptation field with the random access indicator set: how ffmpeg marks keyframes.
            if packet[3] & 0x20 != 0 && packet[4] > 0 && packet[5] & 0x40 != 0 {
                chunk.key = true;
            }
            chunk.bytes.extend_from_slice(packet);
        }
        self.partial.drain(..whole);
        while self.chunks.front().is_some_and(|c| now.duration_since(c.at) > self.keep) {
            self.chunks.pop_front();
        }
    }

    /// The stream from the last keyframe at least `seconds` before `now` (or the oldest one kept)
    /// to the end. Starting a little early is fine; starting after a keyframe wouldn't decode.
    fn last(&self, seconds: f64, now: Instant) -> Vec<u8> {
        let from = now.checked_sub(Duration::from_secs_f64(seconds)).unwrap_or(now);
        let start = self
            .chunks
            .iter()
            .rposition(|c| c.key && c.at <= from)
            .or_else(|| self.chunks.iter().position(|c| c.key))
            .unwrap_or(self.chunks.len());
        self.chunks.range(start..).flat_map(|c| c.bytes.iter().copied()).collect()
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
        // Room for the longest clip a death can ask for. It's memory: about 1.3 MB a second at 1080p30.
        let keep = Duration::from_secs_f64(s.before + s.after + KEEP_MARGIN);

        let mut child = ffmpeg(ffmpeg_exe)
            .args(["-loglevel", "warning"])
            .args(&input)
            .args(["-vf", &format!("{scale}format={pix}"), "-c:v", encoder])
            .args(encoder_args(encoder))
            // A keyframe every second, each packet passed on as soon as it's made.
            .args(["-g", &s.fps.to_string(), "-an", "-flush_packets", "1", "-f", "mpegts", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't start ffmpeg ({}): {e}", ffmpeg_exe.display()))?;
        tie_to_app(&child);

        let ring = Arc::new(Mutex::new(Ring::new(keep)));
        let mut stdout = child.stdout.take().unwrap();
        let to_ring = ring.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = stdout.read(&mut buf) {
                to_ring.lock().unwrap().push(&buf[..n], Instant::now());
            }
        });
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().unwrap();
        let to_errors = errors.clone();
        std::thread::spawn(move || keep_tail(stderr, to_errors, 1500));

        crate::log(&format!(
            "Recording started ({capture}, {encoder}, {} fps, {}p, keeping the last {:.0}s in memory)",
            s.fps,
            s.height,
            keep.as_secs_f64()
        ));
        Ok(Self { ffmpeg: ffmpeg_exe.to_path_buf(), capture, child: Mutex::new(Some(child)), ring, errors })
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
        let stream = self.ring.lock().unwrap().last(seconds, Instant::now());
        save_stream(&self.ffmpeg, stream, out)
    }
}

/// Turns MPEG-TS bytes into an .mp4 without re-encoding.
fn save_stream(ffmpeg_exe: &Path, stream: Vec<u8>, out: &Path) -> Result<(), String> {
    if stream.is_empty() {
        return Err("no recording yet".into());
    }
    let mut child = ffmpeg(ffmpeg_exe)
        .args(["-loglevel", "error", "-y", "-f", "mpegts", "-i", "pipe:0"])
        .args(["-c", "copy", "-movflags", "+faststart"])
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

    /// A TS packet: PAT (pid 0), a keyframe start (random access indicator set) or other video.
    fn packet(kind: &str) -> Vec<u8> {
        let mut p = vec![0xffu8; TS_PACKET];
        p[0] = 0x47;
        let pid: u16 = if kind == "pat" { 0 } else { 256 };
        (p[1], p[2], p[3]) = ((pid >> 8) as u8, pid as u8, 0x10);
        if kind == "key" {
            (p[3], p[4], p[5]) = (0x30, 1, 0x40);
        }
        p
    }

    #[test]
    fn ring_keeps_recent_chunks_starting_at_keyframes() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut ring = Ring::new(Duration::from_secs(5));
        // Every second: PAT + keyframe + video. Every half second between: PAT + video.
        for i in 0..10 {
            let s = f64::from(i);
            ring.push(&[packet("pat"), packet("key"), packet("video")].concat(), at(s));
            ring.push(&[packet("pat"), packet("video")].concat(), at(s + 0.5));
        }
        // Chunks more than 5s older than the newest (9.5) are gone, so 4.5 is the oldest left.
        assert_eq!(ring.chunks.front().unwrap().at, at(4.5));

        // The last 2s from 9.5: the keyframe at 7.0 is the last one at or before 7.5, so the chunks
        // from 7, 7.5, 8, 8.5, 9 and 9.5: three of 3 packets and three of 2.
        let last = ring.last(2.0, at(9.5));
        assert_eq!(last.len(), 15 * TS_PACKET);
        assert_eq!(&last[..TS_PACKET], &packet("pat")[..]); // starts at the PAT before the keyframe
        assert_eq!(&last[TS_PACKET..2 * TS_PACKET], &packet("key")[..]);

        // More than is kept: from the oldest keyframe (5.0), skipping 4.5 which has none.
        assert_eq!(ring.last(60.0, at(9.5)).len(), 5 * 5 * TS_PACKET); // 5 seconds of 5 packets
    }

    #[test]
    fn ring_joins_packets_split_across_reads() {
        let mut ring = Ring::new(Duration::from_secs(5));
        let data = [packet("pat"), packet("key")].concat();
        let now = Instant::now();
        ring.push(&data[..100], now);
        ring.push(&data[100..300], now);
        ring.push(&data[300..], now);
        assert_eq!(ring.last(0.0, now), data);
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
            .args(["-an", "-flush_packets", "1", "-f", "mpegts", "pipe:1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ring = Ring::new(Duration::from_secs(10));
        let mut stdout = child.stdout.take().unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            ring.push(&buf[..n], Instant::now());
        }
        child.wait().unwrap();

        let out = std::env::temp_dir().join(format!("ring_test_{}.mp4", std::process::id()));
        save_stream(&exe, ring.last(2.0, Instant::now()), &out).unwrap();
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
        std::fs::remove_file(&out).unwrap();
        let count: u32 = frames.rsplit("frame=").next().unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert!((60..=95).contains(&count), "{count} frames");
    }
}
