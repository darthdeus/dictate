//! CLI dispatch — mirrors `bin/dictate` subcommands.
//!
//! Native Rust implementation (phase 1: shells out to ffmpeg/whisper/
//! xdotool like bash; phase 2 replaces backends natively behind the same
//! traits). `preview-worker` is a hidden subprocess for live previews.

use clap::{Parser, Subcommand};
use dictate_lib::{config, paths::Paths, pipeline, timings};

#[derive(Parser, Debug)]
#[command(name = "dictate", about = "Hold-to-talk speech-to-text")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Begin capture (idempotent under key autorepeat)
    Start {
        /// `llm` runs the optional cleanup pass after transcription
        mode: Option<String>,
    },
    /// Stop, transcribe, deliver to focused window
    Stop,
    /// Start if idle, stop if recording
    Toggle,
    /// Visual-only overlay walkthrough (no mic/transcribe)
    Demo,
    /// Summarize preview or release-to-paste performance
    Timings {
        /// `release` for release-to-paste, otherwise preview
        kind: Option<String>,
    },
    /// API cost and speed per setup over the last DAYS
    Stats {
        #[arg(default_value = "30")]
        days: f64,
    },
    /// Check every dependency and print what's wrong
    Doctor,
    /// Record N secs and report peak/RMS dBFS
    Level {
        #[arg(default_value = "5")]
        secs: u64,
    },
    /// Live bar meter until Ctrl-C
    Meter,
    /// Run resident whisper-server in foreground
    Serve,
    /// Run the streaming preview engine in foreground (start/stop use it)
    Daemon,
    /// Per-channel levels to find mic on multi-input interface
    Scan {
        #[arg(default_value = "6")]
        secs: u64,
    },
    /// Compare engines over the same clip
    Bench {
        /// Wav path (defaults to scratch capture)
        wav: Option<String>,
    },
    /// List recent saved dictations
    History {
        #[arg(default_value = "20")]
        n: usize,
    },
    /// Print one saved transcript and its audio path
    Show {
        #[arg(default_value = "latest")]
        id: String,
    },
    /// Deliver a saved transcript to the focused window
    Retry {
        /// Entry id or prefix (default: latest with transcript)
        id: Option<String>,
    },
    /// Re-transcribe saved dictations and compare with their saved transcripts
    Replay {
        #[arg(default_value = "20")]
        n: usize,
    },
    /// Hidden: whisper on whole clips vs on their pause segments
    #[command(hide = true)]
    Segbench {
        #[arg(short)]
        verbose: bool,
        wavs: Vec<std::path::PathBuf>,
    },
    /// Hidden: live-preview scheduler (spawned by `start`)
    #[command(hide = true)]
    PreviewWorker,
    /// Hidden: ElevenLabs shadow label for one history entry (spawned by `stop`)
    #[command(hide = true)]
    Shadow { entry: std::path::PathBuf },
    /// Hidden: time deliveries to the focused window (isolated X server only)
    #[command(hide = true)]
    PasteBench {
        #[arg(long, default_value = "20")]
        runs: usize,
        /// Simulated recognition time the prefetch overlaps, ms
        #[arg(long, default_value = "300")]
        work_ms: u64,
        /// Pause between runs, ms (let restores finish)
        #[arg(long, default_value = "1200")]
        gap_ms: u64,
        #[arg(default_value = "Paste bench text")]
        text: String,
    },
    /// Hidden: serves the dictation as the clipboard, then restores (spawned by `stop`/`retry`)
    #[command(hide = true)]
    ClipboardOwner { xclip_bin: String, copyq_bin: String },
    /// Hidden: clipboard restore worker (spawned detached by `stop`/`retry`)
    #[command(hide = true)]
    RestoreClipboard {
        backend: String,
        settle_secs: f64,
        expected_path: String,
        prev_path: String,
        xclip_bin: String,
        copyq_bin: String,
    },
}

fn ctx() -> anyhow::Result<pipeline::Ctx> {
    let env = config::Env::from_process();
    let cfg = config::load_from_env(&env)?;
    let paths = Paths::resolve(&cfg.as_env(&env))?;
    // A history dir that cannot be created degrades history to disabled
    // (bash `HISTORY_READY=0`); dictation itself must still work.
    let history_ready = paths.ensure_private_dirs()?;
    pipeline::Ctx::new(cfg, paths, history_ready)
}

fn main() -> anyhow::Result<()> {
    dictate_lib::paths::set_private_umask();
    let cli = Cli::parse();
    match cli.command {
        Command::PreviewWorker => return pipeline::run_preview_worker(&ctx()?),
        Command::Shadow { entry } => return pipeline::run_shadow(&ctx()?, &entry),
        Command::RestoreClipboard { backend, settle_secs, expected_path, prev_path, xclip_bin, copyq_bin } => {
            dictate_lib::delivery::run_restore_worker(
                &backend,
                settle_secs,
                std::path::Path::new(&expected_path),
                std::path::Path::new(&prev_path),
                &xclip_bin,
                &copyq_bin,
            );
            return Ok(());
        }
        Command::ClipboardOwner { xclip_bin, copyq_bin } => {
            dictate_lib::delivery::run_clipboard_owner(&xclip_bin, &copyq_bin);
            return Ok(());
        }
        Command::PasteBench { runs, work_ms, gap_ms, text } => {
            return pipeline::cmd_paste_bench(&ctx()?, &text, runs, work_ms, gap_ms);
        }
        Command::Start { mode } => {
            let c = ctx()?;
            pipeline::cmd_start(&c, mode.as_deref() == Some("llm"))?;
        }
        Command::Stop => {
            let c = ctx()?;
            let res = pipeline::cmd_stop(&c);
            pipeline::log_release_summary(&c);
            res?;
        }
        Command::Toggle => {
            pipeline::cmd_toggle(&ctx()?)?;
        }
        Command::Demo => {
            pipeline::cmd_demo(&ctx()?)?;
        }
        Command::Doctor => {
            let code = pipeline::cmd_doctor(&ctx()?);
            std::process::exit(code);
        }
        Command::Level { secs } => {
            pipeline::cmd_level(&ctx()?, secs)?;
        }
        Command::Meter => {
            pipeline::cmd_meter(&ctx()?)?;
        }
        Command::Daemon => {
            dictate_lib::daemon::run(&ctx)?;
        }
        Command::Serve => {
            pipeline::cmd_serve(&ctx()?)?;
        }
        Command::Scan { secs } => {
            pipeline::cmd_scan(&ctx()?, secs)?;
        }
        Command::Bench { wav } => {
            pipeline::cmd_bench(&ctx()?, wav.as_deref())?;
        }
        Command::Segbench { verbose, wavs } => {
            pipeline::cmd_segbench(&ctx()?, &wavs, verbose)?;
        }
        Command::History { n } => {
            let c = ctx()?;
            print!("{}", c.history.list(n)?);
        }
        Command::Show { id } => {
            let c = ctx()?;
            print!("{}", c.history.show(&id)?);
        }
        Command::Retry { id } => {
            pipeline::cmd_retry(&ctx()?, id.as_deref())?;
        }
        Command::Replay { n } => {
            let code = pipeline::cmd_replay(&ctx()?, n)?;
            std::process::exit(code);
        }
        Command::Stats { days } => {
            print!("{}", dictate_lib::usage::summarize(&ctx()?.state_dir(), days)?);
        }
        Command::Timings { kind } => {
            let c = ctx()?;
            if kind.as_deref() == Some("release") {
                print!("{}", timings::summarize_release(&c.paths.release_timing)?);
            } else {
                print!("{}", timings::summarize_preview(&c.paths.preview_timing)?);
            }
        }
    }
    Ok(())
}
