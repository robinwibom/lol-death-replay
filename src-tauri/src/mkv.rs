//! Matroska, as ffmpeg writes the screen recording into a pipe: a header (the EBML header, the
//! start of the Segment, and the Info and Tracks a reader needs before anything else) and then one
//! Cluster per frame (`-cluster_size_limit 0`). recorder.rs keeps the stream and decides where
//! clips start. Matroska carries any codec ffmpeg encodes (H.264, AV1, HEVC), and a frame's
//! keyframe flag and timestamp sit right at the start of its cluster.

/// The Segment, which holds everything else. On a pipe its size is unknown.
pub const SEGMENT: u32 = 0x1853_8067;
pub const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
/// ffmpeg always writes timestamps in milliseconds (a TimestampScale of 1,000,000 ns).
pub const MKV_HZ: f64 = 1000.0;

/// An element's header: its id, how long the header is, and the size of what follows (`None`
/// for "unknown", which ffmpeg writes for the Segment when it can't seek back to fill it in).
#[derive(Debug, PartialEq)]
pub struct Element {
    pub id: u32,
    pub header_len: usize,
    pub size: Option<u64>,
}

/// A variable-length integer: its value (with the length marker bit kept, as element ids are
/// written, or cleared, as sizes are) and how many bytes it takes. `None` until all of it is there.
fn vint(data: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None; // 0x00: not a valid start; never written by ffmpeg
    }
    let bytes = data.get(..len)?;
    let marker = if keep_marker { first } else { first & (0xFF_u16 >> len) as u8 };
    let value = bytes[1..].iter().fold(u64::from(marker), |v, &b| v << 8 | u64::from(b));
    Some((value, len))
}

/// The header of the element starting at `data[0]`, or `None` if it isn't all there yet.
pub fn element(data: &[u8]) -> Option<Element> {
    let (id, id_len) = vint(data, true)?;
    let (size, size_len) = vint(&data[id_len..], false)?;
    let unknown = size == (1 << (7 * size_len)) - 1; // all ones
    Some(Element { id: id as u32, header_len: id_len + size_len, size: (!unknown).then_some(size) })
}

/// The children of an element's body, as (id, body). Stops at anything cut off or of unknown size.
fn children(mut body: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let e = element(body)?;
        let end = e.header_len.checked_add(usize::try_from(e.size?).ok()?)?;
        let child = body.get(e.header_len..end)?;
        body = &body[end..];
        Some((e.id, child))
    })
}

/// What recorder.rs needs to know about a cluster: whether its first frame is a keyframe (so a clip
/// can start with it), and that frame's timestamp in milliseconds.
#[derive(Debug, Default, PartialEq)]
pub struct Frame {
    pub key: bool,
    pub pts: Option<u64>,
}

/// Reads the first frame of a cluster from its body (the bytes after its header).
pub fn first_frame(cluster: &[u8]) -> Frame {
    let mut timestamp = None;
    for (id, body) in children(cluster) {
        match id {
            TIMESTAMP => timestamp = Some(body.iter().fold(0u64, |v, &b| v << 8 | u64::from(b))),
            // A block: the track number, the time from the cluster's timestamp (signed 16 bits), flags.
            SIMPLE_BLOCK | BLOCK_GROUP => {
                let (block, key) = if id == SIMPLE_BLOCK {
                    (body, None)
                } else {
                    // A BlockGroup's frame is a keyframe when it refers to no other frame.
                    let block = children(body).find(|(id, _)| *id == BLOCK).map(|(_, b)| b);
                    (block.unwrap_or_default(), Some(!children(body).any(|(id, _)| id == REFERENCE_BLOCK)))
                };
                let Some((_, track_len)) = vint(block, false) else { return Frame::default() };
                let Some(head) = block.get(track_len..track_len + 3) else { return Frame::default() };
                let offset = i64::from(i16::from_be_bytes([head[0], head[1]]));
                return Frame {
                    key: key.unwrap_or(head[2] & 0x80 != 0),
                    pts: timestamp.and_then(|t| t.checked_add_signed(offset)),
                };
            }
            _ => {}
        }
    }
    Frame::default()
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// An element with a known size, its size written in 8 bytes as ffmpeg does for clusters.
    pub fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let id_bytes = id.to_be_bytes();
        let skip = id_bytes.iter().position(|&b| b != 0).unwrap();
        let size = (body.len() as u64 | 1 << 56).to_be_bytes();
        [&id_bytes[skip..], &size[..], body].concat()
    }

    /// The start of a stream: an EBML header and a Segment of unknown size holding a Tracks element.
    pub fn header() -> Vec<u8> {
        let segment_unknown = [0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let ebml = 0x1A45_DFA3; // the EBML header, the first element of every Matroska stream
        [el(ebml, b"matroska"), segment_unknown.to_vec(), el(0x1654_AE6B, b"tracks")].concat()
    }

    /// A cluster holding one frame (as a SimpleBlock) at `pts` milliseconds.
    pub fn cluster(pts: u64, key: bool) -> Vec<u8> {
        let block = [0x81, 0, 0, if key { 0x80 } else { 0 }, 0xAA, 0xBB]; // track 1, offset 0, flags, data
        el(CLUSTER, &[el(TIMESTAMP, &pts.to_be_bytes()), el(SIMPLE_BLOCK, &block)].concat())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn reads_element_headers() {
        assert_eq!(element(&[0xE7, 0x81, 5]), Some(Element { id: TIMESTAMP, header_len: 2, size: Some(1) }));
        let segment = [0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(element(&segment), Some(Element { id: SEGMENT, header_len: 12, size: None }));
        assert_eq!(element(&cluster(0, true)).unwrap().id, CLUSTER);
        assert_eq!(element(&segment[..7]), None); // cut off
        assert_eq!(element(&[]), None);
    }

    #[test]
    fn reads_the_first_frame_of_a_cluster() {
        let body = |c: Vec<u8>| c[element(&c).unwrap().header_len..].to_vec();
        assert_eq!(first_frame(&body(cluster(1234, true))), Frame { key: true, pts: Some(1234) });
        assert_eq!(first_frame(&body(cluster(99, false))), Frame { key: false, pts: Some(99) });

        // A BlockGroup is a keyframe unless it has a ReferenceBlock; the block's offset counts.
        let block = [0x81, 0x00, 0x21, 0x00, 0xAA]; // track 1, 33 ms after the cluster's timestamp
        let group = |reference: bool| {
            let mut kids = el(BLOCK, &block);
            if reference {
                kids.extend(el(REFERENCE_BLOCK, &[0xDF]));
            }
            [el(TIMESTAMP, &[0x03, 0xE8]), el(BLOCK_GROUP, &kids)].concat()
        };
        assert_eq!(first_frame(&group(false)), Frame { key: true, pts: Some(1033) });
        assert_eq!(first_frame(&group(true)), Frame { key: false, pts: Some(1033) });

        assert_eq!(first_frame(&el(TIMESTAMP, &[1])), Frame::default()); // no frame
    }
}
