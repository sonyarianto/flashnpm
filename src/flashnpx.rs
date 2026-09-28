//! `flashnpx` — short for `flashnpm exec`. Same flags as exec, no subcommand.

use flashnpm::api::{self, LogLevel};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "flashnpx",
    version,
    about = "Run a package bin, installing it when needed"
)]
struct Cli {
    /// The command to run.
    #[arg(value_name = "COMMAND")]
    command: Option<String>,
    /// Arguments passed to the command.
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<String>,
    /// Install a package for the command (repeatable).
    #[arg(short = 'p', long = "package")]
    package: Vec<String>,
    /// Run a shell line with -p packages on PATH.
    #[arg(short = 'c', long = "call")]
    call: Option<String>,
    /// Accepted for npx compatibility; no prompts exist.
    #[arg(short = 'y', long = "yes")]
    yes: bool,
    /// Project directory (default: nearest package.json above cwd).
    #[arg(long, global = true)]
    dir: Option<std::path::PathBuf>,
    /// Content store directory (default: FLASHNPM_STORE or ~/.flashnpm/store).
    #[arg(long, global = true)]
    store: Option<std::path::PathBuf>,
    /// Override the default registry.
    #[arg(long, global = true)]
    registry: Option<String>,
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Quiet: no progress notes.
    #[arg(short = 's', long = "silent", global = true)]
    quiet: bool,
    /// Only pick versions published before this date.
    #[arg(long, global = true)]
    before: Option<String>,
    /// Only pick versions published at least this many days ago (0: off).
    #[arg(long = "min-release-age", global = true)]
    min_release_age: Option<f64>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let log: Box<api::LogFn> = Box::new(move |msg, level| {
        if cli.quiet && matches!(level, LogLevel::Info) {
            return;
        }
        eprintln!("[flashnpx] {msg}");
    });
    let opts = api::ProjectOptions {
        dir: cli.dir.clone(),
        production: false,
        frozen: false,
        verify: false,
        offline: None,
        prefer_offline: None,
        registry: cli.registry.clone(),
        store: cli.store.clone(),
        min_release_age: cli.min_release_age,
        before: cli.before.clone(),
        workspace: Vec::new(),
    };
    let req = api::ExecRequest {
        command: cli.command.clone(),
        args: cli.args.clone(),
        packages: cli.package.clone(),
        call: cli.call.clone(),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    match api::exec_in(&cwd, opts, req, log).await {
        Ok(r) => std::process::exit(r.code),
        Err(e) => {
            eprintln!("flashnpx: [{}] {}", e.code, e.message);
            std::process::exit(1);
        }
    }
}
