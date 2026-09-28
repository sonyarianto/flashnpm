//! Command-line parsing (`clap`) and output formatting.
//! Mirrors `upm`'s `src/cli.ts` usage surface (MVP subset).

use clap::{Parser, Subcommand};

use crate::api::ProjectOptions;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "flashnpm",
    version,
    about = "Flash npm — a fast, tiny npm-registry package manager in Rust",
    allow_external_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Implied `run <script> [args...]` (e.g. `flashnpm test --watch`).
    #[arg(
        value_name = "SCRIPT-OR-ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub external: Vec<String>,

    /// Project directory (default: nearest package.json above cwd).
    #[arg(long, global = true)]
    pub dir: Option<PathBuf>,

    /// Content store directory (default: FLASHNPM_STORE or ~/.flashnpm/store).
    #[arg(long, global = true)]
    pub store: Option<PathBuf>,

    /// Override the default registry.
    #[arg(long, global = true)]
    pub registry: Option<String>,

    /// Install only non-dev packages.
    #[arg(long, global = true)]
    pub production: bool,

    /// Never resolve: stale/missing flashnpm.lock is an error.
    #[arg(long = "frozen-lockfile", global = true)]
    pub frozen: bool,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Check sizes, links and bins instead of trusting install state.
    #[arg(long, global = true)]
    pub verify: bool,

    /// Never use the network.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Pick from cached registry docs without revalidating.
    #[arg(long = "prefer-offline", global = true)]
    pub prefer_offline: bool,

    /// Only pick versions published before this date.
    #[arg(long, global = true)]
    pub before: Option<String>,

    /// Only pick versions published at least this many days ago (0: off).
    #[arg(long = "min-release-age", global = true)]
    pub min_release_age: Option<f64>,

    /// Quiet: no progress notes or install summary.
    #[arg(short = 's', long = "silent", global = true)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install the project's dependencies (also `i`; `ci` is frozen).
    #[command(alias = "i")]
    Install,
    /// Install and record into package.json, then install.
    Add {
        #[arg(value_name = "SPEC")]
        specs: Vec<String>,
        /// Save to devDependencies.
        #[arg(short = 'D', long = "dev", conflicts_with = "optional")]
        dev: bool,
        /// Save to optionalDependencies.
        #[arg(short = 'O', long = "optional", conflicts_with = "dev")]
        optional: bool,
        /// Save the exact version instead of a caret range.
        #[arg(short = 'E', long = "exact")]
        exact: bool,
        /// Select workspaces (name or path; repeatable).
        #[arg(short = 'w', long = "workspace")]
        workspace: Vec<String>,
    },
    /// Remove from package.json, then install.
    #[command(alias = "uninstall", alias = "rm", alias = "r", alias = "un")]
    Remove {
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Select workspaces (name or path; repeatable).
        #[arg(short = 'w', long = "workspace")]
        workspace: Vec<String>,
    },
    /// Re-resolve preferring locked versions, then install.
    Dedupe,
    /// Show which registry version matches each spec.
    Resolve {
        #[arg(value_name = "SPEC")]
        specs: Vec<String>,
    },
    /// Cache packages without linking them.
    Fetch {
        #[arg(value_name = "SPEC")]
        specs: Vec<String>,
        /// Use flashnpm.lock instead of specs (current platform only).
        #[arg(long)]
        lock: bool,
    },
    /// Write flashnpm.lock without installing.
    Lock,
    /// Clean unreferenced store content.
    Prune,
    /// Frozen install (CI spelling).
    Ci,
    /// Run a package script (lists scripts when none is named).
    #[command(alias = "run-script")]
    Run {
        /// The script to run (empty lists scripts).
        #[arg(value_name = "SCRIPT")]
        script: Option<String>,
        /// Arguments passed to the script.
        #[arg(
            value_name = "ARGS",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
        /// Skip missing scripts instead of failing.
        #[arg(long = "if-present")]
        if_present: bool,
        /// Select workspaces (name or path; repeatable).
        #[arg(short = 'w', long = "workspace")]
        workspace: Vec<String>,
        /// Select all workspaces.
        #[arg(long)]
        workspaces: bool,
        /// With --workspaces, run the root script first.
        #[arg(long = "include-workspace-root")]
        include_root: bool,
    },
    /// Run a package bin, installing it when needed (also `flashnpx`).
    #[command(alias = "x")]
    Exec {
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
    },
}

impl Cli {
    pub fn project_options(&self) -> ProjectOptions {
        ProjectOptions {
            dir: self.dir.clone(),
            production: self.production,
            frozen: self.frozen || matches!(self.command, Some(Command::Ci)),
            verify: self.verify,
            offline: flag_opt(self.offline),
            prefer_offline: flag_opt(self.prefer_offline),
            registry: self.registry.clone(),
            store: self.store.clone(),
            min_release_age: self.min_release_age,
            before: self.before.clone(),
            workspace: Vec::new(),
        }
    }
}

fn flag_opt(v: bool) -> Option<bool> {
    if v {
        Some(true)
    } else {
        None
    }
}
