//! Rolling screen recording with ffmpeg: short .ts segments that overwrite the oldest ones,
//! so the last ~20 seconds are always on disk.

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use crate::settings::Settings;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SEGMENT_SECS: f64 = 1.0;
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

/// Deletes segment folders left behind by earlier runs that didn't shut down cleanly.
/// Only call this before the first recording starts: it doesn't know which folder is in use.
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

pub struct Recorder {
    ffmpeg: PathBuf,
    dir: PathBuf,
    /// The capture method actually in use: gfxcapture becomes ddagrab when the game window isn't found.
    pub capture: &'static str,
    child: Mutex<Option<Child>>,
}

impl Recorder {
    pub fn start(ffmpeg_exe: &Path, s: &Settings, encoder: &str) -> Result<Self, String> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let dir = std::env::temp_dir().join(format!("lol_replay_{stamp}"));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

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
        let wrap = ((s.before + s.after) / SEGMENT_SECS) as u32 + 3;
        let log = std::fs::File::create(dir.join("ffmpeg.log")).map_err(|e| e.to_string())?;

        let child = ffmpeg(ffmpeg_exe)
            .args(["-loglevel", "warning", "-y"])
            .args(&input)
            .args(["-vf", &format!("{scale}format={pix}"), "-c:v", encoder])
            .args(encoder_args(encoder))
            .args(["-g", &s.fps.to_string(), "-an", "-f", "segment"])
            .args(["-segment_time", &SEGMENT_SECS.to_string(), "-segment_wrap", &wrap.to_string()])
            .args(["-segment_format", "mpegts", "seg%03d.ts"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("couldn't start ffmpeg ({}): {e}", ffmpeg_exe.display()))?;
        tie_to_app(&child);

        crate::log(&format!("Recording started ({capture}, {encoder}, {} fps, {}p)", s.fps, s.height));
        Ok(Self { ffmpeg: ffmpeg_exe.to_path_buf(), dir, capture, child: Mutex::new(Some(child)) })
    }

    pub fn alive(&self) -> bool {
        match self.child.lock().unwrap().as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            // "q" lets ffmpeg finish the current segment cleanly.
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
        let text = std::fs::read_to_string(self.dir.join("ffmpeg.log")).unwrap_or_default();
        let start = text.len().saturating_sub(1500);
        text.get(start..).unwrap_or(&text).to_string()
    }

    /// Stitches the newest segments together and keeps the last `seconds`.
    pub fn save_last(&self, seconds: f64, out: &Path) -> Result<(), String> {
        let mut segs: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                (meta.len() > 0).then(|| (meta.modified().ok().unwrap_or(UNIX_EPOCH), e.path()))
            })
            .collect();
        segs.sort();
        let need = (seconds / SEGMENT_SECS) as usize + 2;
        let segs = &segs[segs.len().saturating_sub(need)..];
        if segs.is_empty() {
            return Err("no recording yet".into());
        }
        let names: Vec<String> = segs
            .iter()
            .map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let result = ffmpeg(&self.ffmpeg)
            .args(["-loglevel", "error", "-y", "-sseof", &format!("-{seconds}")])
            .args(["-i", &format!("concat:{}", names.join("|"))])
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(out)
            .current_dir(&self.dir)
            .output()
            .map_err(|e| e.to_string())?;
        if result.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&result.stderr);
            Err(err.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect())
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
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
}
