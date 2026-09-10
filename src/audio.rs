//! Audio capture backends.
//!
//! Seam: `pw-record` (preferred, exact `--channels`) vs ffmpeg PulseAudio
//! fallback. Raw headerless s16le/48kHz multichannel is the interchange
//! format so partial prefixes decode (WAV length field would lie mid-write).
//! See `cap_bg`, `raw_to_mono16`, `cap_secs` in `bin/dictate`.
//!
//! Phase 1 shells out exactly like bash; binary names are struct fields so
//! tests inject fakes. Phase 2 can implement `cpal` behind `AudioBackend`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Raw capture format produced by `cap_bg`.
#[derive(Clone, Copy, Debug)]
pub struct RawFormat {
    pub channels: u32,
    pub rate: u32,
}

impl Default for RawFormat {
    fn default() -> Self {
        Self { channels: 1, rate: 48000 }
    }
}

pub trait AudioBackend {
    /// Start unbounded capture of raw PCM into `dest`; return child pid.
    /// Must honor `MAX_SECS` via `timeout -s INT` semantics.
    fn start_capture(&self, dest: &Path, fmt: RawFormat, mic: &str, max_secs: u64) -> Result<u32>;
    /// Extract `chan` from multichannel `src` to 16kHz mono `dest`.
    fn to_mono16(&self, src: &Path, dest: &Path, chan: u32) -> Result<()>;
}

/// Process backend: `timeout -s INT N pw-record ...` or ffmpeg fallback.
#[derive(Clone, Debug)]
pub struct ProcessAudio {
    pub pw_record_bin: String,
    /// `DICTATE_PW`: "0" forces ffmpeg even when pw-record exists.
    pub dictate_pw: String,
    pub ffmpeg_bin: String,
    pub timeout_bin: String,
    /// Pulse source name when pw-record is unavailable.
    pub pulse_mic: String,
}

impl ProcessAudio {
    pub fn new(pulse_mic: &str) -> Self {
        Self {
            pw_record_bin: "pw-record".into(),
            dictate_pw: std::env::var("DICTATE_PW").unwrap_or_else(|_| "1".into()),
            ffmpeg_bin: "ffmpeg".into(),
            timeout_bin: "timeout".into(),
            pulse_mic: pulse_mic.to_string(),
        }
    }

    pub fn use_pw_record(&self) -> bool {
        self.dictate_pw != "0" && lookup_in_path(&self.pw_record_bin)
    }

    /// Argv for the unbounded capture (without spawn — testable).
    /// Returns `(program, args)`.
    pub fn capture_command(&self, dest: &Path, fmt: RawFormat, mic: &str, max_secs: u64) -> (String, Vec<String>) {
        let max = max_secs.to_string();
        if self.use_pw_record() {
            (
                self.timeout_bin.clone(),
                vec![
                    "-s".into(), "INT".into(), max,
                    self.pw_record_bin.clone(),
                    "--target".into(), mic.into(),
                    "--rate".into(), fmt.rate.to_string(),
                    "--channels".into(), fmt.channels.to_string(),
                    "--format".into(), "s16".into(),
                    "--container".into(), "raw".into(),
                    dest.display().to_string(),
                ],
            )
        } else {
            (
                self.timeout_bin.clone(),
                vec![
                    "-s".into(), "INT".into(), max,
                    self.ffmpeg_bin.clone(),
                    "-nostdin".into(), "-hide_banner".into(), "-y".into(),
                    "-f".into(), "pulse".into(),
                    "-channels".into(), fmt.channels.to_string(),
                    "-i".into(), self.pulse_mic.clone(),
                    "-ar".into(), "48000".into(), "-f".into(), "s16le".into(),
                    dest.display().to_string(),
                ],
            )
        }
    }

    /// Argv for channel extraction to 16k mono (without spawn — testable).
    pub fn mono16_command(&self, src: &Path, dest: &Path, chan: u32, channels: u32) -> (String, Vec<String>) {
        let _ = channels;
        (
            self.ffmpeg_bin.clone(),
            vec![
                "-nostdin".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(),
                "-f".into(), "s16le".into(), "-ar".into(), "48000".into(),
                "-ac".into(), channels.to_string(),
                "-i".into(), src.display().to_string(),
                "-af".into(), format!("pan=mono|c0=c{chan}"),
                "-ar".into(), "16000".into(), "-ac".into(), "1".into(),
                "-c:a".into(), "pcm_s16le".into(), "-y".into(), dest.display().to_string(),
            ],
        )
    }

    /// Run channel extraction with an explicit channel count (pipeline use;
    /// `to_mono16` reads the count from the environment instead).
    pub fn mono16_with_channels(&self, src: &Path, dest: &Path, chan: u32, channels: u32) -> Result<()> {
        let (prog, args) = self.mono16_command(src, dest, chan, channels);
        let status = std::process::Command::new(prog).args(args).status()?;
        if !status.success() {
            anyhow::bail!("dictate: could not extract channel {chan}");
        }
        Ok(())
    }

    /// Measure `max_volume` dB via volumedetect (silence gate, ~30ms).
    /// Returns `None` when unparseable (caller treats as "no gate").
    pub fn measure_peak_db(&self, wav: &Path) -> Option<f32> {
        let out = std::process::Command::new(&self.ffmpeg_bin)
            .args(["-nostdin", "-hide_banner", "-i", &wav.to_string_lossy(), "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        parse_volumedetect_peak(&stderr)
    }
}

/// Parse `max_volume: -12.3 dB` (or `-inf dB`) from ffmpeg stderr.
pub fn parse_volumedetect_peak(stderr: &str) -> Option<f32> {
    for line in stderr.lines() {
        if let Some(pos) = line.find("max_volume:") {
            let rest = line[pos + "max_volume:".len()..].trim();
            let num = rest.split_whitespace().next()?;
            if num == "-inf" || num == "n/a" {
                return Some(f32::NEG_INFINITY);
            }
            if let Ok(v) = num.parse::<f32>() {
                return Some(v);
            }
        }
    }
    None
}

/// Parse `(peak, rms)` from `astats=metadata=1` output (level/scan).
/// volumedetect clamps at the 16-bit floor, so astats is required to tell
/// quiet mics apart from dead inputs (see bash comments).
pub fn parse_astats_peak_rms(stderr: &str) -> (Option<f32>, Option<f32>) {
    let mut peak = None;
    let mut rms = None;
    for line in stderr.lines() {
        if line.contains("Peak level dB") {
            peak = line.split_whitespace().last().and_then(|s| s.parse().ok());
        } else if line.contains("RMS level dB") {
            rms = line.split_whitespace().last().and_then(|s| s.parse().ok());
        }
    }
    (peak, rms)
}

fn lookup_in_path(bin: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else { return false };
    for dir in std::env::split_paths(&path_var) {
        let c = dir.join(bin);
        if c.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = std::fs::metadata(&c) {
                    if md.permissions().mode() & 0o111 != 0 {
                        return true;
                    }
                    continue;
                }
            }
            #[cfg(not(unix))]
            return true;
        }
    }
    false
}

impl AudioBackend for ProcessAudio {
    fn start_capture(&self, dest: &Path, fmt: RawFormat, mic: &str, max_secs: u64) -> Result<u32> {
        let (prog, args) = self.capture_command(dest, fmt, mic, max_secs);
        let log = log_path();
        let logfile = std::fs::OpenOptions::new().create(true).append(true).open(&log)
            .with_context(|| format!("dictate: cannot open {}", log.display()))?;
        let child = std::process::Command::new(prog)
            .args(args)
            .stdout(logfile.try_clone()?)
            .stderr(logfile)
            .spawn()
            .context("dictate: recorder failed to start")?;
        Ok(child.id())
    }

    fn to_mono16(&self, src: &Path, dest: &Path, chan: u32) -> Result<()> {
        let channels = self.channels_hint();
        let (prog, args) = self.mono16_command(src, dest, chan, channels);
        let status = std::process::Command::new(prog).args(args).status()?;
        if !status.success() {
            anyhow::bail!("dictate: could not extract channel {chan}");
        }
        Ok(())
    }
}

impl ProcessAudio {
    fn channels_hint(&self) -> u32 {
        std::env::var("DICTATE_CHANNELS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
    }
}

fn log_path() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dictate/dictate.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_audio() -> ProcessAudio {
        ProcessAudio {
            pw_record_bin: "definitely-not-pw-record-xyz".into(),
            dictate_pw: "1".into(),
            ffmpeg_bin: "ffmpeg".into(),
            timeout_bin: "timeout".into(),
            pulse_mic: "@DEFAULT_SOURCE@".into(),
        }
    }

    #[test]
    fn ffmpeg_fallback_when_no_pw_record() {
        let a = proc_audio();
        assert!(!a.use_pw_record());
        let (prog, args) = a.capture_command(
            Path::new("/tmp/r.pcm"),
            RawFormat { channels: 10, rate: 48000 },
            "mic", 300,
        );
        assert_eq!(prog, "timeout");
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"INT".to_string()));
        assert!(args.contains(&"pulse".to_string()));
        assert!(args.contains(&"10".to_string()));
    }

    #[test]
    fn dictate_pw_zero_forces_ffmpeg() {
        let mut a = proc_audio();
        a.pw_record_bin = "sh".into(); // exists, but must be ignored
        a.dictate_pw = "0".into();
        assert!(!a.use_pw_record());
    }

    #[test]
    fn mono16_extracts_selected_channel_at_16k() {
        let a = proc_audio();
        let (_, args) = a.mono16_command(Path::new("in.pcm"), Path::new("o.wav"), 6, 10);
        let af = args.windows(2).find(|w| w[0] == "-af").unwrap()[1].clone();
        assert_eq!(af, "pan=mono|c0=c6");
        assert!(args.windows(2).any(|w| w[0] == "-ar" && w[1] == "16000"));
    }

    #[test]
    fn volumedetect_parsing() {
        let err = "[Parsed_volumedetect_0 @ 0x] n: 100 vol: -20.5 vol: -30.1 max_volume: -12.3 dB mean_volume: -25.0 dB\n";
        assert_eq!(parse_volumedetect_peak(err), Some(-12.3));
        assert_eq!(parse_volumedetect_peak("max_volume: -inf dB"), Some(f32::NEG_INFINITY));
        assert_eq!(parse_volumedetect_peak("nothing here"), None);
    }

    #[test]
    fn astats_parsing() {
        let err = "[Parsed_astats_1 @ 0x] Peak level dB: -8.2\n[Parsed_astats_1 @ 0x] RMS level dB: -22.7\n";
        assert_eq!(parse_astats_peak_rms(err), (Some(-8.2), Some(-22.7)));
    }
}
