//! 16 kHz mono 16-bit WAV I/O for the clips dictate produces, plus
//! pause-aware splitting for whisper's 30 s window.

use anyhow::{bail, Result};
use std::ops::Range;
use std::path::Path;

pub const RATE: u32 = 16000;

/// Longest piece sent to whisper in one request. whisper.cpp mishandles audio
/// past its 30 s window (it duplicated one phrase and dropped the next in a
/// 37 s dictation), so longer clips go in pieces.
pub const WHISPER_PIECE_SECS: f64 = 28.0;

pub fn write_mono16(path: &Path, samples: &[i16]) -> Result<()> {
    let mut buf = Vec::new();
    encode_mono16(&mut buf, samples);
    std::fs::write(path, buf)?;
    Ok(())
}

pub fn encode_mono16(buf: &mut Vec<u8>, samples: &[i16]) {
    let data_len = (samples.len() * 2) as u32;
    buf.reserve(44 + data_len as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&RATE.to_le_bytes());
    buf.extend_from_slice(&(RATE * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
}

/// Reads 16 kHz mono 16-bit PCM, as written by `write_mono16` or ffmpeg
/// (`-ac 1 -ar 16000`). Walks the chunks, so ffmpeg's LIST chunk is skipped.
pub fn read_mono16(path: &Path) -> Result<Vec<i16>> {
    let b = std::fs::read(path)?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut i = 12;
    let mut fmt_ok = false;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32_at(i + 4) as usize;
        let body = i + 8..(i + 8).saturating_add(len).min(b.len());
        if id == b"fmt " {
            if body.len() < 16 {
                bail!("short fmt chunk");
            }
            let s = body.start;
            fmt_ok = u16_at(s) == 1 && u16_at(s + 2) == 1 && u32_at(s + 4) == RATE && u16_at(s + 14) == 16;
            if !fmt_ok {
                bail!("expected 16 kHz mono 16-bit PCM");
            }
        } else if id == b"data" {
            if !fmt_ok {
                bail!("data before fmt chunk");
            }
            return Ok(b[body].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect());
        }
        i = i.saturating_add(8 + len + (len & 1));
    }
    bail!("no data chunk")
}

/// Ranges covering `samples`, each at most `max_secs` long. Each cut lands in
/// the quietest 100 ms of the last third of its window, so it falls in a
/// pause between words rather than inside one.
pub fn split_at_pauses(samples: &[i16], max_secs: f64) -> Vec<Range<usize>> {
    let max = (max_secs * RATE as f64) as usize;
    let step = RATE as usize / 50; // 20 ms
    let window = step * 5; // 100 ms
    let mut out = Vec::new();
    let mut start = 0;
    while samples.len() - start > max {
        let hi = start + max;
        let cut = (start + max * 2 / 3..hi - window)
            .step_by(step)
            .min_by_key(|&w| energy(&samples[w..w + window]))
            .map(|w| w + window / 2)
            .unwrap_or(hi);
        out.push(start..cut);
        start = cut;
    }
    out.push(start..samples.len());
    out
}

/// Peak level in dBFS, as ffmpeg volumedetect reports it (-91 for silence).
pub fn peak_db(samples: &[i16]) -> f32 {
    let peak = samples.iter().map(|&s| (s as i32).unsigned_abs()).max().unwrap_or(0);
    if peak == 0 { -91.0 } else { 20.0 * (peak as f32 / 32768.0).log10() }
}

/// How loud the speech in a clip is: the 95th percentile of 100 ms RMS
/// levels in dBFS, so pauses don't pull it down and one shout doesn't push
/// it up. About -20 at a good gain, -32 when it is turned too low.
pub fn speech_db(samples: &[i16]) -> f32 {
    let mut levels: Vec<f32> = samples.chunks_exact(RATE as usize / 10)
        .map(|b| 10.0 * (energy(b) as f32 / b.len() as f32 / (32768.0 * 32768.0)).max(1e-10).log10())
        .collect();
    if levels.is_empty() {
        return -100.0;
    }
    levels.sort_by(f32::total_cmp);
    levels[levels.len() * 95 / 100]
}

/// Samples at (or within a hair of) full scale: the converter clipped.
pub fn clipped_samples(samples: &[i16]) -> usize {
    samples.iter().filter(|&&s| s.unsigned_abs() >= 32700).count()
}

fn energy(block: &[i16]) -> u64 {
    block.iter().map(|&s| (s as i64 * s as i64) as u64).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_level_ignores_pauses() {
        let mut s = vec![0i16; RATE as usize * 3];
        s.extend((0..RATE as usize).map(|i| if i % 2 == 0 { 3277 } else { -3277 }));
        assert!((speech_db(&s) + 20.0).abs() < 0.5, "{}", speech_db(&s));
        assert_eq!(clipped_samples(&s), 0);
        assert_eq!(clipped_samples(&[32767, -32768, 100]), 2);
    }

    fn tone(secs: f64) -> Vec<i16> {
        (0..(secs * RATE as f64) as usize).map(|i| if i % 2 == 0 { 3000 } else { -3000 }).collect()
    }

    #[test]
    fn short_clip_is_one_piece() {
        let s = tone(5.0);
        assert_eq!(split_at_pauses(&s, 28.0), vec![0..s.len()]);
    }

    #[test]
    fn long_clip_cuts_in_the_pause() {
        let mut s = tone(24.0);
        s.extend(vec![0i16; (RATE / 2) as usize]); // pause at 24.0-24.5 s
        s.extend(tone(15.5));
        let pieces = split_at_pauses(&s, 28.0);
        assert_eq!(pieces.len(), 2);
        let cut = pieces[0].end as f64 / RATE as f64;
        assert!((24.0..24.5).contains(&cut), "cut at {cut}");
        assert_eq!(pieces[1].end, s.len());
        assert!(pieces.iter().all(|r| r.len() as f64 / RATE as f64 <= 28.0));
    }

    #[test]
    fn wav_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        let s = vec![1i16, -2, 3, i16::MAX, i16::MIN];
        write_mono16(&p, &s).unwrap();
        assert_eq!(read_mono16(&p).unwrap(), s);
    }
}
