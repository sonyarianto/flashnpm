//! The commands as functions: what `flashnpm <command>` does, without argv
//! and without printing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{read_config, FlagOverrides};
use crate::error::{FlashnpmError, ErrorCode};
use crate::lock::{self, Lockfile, LOCKFILE};
use crate::package_json::{find_root, read_manifest, remove_dep, save_dep, save_range, Group};
use crate::registry::Registry;
use crate::resolve::{
    resolve_specs, resolve_tree, Resolution, ResolveOptions, ResolvedPackage, RootManifest,
};
use crate::spec::{parse_dep, parse_spec, SpecType};
use crate::state::{compute_state, is_current, write_state};
use crate::store::Store;

fn fail(code: ErrorCode, msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(code, msg)
}

pub type LogFn = dyn Fn(&str, LogLevel) + Send + Sync;

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Info,
    Warn,
    Debug,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectOptions {
    pub dir: Option<PathBuf>,
    pub production: bool,
    pub frozen: bool,
    pub verify: bool,
    pub offline: Option<bool>,
    pub prefer_offline: Option<bool>,
    pub registry: Option<String>,
    pub store: Option<PathBuf>,
    pub min_release_age: Option<f64>,
    pub before: Option<String>,
    /// `-w` workspace selections (add/remove/run).
    pub workspace: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub packages: usize,
    pub up_to_date: bool,
    pub lockfile: String,
}

#[derive(Debug, Clone)]
pub struct AddResult {
    pub added: Vec<Added>,
    pub install: InstallResult,
}

#[derive(Debug, Clone)]
pub struct Added {
    pub name: String,
    pub range: String,
    pub group: String,
}

fn resolve_dir(cwd: &Path, dir: &Option<PathBuf>) -> Result<PathBuf, FlashnpmError> {
    if let Some(d) = dir {
        return Ok(d.clone());
    }
    find_root(cwd).ok_or_else(|| {
        fail(
            ErrorCode::Emanifet,
            "no package.json found above current directory",
        )
    })
}

fn log_noop() -> Box<LogFn> {
    Box::new(|_, _| {})
}

struct Ctx {
    dir: PathBuf,
    store: Store,
    registry: Registry,
    resolve_opts: ResolveOptions,
    production: bool,
    verify: bool,
    log: Box<LogFn>,
    workspaces: Vec<crate::workspaces::Workspace>,
    /// The workspace the cwd belongs to, if any.
    workspace: Option<crate::workspaces::Workspace>,
}

async fn context(opts: &ProjectOptions, log: Box<LogFn>, cwd: &Path) -> Result<Ctx, FlashnpmError> {
    // `--dir` uses the directory as given; otherwise npm's walk up finds the
    // project (a workspace root when cwd sits in one of its workspaces).
    // Install/lock/dedupe/prune always work from the root.
    let (dir, workspaces, workspace) = match &opts.dir {
        Some(d) => {
            let (manifest, _) = read_manifest(d).await?;
            let spaces = if manifest.workspaces.is_some() {
                crate::workspaces::find_workspaces(d, &manifest)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (d.clone(), spaces, None)
        }
        None => match crate::workspaces::find_root(cwd).await {
            Ok(found) => (found.dir, found.workspaces, found.workspace),
            Err(_) => {
                let dir = resolve_dir(cwd, &None)?;
                (dir, Vec::new(), None)
            }
        },
    };
    let flags = FlagOverrides {
        registry: opts.registry.clone(),
        min_release_age: opts.min_release_age,
        before: opts.before.clone(),
        min_release_age_exclude: None,
        offline: opts.offline,
        prefer_offline: opts.prefer_offline,
    };
    let config = read_config(&dir, &flags)?;
    let production = opts.production;
    let store_dir = opts.store.clone().unwrap_or_else(Store::default_dir);
    let store = Store::new(store_dir.clone());
    tokio::fs::create_dir_all(&store_dir)
        .await
        .map_err(|e| fail(ErrorCode::Eio, format!("cannot create store: {e}")))?;
    let registry = Registry::new(config.clone(), &store_dir)?;
    Ok(Ctx {
        dir,
        store,
        registry,
        resolve_opts: ResolveOptions {
            production,
            before: config.before,
            release_age_exclude: config.release_age_exclude,
            keep: HashMap::new(),
        },
        production,
        log,
        verify: opts.verify,
        workspaces,
        workspace,
    })
}

/// Install the project's dependencies.
pub async fn install(opts: ProjectOptions, log: Box<LogFn>) -> Result<InstallResult, FlashnpmError> {
    install_in(
        &std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?,
        opts,
        log,
    )
    .await
}

/// Local `file:` tarballs pinned by the lock: re-hash each. Returns the
/// lock keys whose bytes moved. URL tarballs fail pinned (`EINTEGRITY`);
/// missing local files are left to the store error.
async fn check_tarball_pins(dir: &Path, lock: &Lockfile) -> Result<Vec<String>, FlashnpmError> {
    let mut moved = Vec::new();
    for (key, entry) in &lock.packages {
        if entry.version.is_none() {
            continue;
        }
        let at = key[1..].find('@').map(|i| i + 1).unwrap_or(0);
        if at == 0 {
            continue;
        }
        let source = key[at + 1..].to_string();
        if source.starts_with("file:") {
            let rel = &source["file:".len()..];
            let bytes = tokio::fs::read(dir.join(rel)).await.map_err(|_| {
                fail(
                    ErrorCode::Elock,
                    format!("{LOCKFILE} is stale: {source} is gone"),
                )
            })?;
            let hash = crate::integrity::hash_sha512(&bytes);
            if hash != entry.integrity {
                moved.push(key.clone());
            }
        } else {
            // URL pins are checked at fetch; probe cheaply via stored integrity
            // only when the store lacks the entry is handled downstream.
        }
    }
    moved.sort();
    Ok(moved)
}

pub async fn install_in(
    cwd: &Path,
    opts: ProjectOptions,
    log: Box<LogFn>,
) -> Result<InstallResult, FlashnpmError> {
    crate::profile::mark("install start");
    let mut ctx = context(&opts, log, cwd).await?;
    let (root, _) = read_manifest(&ctx.dir).await?;
    let existing = lock::read_lockfile(&ctx.dir).await?;

    // Reuse locked versions when the tree hasn't moved (stability, not freshness).
    if let Some(ref lock) = existing {
        if lock::same_tree(lock, &root, &ctx.workspaces) {
            // Local tarballs are the project's own files: re-hash; a replaced
            // file unlocks its key, a URL that changed fails pinned.
            let moved = check_tarball_pins(&ctx.dir, lock).await?;
            if moved.is_empty() {
                let resolution = lock::from_lockfile(lock, &ctx.registry.config.registry)?;
                return install_resolution(&mut ctx, &root, resolution, false).await;
            }
            if opts.frozen {
                return Err(fail(
                    ErrorCode::Elock,
                    format!(
                        "{LOCKFILE} is stale: a local tarball changed ({})",
                        moved.join(", ")
                    ),
                ));
            }
            // fall through to re-resolve, keeping everything but the moved keys
            for key in &moved {
                ctx.resolve_opts.keep.remove(key);
            }
            let resolution = resolve_tree(
                &ctx.registry,
                &ctx.store,
                &ctx.dir,
                root.clone(),
                &ctx.workspaces,
                &ctx.resolve_opts,
            )
            .await?;
            let lock = lock::to_lockfile(&resolution, &ctx.registry.config.registry);
            lock::write_lockfile(&ctx.dir, &lock).await?;
            return install_resolution(&mut ctx, &root, resolution, false).await;
        }
    }
    if opts.frozen {
        return Err(fail(
            ErrorCode::Elock,
            format!("{LOCKFILE} is missing or stale (frozen lockfile)"),
        ));
    }

    // Keep unrelated locks stable across re-resolves.
    if let Some(ref lock) = existing {
        // Names any top (root or workspace) still wants.
        let wanted = |name: &String| {
            root.dependencies.contains_key(name)
                || root.dev_dependencies.contains_key(name)
                || root.optional_dependencies.contains_key(name)
                || ctx.workspaces.iter().any(|w| {
                    w.manifest.dependencies.contains_key(name)
                        || w.manifest.dev_dependencies.contains_key(name)
                        || w.manifest.optional_dependencies.contains_key(name)
                })
        };
        for (key, entry) in &lock.packages {
            // tarball keys are `name@source`; registry keys `name@version`.
            let (name, pin) = match &entry.version {
                Some(_) => {
                    let at = key[1..].find('@').map(|i| i + 1).unwrap_or(0);
                    (key[..at].to_string(), key.clone())
                }
                None => match key.rsplit_once('@') {
                    Some((n, v)) => (n.to_string(), v.to_string()),
                    None => continue,
                },
            };
            if wanted(&name) {
                ctx.resolve_opts.keep.insert(name, pin);
            }
        }
    }

    let resolution = resolve_tree(
        &ctx.registry,
        &ctx.store,
        &ctx.dir,
        root.clone(),
        &ctx.workspaces,
        &ctx.resolve_opts,
    )
    .await?;
    let lock = lock::to_lockfile(&resolution, &ctx.registry.config.registry);
    lock::write_lockfile(&ctx.dir, &lock).await?;
    install_resolution(&mut ctx, &root, resolution, false).await
}

async fn install_resolution(
    ctx: &mut Ctx,
    root: &RootManifest,
    resolution: Resolution,
    _verify: bool,
) -> Result<InstallResult, FlashnpmError> {
    let lock = lock::to_lockfile(&resolution, &ctx.registry.config.registry);
    // `--verify` checks the tree instead of trusting recorded state.
    if !ctx.verify
        && is_current(&ctx.dir, &lock, root, &ctx.workspaces, ctx.production).await
        && crate::link::tree_standing(&ctx.dir, root, &resolution.packages, &ctx.workspaces).await
    {
        (ctx.log)("already up to date", LogLevel::Info);
        return Ok(InstallResult {
            packages: resolution.packages.len(),
            up_to_date: true,
            lockfile: LOCKFILE.to_string(),
        });
    }
    let refs: Vec<&ResolvedPackage> = resolution.packages.values().collect();
    let log_msg = |m: &str| (ctx.log)(m, LogLevel::Info);
    // Local tarballs are the project's own files: make sure the store holds
    // the locked bytes before fill (covers fresh stores + pruned indexes).
    for pkg in resolution.packages.values() {
        if let Some(source) = &pkg.source {
            if source.starts_with("file:") && !ctx.store.has(pkg).await {
                let bytes = crate::tarball::read_bytes(source, &ctx.dir, &ctx.registry).await?;
                if crate::integrity::hash_sha512(&bytes) != pkg.integrity {
                    return Err(crate::tarball::stale(
                        &crate::tarball::key_of(&pkg.name, source),
                        "bytes differ".to_string(),
                    ));
                }
                ctx.store.adopt_pkg(pkg, &bytes).await?;
            }
        }
    }
    crate::profile::mark("install fill start");
    ctx.store
        .fill(&ctx.registry, &refs, &|m| log_msg(m))
        .await?;
    crate::profile::mark("install fill done");
    let (linked, _) = crate::link::link_tree(
        &ctx.dir,
        root,
        &resolution.packages,
        &ctx.store,
        &ctx.workspaces,
    )
    .await?;
    crate::profile::mark("install link done");
    // write lockfile when installing from it too (normalizes field order)
    lock::write_lockfile(&ctx.dir, &lock).await?;
    write_state(
        &ctx.dir,
        &compute_state(&lock, root, &ctx.workspaces, ctx.production),
    )
    .await?;
    if ctx.verify {
        let repaired = crate::link::verify_tree(
            &ctx.dir,
            root,
            &resolution.packages,
            &ctx.store,
            &ctx.workspaces,
        )
        .await?;
        (ctx.log)(
            &format!("verified {} packages, repaired {repaired}", linked),
            LogLevel::Info,
        );
    } else {
        (ctx.log)(&format!("installed {linked} packages"), LogLevel::Info);
    }
    Ok(InstallResult {
        packages: linked,
        up_to_date: false,
        lockfile: LOCKFILE.to_string(),
    })
}

/// Write `flashnpm.lock` without installing.
pub async fn lock_cmd(opts: ProjectOptions) -> Result<Lockfile, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let ctx = context(&opts, log_noop(), &cwd).await?;
    let (root, _) = read_manifest(&ctx.dir).await?;
    let resolution = resolve_tree(
        &ctx.registry,
        &ctx.store,
        &ctx.dir,
        root,
        &ctx.workspaces,
        &ctx.resolve_opts,
    )
    .await?;
    let lock = lock::to_lockfile(&resolution, &ctx.registry.config.registry);
    lock::write_lockfile(&ctx.dir, &lock).await?;
    Ok(lock)
}

/// Resolve specs to versions without installing.
pub async fn resolve_cmd(
    specs: Vec<String>,
    opts: ProjectOptions,
) -> Result<Vec<ResolvedPackage>, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let dir = opts.dir.clone().unwrap_or(cwd);
    let opts = ProjectOptions {
        dir: Some(dir.clone()),
        ..opts
    };
    let ctx = context(&opts, log_noop(), &dir).await?;
    resolve_specs(&ctx.registry, &specs, &ctx.resolve_opts).await
}

/// Cache packages without linking them.
pub async fn fetch_cmd(specs: Vec<String>, opts: ProjectOptions) -> Result<usize, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let dir = opts.dir.clone().unwrap_or(cwd);
    let opts = ProjectOptions {
        dir: Some(dir.clone()),
        ..opts
    };
    let ctx = context(&opts, log_noop(), &dir).await?;
    let resolved = resolve_specs(&ctx.registry, &specs, &ctx.resolve_opts).await?;
    let refs: Vec<&ResolvedPackage> = resolved.iter().collect();
    ctx.store.fill(&ctx.registry, &refs, &|_| {}).await
}

/// Cache the current lockfile's packages without linking them.
pub async fn fetch_lockfile(opts: ProjectOptions, production: bool) -> Result<usize, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let dir = opts.dir.clone().unwrap_or(cwd);
    let opts = ProjectOptions {
        dir: Some(dir.clone()),
        ..opts
    };
    let ctx = context(&opts, log_noop(), &dir).await?;
    let lock = lock::read_lockfile(&ctx.dir)
        .await?
        .ok_or_else(|| fail(ErrorCode::Elock, format!("{LOCKFILE} is missing")))?;
    let mut resolution = lock::from_lockfile(&lock, &ctx.registry.config.registry)?;
    if production {
        resolution.packages.retain(|_, p| !p.dev);
    }
    // Local tarballs adopt at fetch time (their bytes are the project's own).
    for pkg in resolution.packages.values() {
        if let Some(source) = &pkg.source {
            if source.starts_with("file:") {
                let bytes = crate::tarball::read_bytes(source, &ctx.dir, &ctx.registry).await?;
                if crate::integrity::hash_sha512(&bytes) != pkg.integrity {
                    return Err(crate::tarball::stale(
                        &crate::tarball::key_of(&pkg.name, source),
                        "bytes differ".to_string(),
                    ));
                }
                ctx.store.adopt_pkg(pkg, &bytes).await?;
            }
        }
    }
    let refs: Vec<&ResolvedPackage> = resolution.packages.values().collect();
    ctx.store.fill(&ctx.registry, &refs, &|_| {}).await
}
/// Add dependencies to `package.json`, then install.
pub async fn add(
    specs: Vec<String>,
    group: Group,
    exact: bool,
    opts: ProjectOptions,
    log: Box<LogFn>,
) -> Result<AddResult, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    if specs.is_empty() {
        return Err(fail(ErrorCode::Eoption, "add needs at least one package"));
    }
    // resolve versions before editing package.json
    let ctx0 = context(&opts, log_noop(), &cwd).await?;
    let dir = target_manifest_dir(&ctx0, &cwd, &opts)?;
    // parse first so nothing is written when a spec is bad
    let mut parsed: Vec<crate::spec::Spec> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in &specs {
        // Bare tarball (`./lib.tgz`): name comes from its own package.json.
        if let Some(fetch) = crate::spec::bare_tarball(raw) {
            let rebased = crate::tarball::from_cwd(&dir, &cwd, &fetch);
            let name = crate::tarball::name_of(&fetch, &dir, &cwd, &ctx0.registry).await?;
            if !seen.insert(name.clone()) {
                return Err(fail(
                    ErrorCode::Einvalidspec,
                    format!("duplicate package {name}"),
                ));
            }
            let spec = parse_dep(&name, &rebased, None)?;
            parsed.push(spec);
            continue;
        }
        let mut spec = parse_spec(raw, None)?;
        if !seen.insert(spec.name.clone()) {
            return Err(fail(
                ErrorCode::Einvalidspec,
                format!("duplicate package {}", spec.name),
            ));
        }
        if matches!(spec.spec_type, SpecType::Tarball) {
            // Paths are the shell's (cwd); the manifest keeps them root-relative.
            spec.fetch_spec = crate::tarball::from_cwd(&dir, &cwd, &spec.fetch_spec);
            // validate it reads now (bad path / no package.json fails before writing)
            let source = crate::spec::tarball_source(&spec.fetch_spec, "");
            let bytes = crate::tarball::read_bytes(&source, &dir, &ctx0.registry).await?;
            crate::tarball::inner_manifest(&bytes, &source)?;
            parsed.push(spec);
            continue;
        }
        parsed.push(spec);
    }
    let mut added = Vec::new();
    for spec in &parsed {
        if matches!(spec.spec_type, SpecType::Workspace) {
            // `workspace:` ranges stay as typed; the version must satisfy.
            let ws = ctx0
                .workspaces
                .iter()
                .find(|w| w.name == spec.name)
                .ok_or_else(|| {
                    fail(
                        ErrorCode::Eworkspace,
                        format!("workspace {} not found", spec.name),
                    )
                })?;
            if !crate::semver::satisfies_str(&ws.version, &spec.fetch_spec, false) {
                return Err(fail(
                    ErrorCode::Eworkspace,
                    format!(
                        "workspace {} is {}, not {}",
                        spec.name, ws.version, spec.fetch_spec
                    ),
                ));
            }
            let range = format!("workspace:{}", spec.fetch_spec);
            save_dep(&dir, &spec.name, &range, group).await?;
            added.push(Added {
                name: spec.name.clone(),
                range,
                group: group.key().to_string(),
            });
            continue;
        }
        if matches!(spec.spec_type, SpecType::Tarball) {
            let range = spec.fetch_spec.clone();
            save_dep(&dir, &spec.name, &range, group).await?;
            added.push(Added {
                name: spec.name.clone(),
                range,
                group: group.key().to_string(),
            });
            continue;
        }
        // A workspace link never asks the registry: bare names save a caret
        // of the local version, explicit matching ranges save as typed.
        if !matches!(spec.spec_type, SpecType::Tag) {
            if let Some(ws) = ctx0.workspaces.iter().find(|w| w.name == spec.name) {
                if spec.raw == spec.name || spec.fetch_spec == "*" {
                    let range = if exact {
                        ws.version.clone()
                    } else {
                        format!("^{}", ws.version)
                    };
                    save_dep(&dir, &spec.name, &range, group).await?;
                    added.push(Added {
                        name: spec.name.clone(),
                        range,
                        group: group.key().to_string(),
                    });
                    continue;
                }
                if crate::semver::satisfies_str(&ws.version, &spec.fetch_spec, false) {
                    let range = spec.fetch_spec.clone();
                    save_dep(&dir, &spec.name, &range, group).await?;
                    added.push(Added {
                        name: spec.name.clone(),
                        range,
                        group: group.key().to_string(),
                    });
                    continue;
                }
            }
        }
        let version = if matches!(spec.spec_type, SpecType::Version) {
            spec.fetch_spec.clone()
        } else {
            let doc = ctx0
                .registry
                .packument(&spec.fetch_name, &spec.escaped_name, spec.scope.as_deref())
                .await?;
            let pick_opts = crate::pick::PickOptions {
                before: ctx0.resolve_opts.before,
                exclude_from_age: crate::pick::excluded(
                    &spec.fetch_name,
                    &ctx0.resolve_opts.release_age_exclude,
                ),
            };
            crate::pick::pick_manifest(&doc, spec, &pick_opts)?
        };
        let range = save_range(spec, &version, exact || ctx0.registry.config.save_exact);
        save_dep(&dir, &spec.name, &range, group).await?;
        added.push(Added {
            name: spec.name.clone(),
            range,
            group: group.key().to_string(),
        });
    }
    let install = install_in(&cwd, opts, log).await?;
    Ok(AddResult { added, install })
}

/// Remove dependencies from `package.json`, then install.
///
/// Edits the `-w` workspace, the workspace cwd is in, or the root.
pub async fn remove(
    names: Vec<String>,
    opts: ProjectOptions,
    log: Box<LogFn>,
) -> Result<InstallResult, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let ctx0 = context(&opts, log_noop(), &cwd).await?;
    let dir = target_manifest_dir(&ctx0, &cwd, &opts)?;
    for name in &names {
        // validate names early
        parse_dep(name, "npm:placeholder@*", None).map_err(|_| {
            fail(
                ErrorCode::Einvalidspec,
                format!("invalid package name {name:?}"),
            )
        })?;
        if !remove_dep(&dir, name).await? {
            return Err(fail(
                ErrorCode::Enodep,
                format!("{name} is not a dependency"),
            ));
        }
    }
    install_in(&cwd, opts, log).await
}

/// The manifest `add`/`remove` edits: the `-w` workspace (exactly one), the
/// workspace cwd is in, or the root.
fn target_manifest_dir(ctx: &Ctx, cwd: &Path, opts: &ProjectOptions) -> Result<PathBuf, FlashnpmError> {
    if opts.workspace.is_empty() {
        if let Some(ws) = &ctx.workspace {
            return Ok(ws.dir.clone());
        }
        return Ok(ctx.dir.clone());
    }
    let sels = crate::workspaces::select(&ctx.dir, cwd, &ctx.workspaces, &opts.workspace)?;
    if sels.len() != 1 {
        return Err(fail(
            ErrorCode::Eworkspace,
            format!(
                "add/remove takes exactly one workspace, -w matched {}",
                sels.len()
            ),
        ));
    }
    Ok(sels[0].dir.clone())
}

/// Re-resolve preferring locked versions, then install.
///
/// Unlike `install` (which reuses a current lockfile untouched), dedupe walks
/// the tree again: shared ranges collapse back onto one locked version where
/// allowed, and the lockfile is rewritten. Delete `flashnpm.lock` for fully fresh
/// versions instead.
pub async fn dedupe(opts: ProjectOptions, log: Box<LogFn>) -> Result<InstallResult, FlashnpmError> {
    let cwd = std::env::current_dir().map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let mut ctx = context(&opts, log, &cwd).await?;
    let (root, _) = read_manifest(&ctx.dir).await?;
    let existing = lock::read_lockfile(&ctx.dir).await?;
    // Seed `keep` from every locked registry entry the tree still wants, so
    // shared ranges collapse onto already-locked versions.
    if let Some(ref lock) = existing {
        let wanted = |name: &String| {
            root.dependencies.contains_key(name)
                || root.dev_dependencies.contains_key(name)
                || root.optional_dependencies.contains_key(name)
                || ctx.workspaces.iter().any(|w| {
                    w.manifest.dependencies.contains_key(name)
                        || w.manifest.dev_dependencies.contains_key(name)
                        || w.manifest.optional_dependencies.contains_key(name)
                })
        };
        for (key, entry) in &lock.packages {
            if entry.version.is_some() {
                continue; // tarballs pin by source, nothing to collapse
            }
            let Some((name, version)) = key.rsplit_once('@') else {
                continue;
            };
            if wanted(&name.to_string()) {
                // Lowest wins when several versions of one name are locked.
                let entry = ctx
                    .resolve_opts
                    .keep
                    .entry(name.to_string())
                    .or_insert_with(|| version.to_string());
                if crate::semver::parse(version)
                    .zip(crate::semver::parse(entry))
                    .map_or(false, |(a, b)| {
                        crate::semver::compare(&a, &b) == std::cmp::Ordering::Less
                    })
                {
                    *entry = version.to_string();
                }
            }
        }
    }
    if opts.frozen && existing.is_none() {
        return Err(fail(
            ErrorCode::Elock,
            format!("{LOCKFILE} is missing (frozen lockfile)"),
        ));
    }
    let resolution = resolve_tree(
        &ctx.registry,
        &ctx.store,
        &ctx.dir,
        root.clone(),
        &ctx.workspaces,
        &ctx.resolve_opts,
    )
    .await?;
    let lock = lock::to_lockfile(&resolution, &ctx.registry.config.registry);
    lock::write_lockfile(&ctx.dir, &lock).await?;
    install_resolution(&mut ctx, &root, resolution, false).await
}

#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub script: Option<String>,
    pub args: Vec<String>,
    pub if_present: bool,
    /// `-w` selections (names or paths).
    pub workspaces_sel: Vec<String>,
    /// `--workspaces`: every workspace.
    pub workspaces_all: bool,
    /// `--include-workspace-root`: with `--workspaces`, the root too, first.
    pub include_root: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRun {
    pub name: String,
    pub path: String,
    pub file: String,
    pub code: i32,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub code: i32,
    /// Set when listing (`run` without a script).
    pub scripts: Vec<(String, String)>,
    /// One per workspace run, in run order.
    pub results: Vec<WorkspaceRun>,
}

/// List `package.json` scripts.
pub async fn list_scripts(dir: &Path) -> Result<Vec<(String, String)>, FlashnpmError> {
    let (_, raw) = read_manifest(dir).await?;
    let scripts = crate::run::read_scripts(&raw, "package.json")?;
    Ok(scripts.into_iter().collect())
}

/// Run a package script (installing the tree first, a no-op when current).
///
/// Without workspace selection this uses the `package.json` in the current
/// directory (or `--dir`) directly — parent directories are not searched.
/// With `-w`/`--workspaces` it runs each selected workspace in dependency
/// order, continuing on failure and returning the first error code.
pub async fn run_in(
    cwd: &Path,
    opts: ProjectOptions,
    req: RunRequest,
    log: Box<LogFn>,
) -> Result<RunResult, FlashnpmError> {
    if !req.workspaces_sel.is_empty() || req.workspaces_all {
        return run_in_workspaces(cwd, opts, req, log).await;
    }
    let dir = opts.dir.clone().unwrap_or_else(|| cwd.to_path_buf());
    let (root, raw) = read_manifest(&dir).await?;
    let scripts = crate::run::read_scripts(&raw, "package.json")?;
    let Some(script) = req.script.clone() else {
        return Ok(RunResult {
            code: 0,
            scripts: scripts.into_iter().collect(),
            results: Vec::new(),
        });
    };
    let Some(command) = scripts.get(&script) else {
        if req.if_present {
            log(&format!("skipping missing script {script}"), LogLevel::Info);
            return Ok(RunResult {
                code: 0,
                scripts: Vec::new(),
                results: Vec::new(),
            });
        }
        return Err(fail(
            ErrorCode::Eoption,
            format!("missing script: {script}"),
        ));
    };
    // Install first so dependencies are current (quick check when nothing changed).
    let install_opts = ProjectOptions {
        dir: Some(dir.clone()),
        ..opts
    };
    install_in(&dir, install_opts, log).await?;
    let file = dir.join("package.json");
    let env = crate::run::script_env(
        &dir,
        &file,
        &script,
        command,
        root.name.as_deref(),
        root.version.as_deref(),
    );
    let line = crate::run::shell_line(command, &req.args);
    let code = crate::run::run_shell(&line, &dir, &env)
        .await
        .map_err(|e| fail(ErrorCode::Eio, format!("cannot run {script}: {e}")))?;
    Ok(RunResult {
        code,
        scripts: Vec::new(),
        results: Vec::new(),
    })
}

/// Run a script across workspaces: deps first, failures don't stop others.
async fn run_in_workspaces(
    cwd: &Path,
    opts: ProjectOptions,
    req: RunRequest,
    log: Box<LogFn>,
) -> Result<RunResult, FlashnpmError> {
    let ctx = context(&opts, log_noop(), cwd).await?;
    let root = ctx.dir.clone();
    let selected: Vec<crate::workspaces::Workspace> = if req.workspaces_all {
        ctx.workspaces.clone()
    } else {
        crate::workspaces::select(&root, cwd, &ctx.workspaces, &req.workspaces_sel)?
            .into_iter()
            .cloned()
            .collect()
    };
    let ordered = order_workspaces(&selected);
    let Some(script) = req.script.clone() else {
        // `run -w` without a script lists nothing; keep the error clear.
        return Err(fail(ErrorCode::Eoption, "run -w needs a script"));
    };
    // Install the whole tree first (a no-op when current).
    let install_opts = ProjectOptions {
        dir: Some(root.clone()),
        ..opts
    };
    install_in(&root, install_opts, log).await?;

    let mut results = Vec::new();
    let mut first_code = 0;
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    if req.workspaces_all && req.include_root {
        let (root_manifest, _) = read_manifest(&root).await?;
        targets.push((root_manifest.name.clone().unwrap_or_default(), root.clone()));
    }
    for ws in &ordered {
        targets.push((ws.name.clone(), ws.dir.clone()));
    }
    for (name, dir) in targets {
        let (_, raw) = read_manifest(&dir).await?;
        let scripts = crate::run::read_scripts(&raw, "package.json")?;
        let Some(command) = scripts.get(&script) else {
            if req.if_present {
                continue;
            }
            results.push(WorkspaceRun {
                name: name.clone(),
                path: dir.to_string_lossy().to_string(),
                file: dir.join("package.json").to_string_lossy().to_string(),
                code: 1,
            });
            if first_code == 0 {
                first_code = 1;
            }
            continue;
        };
        let (manifest, _) = read_manifest(&dir).await?;
        let file = dir.join("package.json");
        let env = crate::run::script_env(
            &dir,
            &file,
            &script,
            command,
            manifest.name.as_deref(),
            manifest.version.as_deref(),
        );
        let line = crate::run::shell_line(command, &req.args);
        let code = crate::run::run_shell(&line, &dir, &env)
            .await
            .map_err(|e| fail(ErrorCode::Eio, format!("cannot run {script}: {e}")))?;
        results.push(WorkspaceRun {
            name,
            path: dir.to_string_lossy().to_string(),
            file: file.to_string_lossy().to_string(),
            code,
        });
        if code != 0 && first_code == 0 {
            first_code = code;
        }
    }
    Ok(RunResult {
        code: first_code,
        scripts: Vec::new(),
        results,
    })
}

/// Order workspaces dependencies-first (post-order DFS); cycles fall back to
/// declaration order for the looping part.
fn order_workspaces(
    selected: &[crate::workspaces::Workspace],
) -> Vec<crate::workspaces::Workspace> {
    let by_name: HashMap<&str, &crate::workspaces::Workspace> =
        selected.iter().map(|w| (w.name.as_str(), w)).collect();
    let mut ordered: Vec<crate::workspaces::Workspace> = Vec::new();
    let mut done: HashMap<String, bool> = HashMap::new();
    fn visit(
        ws: &crate::workspaces::Workspace,
        by_name: &HashMap<&str, &crate::workspaces::Workspace>,
        done: &mut HashMap<String, bool>,
        ordered: &mut Vec<crate::workspaces::Workspace>,
    ) {
        if done.contains_key(&ws.path) {
            return;
        }
        done.insert(ws.path.clone(), false);
        let mut deps: Vec<&str> = Vec::new();
        for dep in ws
            .manifest
            .dependencies
            .keys()
            .chain(ws.manifest.dev_dependencies.keys())
        {
            if by_name.contains_key(dep.as_str()) {
                deps.push(dep);
            }
        }
        deps.sort();
        for dep in deps {
            if let Some(next) = by_name.get(dep) {
                if !done.contains_key(&next.path) {
                    visit(next, by_name, done, ordered);
                }
            }
        }
        done.insert(ws.path.clone(), true);
        ordered.push(ws.clone());
    }
    for ws in selected {
        visit(ws, &by_name, &mut done, &mut ordered);
    }
    ordered
}

#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub packages: Vec<String>,
    pub call: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub code: i32,
}

/// Run a package bin, installing it into the exec project when needed.
pub async fn exec_in(
    cwd: &Path,
    opts: ProjectOptions,
    req: ExecRequest,
    log: Box<LogFn>,
) -> Result<ExecResult, FlashnpmError> {
    let dir = resolve_dir(cwd, &opts.dir)?;
    let local_env = {
        let base: Vec<(String, String)> = std::env::vars().collect();
        crate::run::with_path(&crate::run::bin_dirs(&dir), base, |v| v)
    };

    // `-c '<line>'`: a shell line with `-p` packages on PATH.
    if let Some(line) = req.call {
        let env = if req.packages.is_empty() {
            local_env
        } else {
            let home = exec_install(&dir, &opts, &req.packages, &log).await?;
            combined_env(&dir, &home)
        };
        let mut full = line;
        for arg in &req.args {
            full.push(' ');
            full.push_str(&crate::run::quote_sh(arg));
        }
        let code = crate::run::run_shell(&full, &dir, &env)
            .await
            .map_err(|e| fail(ErrorCode::Eio, format!("cannot run shell line: {e}")))?;
        return Ok(ExecResult { code });
    }

    let Some(command) = req.command.clone() else {
        return Err(fail(
            ErrorCode::Eoption,
            "exec needs a command or -c '<line>'",
        ));
    };
    // The project's own bin, then installed bins — as with npm.
    if let Some(argv) = crate::exec::self_bin(&dir, &command).await {
        return run_argv(&dir, &local_env, argv, &req.args).await;
    }
    if let Some(argv) = crate::exec::local_bin(&dir, &command).await {
        return run_argv(&dir, &local_env, argv, &req.args).await;
    }
    // Install into the exec project: explicit `-p` specs, else the command itself.
    let specs = if req.packages.is_empty() {
        vec![command.clone()]
    } else {
        req.packages.clone()
    };
    for raw in &specs {
        let spec = parse_spec(raw, None)?;
        if matches!(spec.spec_type, SpecType::Tarball | SpecType::Workspace) {
            return Err(fail(
                ErrorCode::Eoption,
                format!("exec takes registry specs only: {raw}"),
            ));
        }
    }
    let home = exec_install(&dir, &opts, &specs, &log).await?;
    let env = combined_env(&dir, &home);
    let bin_name = if req.packages.len() == 1 || specs.len() == 1 && req.packages.is_empty() {
        // Single package: npm's pick (only bin, same file, or package-named).
        let spec = parse_spec(&specs[0], None)?;
        let manifest = read_installed_manifest(&home, &spec.name).await?;
        let bins = manifest_bins(&manifest, &spec.name);
        crate::exec::pick_bin(&bins, &spec.name)?
    } else {
        command.clone()
    };
    let argv = find_exec_bin(&home, &bin_name).await.ok_or_else(|| {
        fail(
            ErrorCode::Enobin,
            format!("no bin {bin_name} in the installed packages (use -p)"),
        )
    })?;
    run_argv(&dir, &env, argv, &req.args).await
}

async fn run_argv(
    dir: &Path,
    env: &[(String, String)],
    argv: Vec<String>,
    args: &[String],
) -> Result<ExecResult, FlashnpmError> {
    let (prog, rest) = argv
        .split_first()
        .ok_or_else(|| fail(ErrorCode::Enobin, "empty bin"))?;
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(rest).args(args).current_dir(dir);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    let mut child = cmd
        .spawn()
        .map_err(|e| fail(ErrorCode::Eio, format!("cannot run {prog}: {e}")))?;
    let status = child
        .wait()
        .await
        .map_err(|e| fail(ErrorCode::Eio, format!("command failed: {e}")))?;
    Ok(ExecResult {
        code: status.code().unwrap_or(128),
    })
}

/// Install `specs` into the exec project for `root`; returns the project dir.
async fn exec_install(
    root: &Path,
    opts: &ProjectOptions,
    specs: &[String],
    log: &Box<LogFn>,
) -> Result<PathBuf, FlashnpmError> {
    let home_base = crate::exec::exec_home(root);
    // One project per set of specs, so each set installs once.
    let mut sorted = specs.to_vec();
    sorted.sort();
    let key = crate::integrity::hash_sha512(sorted.join("\n").as_bytes());
    let home = home_base.join(format!("proj-{:.16}", &key[7..]));
    tokio::fs::create_dir_all(&home)
        .await
        .map_err(|e| fail(ErrorCode::Eio, format!("cannot create exec project: {e}")))?;
    // Write deps (registry specs only; tags/ranges resolve at install).
    let mut deps = serde_json::Map::new();
    for raw in &sorted {
        let spec = parse_spec(raw, None)?;
        deps.insert(
            spec.name.clone(),
            serde_json::Value::String(spec.fetch_spec.clone()),
        );
    }
    let manifest = serde_json::json!({
        "name": "flashnpm-exec",
        "private": true,
        "dependencies": deps,
    });
    let file = home.join("package.json");
    let text =
        serde_json::to_string_pretty(&manifest).map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    tokio::fs::write(&file, format!("{text}\n"))
        .await
        .map_err(|e| fail(ErrorCode::Eio, e.to_string()))?;
    let install_opts = ProjectOptions {
        dir: Some(home.clone()),
        production: false,
        frozen: false,
        verify: false,
        offline: opts.offline,
        prefer_offline: opts.prefer_offline,
        registry: opts.registry.clone(),
        store: opts.store.clone(),
        min_release_age: opts.min_release_age,
        before: opts.before.clone(),
        workspace: Vec::new(),
    };
    let noop: Box<LogFn> = Box::new(|_, _| {});
    let _ = log;
    install_in(&home, install_opts, noop).await?;
    Ok(home)
}

fn combined_env(dir: &Path, exec_home: &Path) -> Vec<(String, String)> {
    let base: Vec<(String, String)> = std::env::vars().collect();
    let mut dirs = crate::run::bin_dirs(exec_home);
    dirs.extend(crate::run::bin_dirs(dir));
    crate::run::with_path(&dirs, base, |v| v)
}

async fn read_installed_manifest(home: &Path, name: &str) -> Result<serde_json::Value, FlashnpmError> {
    let file = home.join("node_modules").join(name).join("package.json");
    let text = tokio::fs::read_to_string(&file).await.map_err(|e| {
        fail(
            ErrorCode::Enobin,
            format!("cannot read installed {name}: {e}"),
        )
    })?;
    serde_json::from_str(&text).map_err(|e| fail(ErrorCode::Enobin, e.to_string()))
}

fn manifest_bins(
    manifest: &serde_json::Value,
    name: &str,
) -> std::collections::BTreeMap<String, String> {
    crate::resolve::normalize_bin(
        manifest
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(name),
        manifest.get("bin").unwrap_or(&serde_json::Value::Null),
    )
}

async fn find_exec_bin(home: &Path, bin_name: &str) -> Option<Vec<String>> {
    #[cfg(unix)]
    let path = home.join("node_modules/.bin").join(bin_name);
    #[cfg(windows)]
    let path = home
        .join("node_modules/.bin")
        .join(format!("{bin_name}.cmd"));
    if path.is_file() {
        Some(vec![path.to_string_lossy().to_string()])
    } else {
        None
    }
}

/// Remove unreferenced store content (MVP: sweep `files/` blobs older than 1h
/// that no valid index references).
pub async fn prune(store_dir: Option<PathBuf>) -> Result<PruneResult, FlashnpmError> {
    let dir = store_dir.unwrap_or_else(Store::default_dir);
    let mut removed_files: u64 = 0;
    let mut removed_bytes: u64 = 0;
    // collect referenced blob names from indexes
    let mut referenced = std::collections::HashSet::new();
    let index_dir = dir.join("index");
    if let Ok(walk) = std::fs::read_dir(&index_dir) {
        for entry in walk.flatten() {
            let pkg_dir = entry.path();
            if !pkg_dir.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&pkg_dir) {
                for f in files.flatten() {
                    if let Ok(bytes) = std::fs::read(f.path()) {
                        if let Ok(idx) = serde_json::from_slice::<crate::store::IndexEntry>(&bytes)
                        {
                            for meta in idx.files.values() {
                                referenced.insert(meta.integrity.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    let files_dir = dir.join("files");
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut stack = vec![files_dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            if mtime.map_or(false, |t| t > cutoff) {
                continue;
            }
            // blob filename is base64url sha512; we cannot map back to integrity
            // cheaply — MVP only removes `*.tmp`-style leftovers.
            if p.extension().map_or(false, |e| e == "tmp") {
                if let Ok(m) = std::fs::metadata(&p) {
                    removed_bytes += m.len();
                }
                let _ = std::fs::remove_file(&p);
                removed_files += 1;
            }
        }
    }
    Ok(PruneResult {
        removed_files,
        removed_bytes,
    })
}

#[derive(Debug, Clone)]
pub struct PruneResult {
    pub removed_files: u64,
    pub removed_bytes: u64,
}
