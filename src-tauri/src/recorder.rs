//! Rolling screen recording: ffmpeg streams Matroska into this process, which keeps the last stretch
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
use crate::inputs;
use crate::settings::Settings;
use crate::mkv::{self, MKV_HZ};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Extra seconds kept beyond the longest clip, so there's always a keyframe (one a second) to start at.
const KEEP_MARGIN: f64 = 3.0;
/// AV1 encoders: the same quality in smaller clips (and a smaller buffer), on NVIDIA RTX 40+,
/// AMD RX 7000+ and Intel Arc / Core Ultra. Tried first unless H.264 is forced.
const AV1_ENCODERS: [&str; 3] = ["av1_nvenc", "av1_amf", "av1_qsv"];
/// H.264 encoders: for older graphics cards, and for clips that must play everywhere.
const H264_ENCODERS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];
/// How the recording leaves ffmpeg: Matroska on stdout (it carries any codec), one cluster per
/// frame so each frame is passed on as soon as the next one is made. See mkv.rs.
const TO_PIPE: [&str; 10] =
    ["-an", "-flush_packets", "1", "-f", "matroska", "-cluster_size_limit", "0", "-write_crc32", "0", "pipe:1"];

/// Constant quality 27 on every H.264 encoder: about 38% smaller than 23, and hard to tell apart
/// in League (each step up is ~11% smaller). It sets both clip size and the memory the buffer takes.
///
/// AV1's scales differ: av1_nvenc's CQ 33-36 looked like h264_nvenc's 26-29 (the same VMAF), so
/// 27 becomes 34. av1_amf takes AV1's own 0-255 quantizer, about 4 times the 0-63 one (34 -> 136).
/// av1_qsv's -global_quality is its ICQ level on the same 1-51 scale as H.264's, so it gets 34 too.
///
/// `-delay 0` stops NVENC holding frames back before handing them out (by default ~10 frames for
/// H.264 and ~21 for AV1 at 30 fps). A clip's start is timed by when frames arrive here
/// (`Ring::capture_origin`), so every frame held back made the sound start that much early: about
/// 1 s with AV1 in a real game.
fn encoder_args(encoder: &str) -> &'static [&'static str] {
    match encoder {
        "av1_nvenc" => &["-preset", "p4", "-rc", "vbr", "-cq", "34", "-delay", "0"],
        "av1_amf" => &["-quality", "speed", "-rc", "cqp", "-qp_i", "136", "-qp_p", "136"],
        "av1_qsv" => &["-preset", "veryfast", "-global_quality", "34"],
        "h264_nvenc" => &["-preset", "p4", "-rc", "vbr", "-cq", "27", "-delay", "0"],
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

/// Whether `encoder` runs on this PC with the arguments recording gives it, into the same
/// container: an encoder can exist and still refuse a rate control mode or quality value.
fn encoder_works(ffmpeg_exe: &Path, encoder: &str) -> bool {
    ffmpeg(ffmpeg_exe)
        .args(["-loglevel", "error", "-f", "lavfi", "-i", "color=s=256x256:d=0.2"])
        .args(["-vf", "format=nv12", "-c:v", encoder])
        .args(encoder_args(encoder))
        .args(["-f", "matroska", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The encoders to try, best first: AV1 on the GPU, then H.264 on the GPU.
fn candidates() -> impl Iterator<Item = &'static str> {
    AV1_ENCODERS.iter().chain(&H264_ENCODERS).copied()
}

/// The first GPU encoder that works on this PC, falling back to the CPU (libx264).
pub fn pick_encoder(ffmpeg_exe: &Path) -> String {
    candidates()
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

/// One Matroska cluster: with `-cluster_size_limit 0`, one frame.
struct Chunk {
    at: Instant,
    /// Starts with a keyframe, so a clip can start with this chunk.
    key: bool,
    /// The timestamp of the first frame in this chunk, and when it arrived here.
    frame: Option<(u64, Instant)>,
    bytes: Vec<u8>,
}

/// What a clip is cut from: the stream, and when its first frame was captured if that's known.
struct Cut {
    stream: Vec<u8>,
    first_frame_at: Option<Instant>,
    /// A line for the log on how the frames' timing looked (see `Ring::timing`).
    timing: Option<String>,
}

/// The Matroska stream's header and its last `keep` of clusters. A clip is the header followed by
/// the clusters from a keyframe on: to a reader, a stream that started there.
struct Ring {
    /// Everything before the first cluster: the EBML header, the Segment's start, Info and Tracks.
    header: Vec<u8>,
    chunks: VecDeque<Chunk>,
    /// An element cut off by the end of the last read, finished by a later one.
    partial: Vec<u8>,
    keep: Duration,
}

impl Ring {
    fn new(keep: Duration) -> Self {
        Self { header: Vec::new(), chunks: VecDeque::new(), partial: Vec::new(), keep }
    }

    /// Adds what ffmpeg just wrote, then forgets chunks older than `keep`.
    fn push(&mut self, data: &[u8], now: Instant) {
        self.partial.extend_from_slice(data);
        let mut done = 0;
        while let Some(e) = mkv::element(&self.partial[done..]) {
            // The Segment holds everything else, so its size is unknown on a pipe: keep its header,
            // and read what's in it element by element. (ffmpeg finishes each cluster before
            // writing it, so clusters always have a size.)
            let Some(size) = e.size.filter(|_| e.id != mkv::SEGMENT).and_then(|s| usize::try_from(s).ok()) else {
                if self.chunks.is_empty() {
                    self.header.extend_from_slice(&self.partial[done..done + e.header_len]);
                }
                done += e.header_len;
                continue;
            };
            let end = done.saturating_add(e.header_len).saturating_add(size);
            let Some(bytes) = self.partial.get(done..end) else { break }; // not all here yet
            if e.id == mkv::CLUSTER {
                let frame = mkv::first_frame(&bytes[e.header_len..]);
                self.chunks.push_back(Chunk {
                    at: now,
                    key: frame.key,
                    frame: frame.pts.map(|pts| (pts, now)),
                    bytes: bytes.to_vec(),
                });
            } else if self.chunks.is_empty() {
                self.header.extend_from_slice(bytes);
            } // else the index ffmpeg writes when it stops: not needed
            done = end;
        }
        self.partial.drain(..done);
        while self.chunks.front().is_some_and(|c| now.duration_since(c.at) > self.keep) {
            self.chunks.pop_front();
        }
    }

    /// When the frame with timestamp 0 was captured, and so when any frame was: its timestamp later.
    ///
    /// Frames are stamped from the capture's first frame, a moment ffmpeg doesn't report, but each
    /// arrives here a little after it was captured (converting, encoding, the pipe). So of all the
    /// frames kept, the one that came through fastest gives the closest answer: arrival minus
    /// timestamp is smallest for it. As the oldest frames are forgotten the answer follows any slow
    /// drift between clocks. It is late by the pipeline's fastest delay, which nothing here can
    /// see: a few frames at best, so an encoder must not hold frames back (`-delay 0` on NVENC).
    /// Each frame of delay is a frame of sound too early in clips.
    fn capture_origin(&self) -> Option<Instant> {
        self.chunks
            .iter()
            .filter_map(|c| c.frame)
            .filter_map(|(pts, at)| at.checked_sub(Duration::from_secs_f64(pts as f64 / MKV_HZ)))
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
            .map(|(origin, pts)| origin + Duration::from_secs_f64(pts as f64 / MKV_HZ));
        if start == self.chunks.len() {
            return Cut { stream: Vec::new(), first_frame_at: None, timing: None }; // no keyframe yet: nothing to save
        }
        let clusters = self.chunks.range(start..).flat_map(|c| c.bytes.iter().copied());
        let timing = self.timing(start);
        Cut { stream: self.header.iter().copied().chain(clusters).collect(), first_frame_at, timing }
    }

    /// For the log: how the frames' timestamps line up with when they arrived, to tell where a clip's
    /// sound and video drift apart. "Late" is arrival minus timestamp, above the fastest frame kept
    /// (the one `capture_origin` goes by). Growing over the buffer means the timestamps run slower
    /// than the clock, or frames queue up before they reach us; either way the clip is timed too
    /// early and its sound plays late by about the clip's lateness. A delay every frame shares
    /// doesn't show here: it makes the sound early instead (see `capture_origin`).
    fn timing(&self, start: usize) -> Option<String> {
        let late = |c: &Chunk| c.frame.map(|(pts, at)| (pts as f64 / MKV_HZ, at));
        let frames: Vec<(f64, Instant)> = self.chunks.iter().filter_map(late).collect();
        let (first, last) = (frames.first()?, frames.last()?);
        let origin = self.capture_origin()?;
        let ms = |(pts, at): (f64, Instant)| {
            (at.duration_since(origin).as_secs_f64() - pts) * 1000.0
        };
        let clip: Vec<f64> = self.chunks.range(start..).filter_map(late).map(ms).collect();
        let (lo, hi) = clip.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        Some(format!(
            "Clip timing: frames {:.0}-{:.0} ms late (first {:.0}, last {:.0}); buffer: {} frames, \
             timestamps cover {:.2}s in {:.2}s of arrivals, oldest {:.0} ms late",
            lo,
            hi,
            clip.first()?,
            clip.last()?,
            frames.len(),
            last.0 - first.0,
            last.1.duration_since(first.1).as_secs_f64(),
            ms(*first),
        ))
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
    /// Keys and clicks, when they're recorded (inputs.rs).
    inputs: Option<inputs::Capture>,
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
        let inputs = s.record_inputs.then(|| inputs::Capture::start(keep, inputs::Area::of_capture(capture)));

        crate::log(&format!(
            "Recording started ({capture}, {encoder}, {} fps, {}p, {}, keeping the last {:.0}s in memory)",
            s.fps,
            s.height,
            if sound.is_some() { "with sound" } else { "no sound" },
            keep.as_secs_f64()
        ));
        Ok(Self { ffmpeg: ffmpeg_exe.to_path_buf(), capture, child: Mutex::new(Some(child)), ring, errors, sound, inputs })
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
    /// With keys and clicks recorded, they're saved beside it (inputs.rs).
    pub fn save_last(&self, seconds: f64, out: &Path) -> Result<(), String> {
        let now = Instant::now();
        let cut = self.ring.lock().unwrap().since_keyframe_before(seconds, now);
        if let Some(timing) = &cut.timing {
            crate::log(timing);
        }
        let sound = match (&self.sound, cut.first_frame_at) {
            (Some(sound), Some(starts)) => Some(sound.since(starts)),
            (Some(_), None) => {
                crate::log("Couldn't tell when the clip starts - saving it without sound");
                None
            }
            (None, _) => None,
        };
        // Empty when nothing was heard yet: no sound track rather than a silent one.
        save_stream(&self.ffmpeg, cut.stream, sound.filter(|s| !s.is_empty()).as_deref(), out)?;
        // Timed from the same first frame as the sound.
        inputs::save_beside(self.inputs.as_ref(), cut.first_frame_at, now, out);
        Ok(())
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

/// Turns Matroska bytes into an .mp4 without re-encoding the video, adding `sound` (heard from the
/// video's first frame on) as AAC.
fn save_stream(ffmpeg_exe: &Path, stream: Vec<u8>, sound: Option<&[i16]>, out: &Path) -> Result<(), String> {
    if stream.is_empty() {
        return Err("no recording yet".into());
    }
    let sound = sound.map(sound_file).transpose()?;
    let mut cmd = ffmpeg(ffmpeg_exe);
    cmd.args(["-loglevel", "error", "-y", "-f", "matroska", "-i", "pipe:0"]);
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

    use crate::mkv::test_support::{cluster, header};

    #[test]
    fn ring_keeps_recent_frames_and_cuts_at_keyframes() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut ring = Ring::new(Duration::from_secs(5));
        ring.push(&header(), t0);
        // Two frames a second: a keyframe on each second, another frame half a second later.
        for i in 0..10u32 {
            let s = f64::from(i);
            ring.push(&cluster(u64::from(i) * 1000, true), at(s));
            ring.push(&cluster(u64::from(i) * 1000 + 500, false), at(s + 0.5));
        }
        // Frames more than 5s older than the newest (9.5) are gone, so 4.5 is the oldest left.
        assert_eq!(ring.chunks.front().unwrap().at, at(4.5));
        assert_eq!(ring.header, header()); // kept, though it's older

        // The last 2s from 9.5: the keyframe at 7.0 is the last one at or before 7.5, so the
        // header and then the frames from 7, 7.5, 8, 8.5, 9 and 9.5.
        let frames = |from: u64| (from..10).flat_map(|i| [cluster(i * 1000, true), cluster(i * 1000 + 500, false)]);
        let last = ring.since_keyframe_before(2.0, at(9.5)).stream;
        assert_eq!(last, [header()].into_iter().chain(frames(7)).collect::<Vec<_>>().concat());

        // More than is kept: from the oldest keyframe (5.0), skipping 4.5 which isn't one.
        let all = ring.since_keyframe_before(60.0, at(9.5)).stream;
        assert_eq!(all, [header()].into_iter().chain(frames(5)).collect::<Vec<_>>().concat());
    }

    #[test]
    fn nothing_to_cut_before_the_first_keyframe() {
        let now = Instant::now();
        let mut ring = Ring::new(Duration::from_secs(5));
        ring.push(&header(), now);
        assert!(ring.since_keyframe_before(1.0, now).stream.is_empty());
        ring.push(&cluster(0, false), now);
        assert!(ring.since_keyframe_before(1.0, now).stream.is_empty());
    }

    #[test]
    fn works_out_when_frames_were_captured() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut ring = Ring::new(Duration::from_secs(5));
        ring.push(&header(), t0);
        // Frames 1/30 s apart, from a capture that started at t0 + 5 ms, arriving 25, 5 and 20 ms late.
        ring.push(&cluster(1000, true), at(0.005 + 1.000 + 0.025));
        ring.push(&cluster(1033, false), at(0.005 + 1.033 + 0.005));
        ring.push(&cluster(1067, true), at(0.005 + 1.067 + 0.020));
        // The fastest frame says the capture started at 5 + 5 = 10 ms: off by its 5 ms of latency.
        let origin = ring.capture_origin().unwrap();
        assert!((origin.duration_since(t0).as_secs_f64() - 0.010).abs() < 1e-6);
        // A clip from the last keyframe starts at its timestamp after that.
        let cut = ring.since_keyframe_before(0.0, at(1.1));
        assert_eq!(cut.stream, [header(), cluster(1067, true)].concat());
        let starts = cut.first_frame_at.unwrap().duration_since(t0).as_secs_f64();
        assert!((starts - (0.010 + 1.067)).abs() < 1e-6, "{starts}");
        // The clip's one frame came 15 ms slower than the fastest; the oldest kept, 20 ms.
        let timing = cut.timing.unwrap();
        assert!(timing.contains("frames 15-15 ms late") && timing.contains("oldest 20 ms late"), "{timing}");
    }

    #[test]
    fn ring_joins_elements_split_across_reads() {
        let now = Instant::now();
        let data = [header(), cluster(0, true), cluster(33, false)].concat();
        // Cut anywhere, even inside an element's id or size, it reads the same.
        for cut in 1..data.len() {
            let mut ring = Ring::new(Duration::from_secs(5));
            ring.push(&data[..cut], now);
            ring.push(&data[cut..], now);
            assert_eq!(ring.since_keyframe_before(0.0, now).stream, data, "cut at {cut}");
        }
    }

    #[test]
    fn av1_comes_before_h264_unless_forced() {
        let all: Vec<_> = candidates().collect();
        assert_eq!(all, ["av1_nvenc", "av1_amf", "av1_qsv", "h264_nvenc", "h264_amf", "h264_qsv"]);
        assert!(all.iter().all(|e| !encoder_args(e).is_empty()));
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
        assert_keyframe_first_and_fast_start(&out);
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

    /// An .mp4 whose first frame is a keyframe (the first entry of its sync sample table is
    /// sample 1) and whose index comes before the video (`+faststart`), so it plays while loading.
    fn assert_keyframe_first_and_fast_start(clip: &Path) {
        let bytes = std::fs::read(clip).unwrap();
        let find = |name: &[u8]| bytes.windows(4).position(|w| w == name).unwrap();
        assert!(find(b"moov") < find(b"mdat"), "the index isn't at the start");
        // stss: version and flags, the number of entries, then the first keyframe's sample number.
        let stss = find(b"stss") + 4;
        assert_eq!(u32::from_be_bytes(bytes[stss + 8..stss + 12].try_into().unwrap()), 1, "starts before a keyframe");
    }

    /// Like the test above, with each AV1 encoder that works on this PC and the arguments the game
    /// is recorded with. Needs a graphics card that encodes AV1; skipped when none does.
    #[test]
    #[ignore]
    fn saves_an_av1_clip_from_memory() {
        let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries/ffmpeg-x86_64-pc-windows-msvc.exe");
        let working: Vec<_> = AV1_ENCODERS.into_iter().filter(|enc| encoder_works(&exe, enc)).collect();
        if working.is_empty() {
            eprintln!("No AV1 encoder works on this PC - skipped");
        }
        for encoder in working {
            let mut child = ffmpeg(&exe)
                .args(["-loglevel", "error", "-re", "-f", "lavfi", "-i", "color=c=red:s=1280x720:r=30:d=4"])
                .args(["-vf", "format=nv12", "-c:v", encoder])
                .args(encoder_args(encoder))
                .args(["-g", "30"])
                .args(TO_PIPE)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let ring = Mutex::new(Ring::new(Duration::from_secs(10)));
            fill(&ring, child.stdout.take().unwrap());
            child.wait().unwrap();
            let ring = ring.into_inner().unwrap();

            let out = std::env::temp_dir().join(format!("ring_test_{encoder}_{}.mp4", std::process::id()));
            let tone = vec![0i16; audio_ring::RATE as usize * 2 * 3];
            save_stream(&exe, ring.since_keyframe_before(2.0, Instant::now()).stream, Some(&tone), &out).unwrap();
            let copied = ffmpeg(&exe).args(["-i"]).arg(&out).args(["-c", "copy", "-f", "null", "-"]).output().unwrap();
            assert_keyframe_first_and_fast_start(&out);
            assert!(crate::mp4::has_sound(&out));
            std::fs::remove_file(&out).unwrap();
            let info = String::from_utf8_lossy(&copied.stderr);
            assert!(copied.status.success() && info.contains("Video: av1"), "{encoder}: {info}");
            let count: u32 = info.rsplit("frame=").next().unwrap().split_whitespace().next().unwrap().parse().unwrap();
            assert!((60..=95).contains(&count), "{encoder}: {count} frames");
        }
    }
}
