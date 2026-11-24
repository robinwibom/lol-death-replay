//! The in-memory buffer of the game's sound: the last stretch kept, each block placed by when it was
//! heard. Kept apart from the Windows capture (audio.rs) so it can be tested without a sound card.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The format sound is kept in: 48 kHz, stereo, 16-bit. Windows converts to it from whatever the
/// game or the speakers use.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// How far the sound may drift from the clock before it's put back in step: padded with silence
/// when it falls behind (loopback sends nothing while nothing plays), trimmed when it runs ahead
/// (the sound card's clock is a little fast). Small enough that nobody hears it out of sync.
const SLACK: u64 = RATE as u64 / 50; // 20 ms

/// The last `keep` of the sound, one sample per channel per frame, frame `n` heard at
/// `origin + n / RATE`. Gaps are silence, so the position of a sample is its time.
pub struct AudioRing {
    origin: Instant,
    /// Frame number of the first sample kept.
    first: u64,
    samples: VecDeque<i16>,
    keep: u64,
}

/// Interleaved samples for `frames` frames.
fn to_samples(frames: u64) -> usize {
    frames as usize * CHANNELS
}

/// Whole frames in `samples` interleaved samples.
fn to_frames(samples: usize) -> u64 {
    (samples / CHANNELS) as u64
}

impl AudioRing {
    pub fn new(origin: Instant, keep: Duration) -> Self {
        Self { origin, first: 0, samples: VecDeque::new(), keep: frames_in(keep) }
    }

    fn frame_at(&self, t: Instant) -> u64 {
        frames_in(t.saturating_duration_since(self.origin))
    }

    fn end(&self) -> u64 {
        self.first + to_frames(self.samples.len())
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
            let gap = to_samples(want - self.end());
            self.samples.extend(std::iter::repeat_n(0, gap));
        } else if want + SLACK < self.end() {
            let overlap = to_samples(self.end() - want).min(data.len());
            data = &data[overlap..];
        }
        self.samples.extend(data);
        let frames = to_frames(self.samples.len());
        if frames > self.keep {
            let extra = frames - self.keep;
            self.samples.drain(..to_samples(extra));
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
            let silence = to_samples((self.first - start).min(self.keep));
            let mut out = vec![0; silence];
            out.extend(&self.samples);
            return out;
        }
        let skip = to_samples(start - self.first).min(self.samples.len());
        self.samples.range(skip..).copied().collect()
    }
}

fn frames_in(d: Duration) -> u64 {
    (d.as_secs_f64() * f64::from(RATE)).round() as u64
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
