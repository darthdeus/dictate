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
    /// Hidden: live-preview scheduler (spawned by `start`)
    #[command(hide = true)]
    PreviewWorker,
}

fn ctx() -> anyhow::Result<pipeline::Ctx> {
    let env = config::Env::from_process();
    let cfg = config::load_from_env(&env)?;
    let paths = Paths::resolve(&cfg.as_env(&env))?;
    paths.ensure_private_dirs()?;
    pipeline::Ctx::new(cfg, paths)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::PreviewWorker => return pipeline::run_preview_worker(&ctx()?),
        Command::Start { mode } => {
            let c = ctx()?;
            pipeline::cmd_start(&c, mode.as_deref() == Some("llm"))?;
        }
        Command::Stop => {
            pipeline::cmd_stop(&ctx()?)?;
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
        Command::Serve => {
            pipeline::cmd_serve(&ctx()?)?;
        }
        Command::Scan { secs } => {
            pipeline::cmd_scan(&ctx()?, secs)?;
        }
        Command::Bench { wav } => {
            pipeline::cmd_bench(&ctx()?, wav.as_deref())?;
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
