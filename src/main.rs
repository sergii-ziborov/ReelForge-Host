//! `reelforge-host` — video + photo → blur everyone except the accepted subject.

#![allow(clippy::doc_markdown, clippy::needless_pass_by_value)]

use clap::{Parser, Subcommand};
use reelforge_host::{
    DEFAULT_MODELS_DIR, HostService, PrivacyExceptOpts, dispatch, handle_jsonrpc, list_methods,
    privacy_except,
};
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "reelforge-host",
    version = VERSION,
    about = "Host process: SightLoom + Intelligence + ReelForge (one MCP)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print version.
    Version,
    /// List host MCP method names.
    Methods,
    /// One-shot JSON method dispatch.
    Dispatch {
        /// Method name.
        #[arg(long, short = 'm')]
        method: String,
        /// JSON object args.
        #[arg(long, short = 'a', default_value = "{}")]
        args: String,
    },
    /// JSON-RPC 2.0 MCP on stdio.
    Serve,
    /// Video + photo → encode (killer path).
    PrivacyExcept {
        /// Input video.
        #[arg(long)]
        video: PathBuf,
        /// Reference photo of the person to keep sharp.
        #[arg(long)]
        photo: PathBuf,
        /// Output mp4.
        #[arg(long)]
        output: PathBuf,
        /// Scratch directory.
        #[arg(long, default_value = "work")]
        work_dir: PathBuf,
        /// ONNX cache (person_detect.onnx + person_reid.onnx).
        #[arg(long, default_value = DEFAULT_MODELS_DIR)]
        models_dir: PathBuf,
        /// Extracted frames per second.
        #[arg(long, default_value_t = 5)]
        sample_fps: u32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit_code()
        }
    }
}

fn run(cli: Cli) -> reelforge_host::Result<()> {
    match cli.command {
        Commands::Version => {
            println!("reelforge-host {VERSION}");
            Ok(())
        }
        Commands::Methods => {
            for m in list_methods() {
                println!("{m}");
            }
            Ok(())
        }
        Commands::Dispatch { method, args } => {
            let args_val: Value = serde_json::from_str(&args)?;
            let mut svc = HostService::new();
            let result = dispatch(&mut svc, &method, &args_val)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Commands::Serve => serve_stdio(),
        Commands::PrivacyExcept {
            video,
            photo,
            output,
            work_dir,
            models_dir,
            sample_fps,
        } => {
            let result = privacy_except(&PrivacyExceptOpts {
                video,
                photo,
                output,
                work_dir,
                models_dir,
                sample_fps,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

fn serve_stdio() -> reelforge_host::Result<()> {
    let mut svc = HostService::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(resp) = handle_jsonrpc(&mut svc, line) {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
            if line.contains("\"shutdown\"")
                && resp.get("result").and_then(Value::as_bool) == Some(true)
            {
                break;
            }
        }
    }
    Ok(())
}
