//! Live preview scheduler.
//!
//! Ports `preview_loop` + `preview_gate.py`. Current bash spawns
//! `cp $RAW` + `ffmpeg` + `curl` per tick with a Python coproc gate.
//! Rust phase 2 runs this as threads over the growing PCM file: energy gate
//! (20ms blocks, 300ms settling request, freeze while quiet) -> mono16
//! resample -> whisper-server HTTP -> overlay submit. Sequential requests
//! only, never overlapping. Timing TSV preserved.

use std::path::PathBuf;

/// 20ms block at 48kHz mono-equivalent per channel.
pub const GATE_BLOCK_SAMPLES: usize = 960;
/// Pause after voice before one settling request.
pub const GATE_SETTLE_SECS: f64 = 0.3;
/// Quiet poll interval while frozen.
pub const GATE_QUIET_POLL_SECS: f64 = 0.04;

/// Energy gate over interleaved s16le PCM. Never trims recognition audio —
/// it only decides whether to *request* a preview.
pub struct EnergyGate {
    pub channels: u32,
    pub channel: u32,
    pub threshold_abs: f32,
    pub offset_bytes: u64,
    pub last_voice: Option<f64>,
    pub pending_settle: bool,
}

impl EnergyGate {
    pub fn new(channels: u32, channel: u32, threshold_dbfs: f32) -> anyhow::Result<Self> {
        if channel >= channels {
            anyhow::bail!("Invalid audio channel");
        }
        Ok(Self {
            channels,
            channel,
            threshold_abs: 32768.0 * 10f32.powf(threshold_dbfs / 20.0),
            offset_bytes: 0,
            last_voice: None,
            pending_settle: false,
        })
    }

    /// Scan `data` (full file bytes read so far) for new above-threshold
    /// 20ms blocks on the selected channel. Pure function for testability;
    /// IO (read/tail file) stays in the caller.
    pub fn scan(&mut self, data: &[i16], now: f64) -> bool {
        let ch = self.channel as usize;
        let nch = self.channels as usize;
        let samples: Vec<i16> = data.iter().skip(ch).step_by(nch).copied().collect();
        let loud = samples.chunks(GATE_BLOCK_SAMPLES).any(|block| {
            if block.is_empty() {
                return false;
            }
            let mean_sq: f64 =
                block.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / block.len() as f64;
            mean_sq.sqrt() as f32 >= self.threshold_abs
        });
        if loud {
            self.last_voice = Some(now);
            self.pending_settle = true;
            return true;
        }
        if self.pending_settle && self.last_voice.map(|t| now - t >= GATE_SETTLE_SECS).unwrap_or(false) {
            self.pending_settle = false;
            return true;
        }
        false
    }
}

#[derive(Clone, Debug)]
pub struct PreviewConfig {
    pub interval_secs: f64,
    pub pause_secs: f64,
    pub pcm_path: PathBuf,
}

/// TSV header (rewritten each recording so the log stays bounded).
pub const TIMING_HEADER: &str =
    "started_unix\tprep_ms\trequest_ms\tfinish_ms\twork_ms\tstart_gap_ms\tupdated\tstatus\n";

/// One timing row. Invariant (checked in tests, mirrors check_preview.py):
/// `prep + request + finish == work` within 0.05ms.
pub fn format_timing_row(
    started_unix: f64,
    prep_ms: f64,
    request_ms: f64,
    finish_ms: f64,
    start_gap_ms: f64,
    updated: bool,
    status: &str,
) -> String {
    format!(
        "{:.6}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{}\t{}\n",
        started_unix,
        prep_ms,
        request_ms,
        finish_ms,
        prep_ms + request_ms + finish_ms,
        start_gap_ms,
        if updated { 1 } else { 0 },
        status
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silence(secs: f64) -> Vec<i16> {
        vec![0i16; (48000.0 * secs) as usize]
    }

    #[test]
    fn timing_row_preserves_stage_accounting() {
        let row = format_timing_row(1700000000.0, 15.0, 25.0, 5.0, 50.0, true, "ok");
        let cols: Vec<&str> = row.trim().split('\t').collect();
        assert_eq!(cols.len(), 8);
        let nums: Vec<f64> = cols[1..6].iter().map(|s| s.parse().unwrap()).collect();
        assert!((nums[0] + nums[1] + nums[2] - nums[3]).abs() < 0.05);
        assert_eq!(cols[6], "1");
        assert_eq!(cols[7], "ok");
        assert!(TIMING_HEADER.starts_with("started_unix"));
    }

    #[test]
    fn silence_requests_nothing_settling_requests_once() {
        let mut g = EnergyGate::new(1, 0, -48.0).unwrap();
        assert!(!g.scan(&silence(0.1), 0.0));
        let mut speech = silence(0.02);
        for v in &mut speech {
            *v = 1000;
        }
        assert!(g.scan(&speech, 0.1));
        assert!(!g.scan(&silence(0.02), 0.2));
        assert!(g.scan(&silence(0.02), 0.41));
        assert!(!g.scan(&silence(0.02), 0.8));
    }
}
