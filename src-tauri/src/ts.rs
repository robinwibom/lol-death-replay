//! MPEG-TS packets, as ffmpeg writes the screen recording: the packet id, and the timestamp of the
//! frame that starts in a packet. recorder.rs keeps the stream and decides where clips start.

pub const TS_PACKET: usize = 188;
/// The video's packet id: ffmpeg numbers streams from 0x100, and the recording has only the video.
pub const VIDEO_PID: u16 = 0x100;
/// MPEG-TS timestamps count at 90 kHz.
pub const TS_HZ: f64 = 90_000.0;

/// The packet id of `packet`.
pub fn pid(packet: &[u8; TS_PACKET]) -> u16 {
    (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
}

/// The presentation timestamp of the frame starting in `packet`, if one does.
pub fn frame_pts(packet: &[u8; TS_PACKET]) -> Option<u64> {
    // payload_unit_start: a PES packet (here, a frame) starts in this one.
    if packet[1] & 0x40 == 0 || packet[3] & 0x10 == 0 {
        return None;
    }
    let start = if packet[3] & 0x20 != 0 { 5 + usize::from(packet[4]) } else { 4 };
    let pes = packet.get(start..start + 14)?;
    if pes[..3] != [0, 0, 1] || pes[7] & 0x80 == 0 {
        return None; // not a PES header, or one without a timestamp
    }
    let p = &pes[9..14];
    Some(
        (u64::from(p[0] >> 1 & 7) << 30)
            | (u64::from(p[1]) << 22)
            | (u64::from(p[2] >> 1) << 15)
            | (u64::from(p[3]) << 7)
            | u64::from(p[4] >> 1),
    )
}

#[cfg(test)]
pub mod test_support {
    use super::TS_PACKET;

    #[derive(PartialEq)]
    pub enum Kind {
        /// The PAT (pid 0).
        Pat,
        /// The start of a keyframe: video with the random access indicator set.
        Key,
        /// Any other video.
        Video,
    }

    pub fn packet(kind: Kind) -> Vec<u8> {
        let mut p = vec![0xffu8; TS_PACKET];
        p[0] = 0x47;
        let pid: u16 = if kind == Kind::Pat { 0 } else { 256 };
        (p[1], p[2], p[3]) = ((pid >> 8) as u8, pid as u8, 0x10);
        if kind == Kind::Key {
            (p[3], p[4], p[5]) = (0x30, 1, 0x40);
        }
        p
    }

    /// `p` as the start of a frame with timestamp `pts`: a PES header after any adaptation field.
    pub fn with_pts(mut p: Vec<u8>, pts: u64) -> Vec<u8> {
        p[1] |= 0x40;
        let start = if p[3] & 0x20 != 0 { 5 + usize::from(p[4]) } else { 4 };
        let header = [
            0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5, // start code, video, PTS only
            (0x21 | (pts >> 29) & 0x0e) as u8,
            (pts >> 22) as u8,
            ((pts >> 14) & 0xfe | 1) as u8,
            (pts >> 7) as u8,
            ((pts << 1) & 0xfe | 1) as u8,
        ];
        p[start..start + header.len()].copy_from_slice(&header);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{packet, with_pts, Kind::*};
    use super::*;

    #[test]
    fn reads_frame_timestamps() {
        for pts in [0, 3000, 123_456_789, (1 << 33) - 1] {
            assert_eq!(frame_pts(with_pts(packet(Video), pts)[..].try_into().unwrap()), Some(pts));
            assert_eq!(frame_pts(with_pts(packet(Key), pts)[..].try_into().unwrap()), Some(pts));
        }
        assert_eq!(frame_pts(packet(Video)[..].try_into().unwrap()), None); // continues a frame
    }

    #[test]
    fn reads_packet_ids() {
        assert_eq!(pid(packet(Pat)[..].try_into().unwrap()), 0);
        assert_eq!(pid(packet(Video)[..].try_into().unwrap()), VIDEO_PID);
    }
}
