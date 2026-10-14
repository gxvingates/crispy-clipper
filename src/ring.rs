//! In-RAM replay buffer of an MPEG-TS stream, indexed by video keyframes so a
//! clip can be cut out with a plain remux (no re-encode).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

const TS: usize = 188;
// PIDs pinned via -mpegts_pmt_start_pid / -mpegts_start_pid in recorder.rs.
pub const PMT_PID: u16 = 0x1000;
pub const VIDEO_PID: u16 = 0x100;

pub struct Ring {
    data: VecDeque<u8>,
    base: u64, // stream offset of data[0]
    keys: VecDeque<(Instant, u64)>,
    pat: Option<[u8; TS]>,
    pmt: Option<[u8; TS]>,
    carry: Vec<u8>,
    keep: Duration,
    /// Full-session recording: every packet is also appended here.
    tee: Option<BufWriter<File>>,
}

impl Ring {
    pub fn new(keep: Duration) -> Ring {
        Ring {
            data: VecDeque::new(),
            base: 0,
            keys: VecDeque::new(),
            pat: None,
            pmt: None,
            carry: Vec::new(),
            keep,
            tee: None,
        }
    }

    /// Drops buffered video (e.g. after FFmpeg restarts) but keeps an active recording.
    pub fn clear(&mut self) {
        let tee = self.tee.take();
        *self = Ring::new(self.keep);
        self.tee = tee;
    }

    /// Starts writing the stream to `file`, beginning at the latest keyframe.
    pub fn start_recording(&mut self, file: File) -> std::io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, file);
        if let Some(data) = self.from_key(self.keys.back().map(|k| k.1)) {
            w.write_all(&data)?;
        }
        self.tee = Some(w);
        Ok(())
    }

    pub fn stop_recording(&mut self) -> std::io::Result<()> {
        match self.tee.take() {
            Some(mut w) => w.flush(),
            None => Ok(()),
        }
    }


    pub fn push(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        let now = Instant::now();
        let mut i = 0;
        while i + TS <= self.carry.len() {
            let p = &self.carry[i..i + TS];
            if p[0] != 0x47 {
                i += 1; // resync
                continue;
            }
            let pid = (((p[1] & 0x1f) as u16) << 8) | p[2] as u16;
            let pusi = p[1] & 0x40 != 0;
            let has_af = p[3] & 0x20 != 0;
            if pid == 0 {
                self.pat = Some(p.try_into().unwrap());
            } else if pid == PMT_PID {
                self.pmt = Some(p.try_into().unwrap());
            } else if pid == VIDEO_PID && pusi && has_af && p[4] > 0 && p[5] & 0x40 != 0 {
                // random_access_indicator: FFmpeg sets it on keyframes.
                self.keys.push_back((now, self.base + self.data.len() as u64));
            }
            self.data.extend(p);
            i += TS;
        }
        if let Some(w) = &mut self.tee {
            if w.write_all(&self.carry[..i]).is_err() {
                self.tee = None; // disk full etc.; the recording is closed as-is
            }
        }
        self.carry.drain(..i);
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.keep).unwrap_or(now);
        // Keep one keyframe at or before the cutoff so a full-length clip stays available.
        while self.keys.len() > 1 && self.keys[1].0 <= cutoff {
            self.keys.pop_front();
        }
        let start = match self.keys.front() {
            Some(&(_, off)) => off,
            // No keyframe yet: don't grow without bound.
            None => (self.base + self.data.len() as u64).saturating_sub(64 << 20),
        };
        if start > self.base {
            let n = (start - self.base) as usize;
            self.data.drain(..n.min(self.data.len()));
            self.base = start;
        }
    }

    /// Bytes for the last `secs` seconds, starting on a keyframe, playable as a .ts file.
    pub fn snapshot(&self, secs: u32) -> Option<Vec<u8>> {
        let since = Instant::now().checked_sub(Duration::from_secs(secs as u64))?;
        let &(_, off) = self
            .keys
            .iter()
            .rev()
            .find(|(t, _)| *t <= since)
            .or_else(|| self.keys.front())?;
        self.from_key(Some(off))
    }

    fn from_key(&self, off: Option<u64>) -> Option<Vec<u8>> {
        let off = off?;
        let (pat, pmt) = (self.pat?, self.pmt?);
        let skip = (off - self.base) as usize;
        let mut out = Vec::with_capacity(2 * TS + self.data.len() - skip);
        out.extend_from_slice(&pat);
        out.extend_from_slice(&pmt);
        let (a, b) = self.data.as_slices();
        if skip < a.len() {
            out.extend_from_slice(&a[skip..]);
            out.extend_from_slice(b);
        } else {
            out.extend_from_slice(&b[skip - a.len()..]);
        }
        Some(out)
    }

    pub fn seconds_buffered(&self) -> u64 {
        self.keys.front().map(|(t, _)| t.elapsed().as_secs()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, key: bool) -> [u8; TS] {
        let mut p = [0xffu8; TS];
        p[0] = 0x47;
        p[1] = ((pid >> 8) as u8 & 0x1f) | if key { 0x40 } else { 0 };
        p[2] = pid as u8;
        p[3] = if key { 0x30 } else { 0x10 };
        if key {
            p[4] = 1;
            p[5] = 0x40;
        }
        p
    }

    #[test]
    fn snapshot_starts_with_headers_then_keyframe() {
        let mut r = Ring::new(Duration::from_secs(60));
        let mut s = Vec::new();
        s.extend(packet(0, false));
        s.extend(packet(PMT_PID, false));
        s.extend(packet(VIDEO_PID, false)); // before first keyframe
        s.extend(packet(VIDEO_PID, true));
        s.extend(packet(0x101, false));
        // Feed in awkward chunk sizes to exercise the carry buffer.
        for c in s.chunks(100) {
            r.push(c);
        }
        let snap = r.snapshot(0).unwrap();
        assert_eq!(snap.len(), 4 * TS); // pat + pmt + key + audio
        assert_eq!(&snap[2 * TS..3 * TS], &packet(VIDEO_PID, true));
    }
}
