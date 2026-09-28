//! `flashnpm` binary: argv in, one `api` command run, formatted output out.

use flashnpm::api::{self, LogLevel};
use flashnpm::cli::{Cli, Command};
use flashnpm::package_json::Group;
use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = run(cli).await;
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    let quiet = cli.quiet;
    let json = cli.json;
    let log: Box<flashnpm::api::LogFn> = Box::new(move |msg, level| {
        if quiet && matches!(level, LogLevel::Info) {
            return;
        }
        eprintln!("[flashnpm] {msg}");
    });

    let opts = cli.project_options();
    // `flashnpm <script> [args...]` implies `run` (t/tst are run test).
    let mut cli = cli;
    if cli.command.is_none() {
        if let Some((script, rest)) = cli.external.split_first() {
            let script = match script.as_str() {
                "t" | "tst" => "test".to_string(),
                s => s.to_string(),
            };
            cli.command = Some(Command::Run {
                script: Some(script),
                args: rest.to_vec(),
                if_present: false,
                workspace: Vec::new(),
                workspaces: false,
                include_root: false,
            });
            cli.external = Vec::new();
        }
    }
    let result: Result<i32, flashnpm::FlashnpmError> = async {
        match &cli.command {
            None | Some(Command::Install) => {
                let r = api::install(opts, log).await?;
                if json {
                    println!("{}", serde_json::json!({"packages": r.packages, "upToDate": r.up_to_date}));
                } else if !quiet {
                    println!(
                        "{}",
                        if r.up_to_date {
                            "already up to date".to_string()
                        } else {
                            format!("installed {} packages", r.packages)
                        }
                    );
                }
                Ok(0)
            }
            Some(Command::Ci) => {
                let mut opts = opts;
                opts.frozen = true;
                let r = api::install(opts, log).await?;
                if json {
                    println!("{}", serde_json::json!({"packages": r.packages, "upToDate": r.up_to_date}));
                } else if !quiet {
                    println!("installed {} packages", r.packages);
                }
                Ok(0)
            }
            Some(Command::Add { specs, dev, optional, exact, workspace }) => {
                let group = if *dev {
                    Group::Dev
                } else if *optional {
                    Group::Optional
                } else {
                    Group::Dependencies
                };
                let mut opts = opts;
                opts.workspace = workspace.clone();
                // `flashnpm install <spec>` is `add`
                let r = api::add(specs.clone(), group, *exact, opts, log).await?;
                if json {
                    println!("{}", serde_json::json!({"added": r.added.iter().map(|a| &a.name).collect::<Vec<_>>()}));
                } else if !quiet {
                    for a in &r.added {
                        println!("added {}@{} to {}", a.name, a.range, a.group);
                    }
                }
                Ok(0)
            }
            Some(Command::Remove { names, workspace }) => {
                let mut opts = opts;
                opts.workspace = workspace.clone();
                let r = api::remove(names.clone(), opts, log).await?;
                if !quiet {
                    println!("removed; {} packages now", r.packages);
                }
                Ok(0)
            }
            Some(Command::Dedupe) => {
                let r = api::dedupe(opts, log).await?;
                if !quiet {
                    println!("deduped; {} packages", r.packages);
                }
                Ok(0)
            }
            Some(Command::Resolve { specs }) => {
                let out = api::resolve_cmd(specs.clone(), opts).await?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &out.iter()
                                .map(|p| serde_json::json!({
                                    "name": p.name, "version": p.version,
                                    "resolved": p.resolved, "integrity": p.integrity,
                                }))
                                .collect::<Vec<_>>()
                        )
                        .unwrap()
                    );
                } else {
                    for p in out {
                        println!("{}@{} {}", p.name, p.version, p.resolved);
                    }
                }
                Ok(0)
            }
            Some(Command::Fetch { specs, lock }) => {
                if *lock {
                    let n = api::fetch_lockfile(opts.clone(), opts.production).await?;
                    if !quiet {
                        println!("fetched {n} packages");
                    }
                } else {
                    let n = api::fetch_cmd(specs.clone(), opts).await?;
                    if !quiet {
                        println!("fetched {n} packages");
                    }
                }
                Ok(0)
            }
            Some(Command::Lock) => {
                let lock = api::lock_cmd(opts).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&lock).unwrap());
                } else if !quiet {
                    println!("wrote {} ({} packages)", flashnpm::lock::LOCKFILE, lock.packages.len());
                }
                Ok(0)
            }
            Some(Command::Prune) => {
                let r = api::prune(opts.store).await?;
                if !quiet {
                    println!("pruned {} files ({} bytes)", r.removed_files, r.removed_bytes);
                }
                Ok(0)
            }
            Some(Command::Run { script, args, if_present, workspace, workspaces, include_root }) => {
                let req = api::RunRequest {
                    script: script.clone(),
                    args: args.clone(),
                    if_present: *if_present,
                    workspaces_sel: workspace.clone(),
                    workspaces_all: *workspaces,
                    include_root: *include_root,
                };
                if script.is_some() && !quiet {
                    eprintln!("> {} {}", script.as_deref().unwrap_or(""), args.join(" "));
                }
                let r = api::run_in(
                    &std::env::current_dir().map_err(|e| {
                        flashnpm::FlashnpmError::new(flashnpm::ErrorCode::Eio, e.to_string())
                    })?,
                    opts,
                    req,
                    log,
                )
                .await?;
                if script.is_none() {
                    if json {                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &r.scripts.iter().map(|(n, c)| serde_json::json!({"name": n, "command": c})).collect::<Vec<_>>()
                            )
                            .unwrap()
                        );
                    } else if r.scripts.is_empty() {
                        eprintln!("no scripts");
                    } else {
                        for (name, command) in &r.scripts {
                            println!("{name}\n  {command}");
                        }
                    }
                    return Ok(0);
                }
                if !r.results.is_empty() && !quiet {
                    for res in &r.results {
                        eprintln!("[{}] exited {}", res.name, res.code);
                    }
                }
                Ok(r.code)
            }
            Some(Command::Exec { command, args, package, call, yes: _ }) => {
                let req = api::ExecRequest {
                    command: command.clone(),
                    args: args.clone(),
                    packages: package.clone(),
                    call: call.clone(),
                };
                let r = api::exec_in(
                    &std::env::current_dir().map_err(|e| {
                        flashnpm::FlashnpmError::new(flashnpm::ErrorCode::Eio, e.to_string())
                    })?,
                    opts,
                    req,
                    log,
                )
                .await?;
                Ok(r.code)
            }
        }
    }
    .await;

    match result {
        Ok(code) => code,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": e.message, "code": e.code.as_str()})
                );
            } else {
                eprintln!("flashnpm: [{}] {}", e.code, e.message);
            }
            1
        }
    }
}
