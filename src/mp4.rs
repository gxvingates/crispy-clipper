//! Reads duration and video size from an MP4's moov box, no FFmpeg process needed.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct Info {
    pub seconds: f64,
    pub width: u32,
    pub height: u32,
    /// Names of the audio tracks in order (from each track's handler name).
    pub audio_tracks: Vec<String>,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// Iterates (type, body) over boxes in `buf`.
fn boxes(mut buf: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        if buf.len() < 8 {
            return None;
        }
        let (mut size, kind, mut head) = (be32(buf) as usize, &buf[4..8], 8);
        if size == 1 && buf.len() >= 16 {
            size = be64(&buf[8..]) as usize;
            head = 16;
        } else if size == 0 {
            size = buf.len();
        }
        if size < head || size > buf.len() {
            return None;
        }
        let body = &buf[head..size];
        buf = &buf[size..];
        Some((kind, body))
    })
}

pub fn info(path: &Path) -> Option<Info> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    // Walk top-level boxes to find moov (start of file with faststart, else the end).
    let mut pos = 0u64;
    let moov = loop {
        if pos + 8 > len {
            return None;
        }
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8]).ok()?;
        let mut size = be32(&h) as u64;
        let mut head = 8;
        if size == 1 {
            f.read_exact(&mut h[8..]).ok()?;
            size = be64(&h[8..]);
            head = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < head || size > 64 << 20 && &h[4..8] == b"moov" {
            return None;
        }
        if &h[4..8] == b"moov" {
            let mut body = vec![0u8; (size - head) as usize];
            f.read_exact(&mut body).ok()?;
            break body;
        }
        pos += size;
    };

    let mut out = Info { seconds: 0.0, width: 0, height: 0, audio_tracks: Vec::new() };
    for (kind, body) in boxes(&moov) {
        if kind == b"mvhd" {
            let v1 = body[0] == 1;
            let (scale, dur) = if v1 {
                (be32(&body[20..]) as f64, be64(&body[24..]) as f64)
            } else {
                (be32(&body[12..]) as f64, be32(&body[16..]) as f64)
            };
            if scale > 0.0 {
                out.seconds = dur / scale;
            }
        } else if kind == b"trak" {
            for (k, tkhd) in boxes(body) {
                if k == b"tkhd" {
                    let off = if tkhd[0] == 1 { 88 } else { 76 };
                    if tkhd.len() >= off + 8 {
                        let (w, h) = (be32(&tkhd[off..]) >> 16, be32(&tkhd[off + 4..]) >> 16);
                        if w > 0 && out.width == 0 {
                            out.width = w;
                            out.height = h;
                        }
                    }
                }
                if k == b"mdia" {
                    for (k2, hdlr) in boxes(tkhd) {
                        // hdlr: version/flags, pre_defined, handler_type, 12 reserved bytes, name.
                        if k2 == b"hdlr" && hdlr.len() >= 24 && &hdlr[8..12] == b"soun" {
                            let name = String::from_utf8_lossy(&hdlr[24..]).trim_matches(char::from(0)).trim().to_string();
                            out.audio_tracks.push(name);
                        }
                    }
                }
            }
        }
    }
    Some(out)
}
