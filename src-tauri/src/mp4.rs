//! The .mp4 clips this app saves, read back: whether one has a sound track.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Whether an .mp4 has a sound track: a handler box of type "soun" in its movie box. Walks the
/// top-level boxes (a clip's are ftyp, moov and mdat) and reads only the movie box.
pub fn has_sound(clip: &Path) -> bool {
    let Ok(mut f) = File::open(clip) else { return false };
    let mut head = [0u8; 8];
    while f.read_exact(&mut head).is_ok() {
        let (size, header) = match u32::from_be_bytes(head[..4].try_into().unwrap()) {
            0 => return false, // runs to the end of the file: the last box, and not moov
            1 => {
                let mut big = [0u8; 8];
                if f.read_exact(&mut big).is_err() {
                    return false;
                }
                (u64::from_be_bytes(big), 16)
            }
            size => (size as u64, 8),
        };
        let Some(body) = size.checked_sub(header) else { return false };
        if &head[4..] == b"moov" {
            if body > 64 << 20 {
                return false; // not a clip of ours
            }
            let mut moov = vec![0; body as usize];
            // hdlr's body: version and flags, a reserved word, then the handler type.
            return f.read_exact(&mut moov).is_ok()
                && moov.windows(16).any(|w| &w[..4] == b"hdlr" && &w[12..] == b"soun");
        }
        if f.seek(SeekFrom::Current(body as i64)).is_err() {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat()
    }

    fn handler(kind: &[u8; 4]) -> Vec<u8> {
        mp4_box(b"hdlr", &[&[0; 8][..], kind, &[0; 13]].concat())
    }

    #[test]
    fn finds_a_sound_track() {
        let dir = std::env::temp_dir();
        let clip = |name: &str, moov: Vec<u8>| {
            let path = dir.join(format!("has_sound_{}_{name}.mp4", std::process::id()));
            let file = [mp4_box(b"ftyp", b"isom"), mp4_box(b"moov", &moov), mp4_box(b"mdat", &[0; 100])].concat();
            std::fs::write(&path, file).unwrap();
            let found = has_sound(&path);
            std::fs::remove_file(&path).unwrap();
            found
        };
        let track = |kind| mp4_box(b"trak", &mp4_box(b"mdia", &handler(kind)));
        assert!(!clip("video", track(b"vide")));
        assert!(clip("both", [track(b"vide"), track(b"soun")].concat()));
        // "soun" elsewhere in the movie box isn't a sound track.
        assert!(!clip("name", [track(b"vide"), mp4_box(b"udta", b"hdlr....soun")].concat()));
        assert!(!has_sound(Path::new("no such clip.mp4")));
    }
}
