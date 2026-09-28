//! Walk a root `package.json` into a flat set of `name@version` packages.
//! Port of `upm`'s `src/resolve.ts` (MVP: no workspaces, no peers folding beyond
//! required-peer install, no hoisting — same as upm).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use futures::stream::StreamExt as _;

use crate::error::{FlashnpmError, ErrorCode};
use crate::pick::{excluded, pick_manifest, PickOptions};
use crate::registry::Registry;
use crate::spec::{parse_dep, parse_spec, SpecType};
use crate::types::PackumentVersion;

fn fail(code: ErrorCode, msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(code, msg)
}

pub type RootSpecs = HashMap<String, HashMap<String, String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub resolved: String,
    pub integrity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A workspace, at this root-relative `/` path. Linked from its directory,
    /// never in `.flashnpm`; key is `name@link:<path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub dev: bool,
    #[serde(default)]
    pub bin: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_dependencies: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct RootManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: HashMap<String, String>,
    pub dev_dependencies: HashMap<String, String>,
    pub optional_dependencies: HashMap<String, String>,
    /// Declared workspace patterns (root only).
    pub workspaces: Option<Vec<String>>,
    /// Normalized bins (string form keyed by package short name).
    pub bin: BTreeMap<String, String>,
    pub peer_dependencies: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    pub production: bool,
    pub before: Option<i64>,
    pub release_age_exclude: Vec<String>,
    /// Keep these `name@version` keys as-is when the range still allows them.
    pub keep: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub root: RootManifest,
    pub packages: HashMap<String, ResolvedPackage>,
    pub warnings: Vec<String>,
    /// The root's workspaces (pass-through for lock/link).
    pub workspaces: Vec<crate::workspaces::Workspace>,
}

pub fn key_of(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

pub fn workspace_key(name: &str, path: &str) -> String {
    format!("{name}@link:{path}")
}

/// Directories with a `package.json` manifest; MVP reads only the root.
pub async fn resolve_tree(
    registry: &Registry,
    store: &crate::store::Store,
    root_dir: &Path,
    root: RootManifest,
    workspaces: &[crate::workspaces::Workspace],
    opts: &ResolveOptions,
) -> Result<Resolution, FlashnpmError> {
    crate::profile::mark("resolve start");
    let by_name: HashMap<&str, &crate::workspaces::Workspace> = {
        let mut m = HashMap::new();
        for ws in workspaces {
            m.insert(ws.name.as_str(), ws);
        }
        m
    };
    let mut packages: HashMap<String, ResolvedPackage> = HashMap::new();
    let mut warnings = Vec::new();
    // queue of (name, range, via_optional, from_top)
    let mut queue: Vec<(String, String, bool, bool)> = Vec::new();
    // A top is the root plus every workspace: their deps walk in full.
    let tops: Vec<&RootManifest> = std::iter::once(&root)
        .chain(workspaces.iter().map(|w| &w.manifest))
        .collect();
    for top in &tops {
        for (name, range) in top
            .dependencies
            .iter()
            .chain(top.optional_dependencies.iter())
        {
            queue.push((name.clone(), range.clone(), false, true));
        }
        if !opts.production {
            for (name, range) in &top.dev_dependencies {
                queue.push((name.clone(), range.clone(), false, true));
            }
        }
    }
    // dev only matters for the root's own tree in the flat linker; keep the set.
    let dev_reachable: HashSet<String> = root.dev_dependencies.keys().cloned().collect();

    // Workspace tops link themselves: insert their identities first.
    for ws in workspaces {
        let key = workspace_key(&ws.name, &ws.path);
        packages.insert(
            key,
            ResolvedPackage {
                name: ws.name.clone(),
                version: ws.version.clone(),
                resolved: String::new(),
                integrity: String::new(),
                source: None,
                local: Some(ws.path.clone()),
                dependencies: edge_map(
                    &ws.manifest.dependencies,
                    &by_name,
                    true,
                    &mut warnings,
                    &ws.name,
                ),
                optional_dependencies: {
                    let m = edge_map(
                        &ws.manifest.optional_dependencies,
                        &by_name,
                        true,
                        &mut warnings,
                        &ws.name,
                    );
                    if m.is_empty() {
                        None
                    } else {
                        Some(m)
                    }
                },
                optional: false,
                dev: false,
                bin: ws.manifest.bin.clone(),
                os: None,
                cpu: None,
                libc: None,
                peer_dependencies: if ws.manifest.peer_dependencies.is_empty() {
                    None
                } else {
                    Some(
                        ws.manifest
                            .peer_dependencies
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    )
                },
            },
        );
    }
    // Queue workspace dep edges (registry ones; workspace ones are already linked).
    for ws in workspaces {
        for (dep, range) in ws
            .manifest
            .dependencies
            .iter()
            .chain(ws.manifest.optional_dependencies.iter())
            .chain(
                if opts.production {
                    None
                } else {
                    Some(&ws.manifest.dev_dependencies)
                }
                .into_iter()
                .flatten(),
            )
        {
            // Workspace-to-workspace edges stay inside the tops; registry edges resolve below.
            if workspace_edge(dep, range, &by_name).is_none() {
                queue.push((dep.clone(), range.clone(), false, true));
            }
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    // Round-based BFS: each frontier's packuments fetch concurrently
    // (bounded), then edges process in pop order — the same picks as the
    // sequential walk, since picking is pure per spec+document.
    while !queue.is_empty() {
        let frontier: Vec<(String, String, bool, bool)> =
            std::mem::take(&mut queue).into_iter().rev().collect();
        let parsed: Vec<Result<crate::spec::Spec, FlashnpmError>> = frontier
            .iter()
            .map(|(name, range, _, _)| parse_dep(name, range, None))
            .collect();
        // Plan packument fetches: unique (registry, name) keys this round.
        let mut planned: HashSet<String> = HashSet::new();
        let mut tasks = Vec::new();
        for (i, (name, range, _, from_top)) in frontier.iter().enumerate() {
            let Ok(spec) = &parsed[i] else { continue };
            // Terminal without registry IO.
            if *from_top && workspace_edge(name, range, &by_name).is_some() {
                continue;
            }
            if matches!(spec.spec_type, SpecType::Tarball) {
                continue;
            }
            if matches!(spec.spec_type, SpecType::Workspace) {
                continue; // errors at processing, in order
            }
            let reg = registry.registry_for(spec.scope.as_deref());
            let key = format!("{reg}|{}", spec.escaped_name);
            if planned.insert(key.clone()) {
                let (fetch_name, escaped, scope) = (
                    spec.fetch_name.clone(),
                    spec.escaped_name.clone(),
                    spec.scope.clone(),
                );
                let reg_ref = registry.clone();
                tasks.push(async move {
                    let doc = reg_ref
                        .packument(&fetch_name, &escaped, scope.as_deref())
                        .await;
                    (key, doc)
                });
            }
        }
        // Bounded concurrency; results map consumed below in pop order.
        let fetched: Vec<(String, Result<crate::types::Packument, FlashnpmError>)> =
            futures::stream::iter(tasks)
                .buffer_unordered(16)
                .collect()
                .await;
        let mut docs: HashMap<String, Result<crate::types::Packument, FlashnpmError>> =
            HashMap::with_capacity(fetched.len());
        for (key, doc) in fetched {
            docs.insert(key, doc);
        }
        for (i, (name, range, via_optional, from_top)) in frontier.into_iter().enumerate() {
            let spec = parsed[i]
                .as_ref()
                .map_err(|e| FlashnpmError::new(e.code, e.message.clone()))?;
            // A workspace edge from a top never touches the registry.
            if from_top {
                if let Some(ws) = workspace_edge(&name, &range, &by_name) {
                    let key = workspace_key(&ws.name, &ws.path);
                    if seen.insert(key) {
                        // already inserted above; nothing to fetch
                    }
                    continue;
                }
                if matches!(spec.spec_type, SpecType::Workspace) {
                    return Err(fail(
                        ErrorCode::Eworkspace,
                        format!("workspace {name} not found (at {range})"),
                    ));
                }
            } else if matches!(spec.spec_type, SpecType::Workspace | SpecType::Tarball)
                && !(spec.spec_type == SpecType::Tarball && is_url_spec(&spec))
            {
                // Registry manifests never link workspaces or read local tarballs.
                if matches!(spec.spec_type, SpecType::Workspace) {
                    return Err(fail(
                        ErrorCode::Eworkspace,
                        format!("only the root and workspaces can link to a workspace: {name}"),
                    ));
                }
                return Err(fail(
                    ErrorCode::Eoption,
                    format!("only the root and workspaces can use a tarball path: {name}"),
                ));
            }
            if matches!(spec.spec_type, SpecType::Workspace) {
                // Non-top workspace specs are refused above; a top one that names
                // nothing is an error (the found ones returned early).
                return Err(fail(
                    ErrorCode::Eworkspace,
                    format!("workspace {name} not found (at {range})"),
                ));
            }
            if matches!(spec.spec_type, SpecType::Tarball) {
                let source = crate::spec::tarball_source(&spec.fetch_spec, "");
                let key = crate::tarball::key_of(&spec.name, &source);
                if !seen.insert(key.clone()) {
                    continue;
                }
                // Pinned bytes win (lock reuse passes them via `keep` as name@source);
                // otherwise read the source now.
                let bytes = crate::tarball::read_bytes(&source, root_dir, registry).await?;
                let integrity = crate::integrity::hash_sha512(&bytes);
                if let Some(kept) = opts.keep.get(&spec.name) {
                    // `keep` holds `version` for registry keys and full keys for tarballs.
                    if kept != &key && kept != &integrity {
                        // stale pin ignored: fresh bytes decide below
                    }
                }
                let inner = crate::tarball::inner_manifest(&bytes, &source)?;
                // Adopt into the store under the tarball index path now, so fill()
                // never needs the project dir later.
                let pkg = ResolvedPackage {
                    name: spec.name.clone(),
                    version: inner.version.clone(),
                    resolved: source.clone(),
                    integrity: integrity.clone(),
                    source: Some(source.clone()),
                    local: None,
                    dependencies: inner
                        .dependencies
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    optional_dependencies: if inner.optional_dependencies.is_empty() {
                        None
                    } else {
                        Some(
                            inner
                                .optional_dependencies
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        )
                    },
                    optional: via_optional,
                    dev: dev_reachable.contains(&spec.name),
                    bin: inner.bin.clone(),
                    os: inner.os.clone(),
                    cpu: inner.cpu.clone(),
                    libc: inner.libc.clone(),
                    peer_dependencies: if inner.peer_dependencies.is_empty() {
                        None
                    } else {
                        Some(
                            inner
                                .peer_dependencies
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        )
                    },
                };
                store.adopt_pkg(&pkg, &bytes).await?;
                for (dep, r) in &pkg.dependencies {
                    queue.push((dep.clone(), r.clone(), via_optional, false));
                }
                if let Some(opt) = &pkg.optional_dependencies {
                    for (dep, r) in opt {
                        queue.push((dep.clone(), r.clone(), true, false));
                    }
                }
                packages.insert(key, pkg);
                continue;
            }
            // Stability: reuse locked version when it still satisfies the range.
            if let Some(kept) = opts.keep.get(&name) {
                if crate::semver::satisfies_str(kept, &spec.fetch_spec, false) {
                    if packages.contains_key(&key_of(&name, kept)) {
                        continue;
                    }
                    // resolve the kept version's own deps from the registry below
                    let reg = registry.registry_for(spec.scope.as_deref());
                    let doc_key = format!("{reg}|{}", spec.escaped_name);
                    let doc = match docs.remove(&doc_key) {
                        Some(Ok(doc)) => doc,
                        Some(Err(e)) => return Err(e),
                        None => {
                            return Err(fail(
                                ErrorCode::Eregistry,
                                format!("missing packument for {}", spec.fetch_name),
                            ));
                        }
                    };
                    let manifest = doc.versions.get(kept).ok_or_else(|| {
                        fail(
                            ErrorCode::Etarget,
                            format!("kept {name}@{kept} not in registry"),
                        )
                    })?;
                    insert_package(
                        &mut packages,
                        &mut queue,
                        spec,
                        manifest,
                        via_optional,
                        &dev_reachable,
                        opts,
                    )?;
                    // Put the doc back: another edge may share this round's fetch.
                    docs.insert(doc_key, Ok(doc));
                    continue;
                }
            }

            let reg = registry.registry_for(spec.scope.as_deref());
            let doc_key = format!("{reg}|{}", spec.escaped_name);
            let doc = match docs.remove(&doc_key) {
                Some(Ok(doc)) => doc,
                Some(Err(e)) => return Err(e),
                None => {
                    return Err(fail(
                        ErrorCode::Eregistry,
                        format!("missing packument for {}", spec.fetch_name),
                    ));
                }
            };
            let pick_opts = PickOptions {
                before: opts.before,
                exclude_from_age: excluded(&spec.fetch_name, &opts.release_age_exclude),
            };
            let version = pick_manifest(&doc, spec, &pick_opts)?;
            let key = key_of(&spec.fetch_name, &version);
            if !seen.insert(key.clone()) {
                docs.insert(doc_key, Ok(doc));
                continue;
            }
            let manifest = doc.versions.get(&version).ok_or_else(|| {
                fail(
                    ErrorCode::Etarget,
                    format!("registry has no {}@{version}", spec.fetch_name),
                )
            })?;
            insert_package(
                &mut packages,
                &mut queue,
                spec,
                manifest,
                via_optional,
                &dev_reachable,
                opts,
            )?;
            docs.insert(doc_key, Ok(doc));
        }
    }

    crate::profile::mark(&format!("resolve done ({} packages)", packages.len()));
    warnings.sort();
    Ok(Resolution {
        root,
        packages,
        warnings,
        workspaces: workspaces.to_vec(),
    })
}

/// A top's edge that resolves to a workspace: `workspace:` ranges always do;
/// a normal range does when the workspace version satisfies it.
fn workspace_edge<'a>(
    name: &str,
    range: &str,
    by_name: &HashMap<&str, &'a crate::workspaces::Workspace>,
) -> Option<&'a crate::workspaces::Workspace> {
    let spec = parse_dep(name, range, None).ok()?;
    if matches!(spec.spec_type, SpecType::Workspace) {
        let ws = by_name.get(name)?;
        if crate::semver::satisfies_str(&ws.version, &spec.fetch_spec, false) {
            return Some(*ws);
        }
        return None;
    }
    let ws = by_name.get(name)?;
    if valid_range_for_link(range) && crate::semver::satisfies_str(&ws.version, range, false) {
        return Some(*ws);
    }
    None
}

fn valid_range_for_link(range: &str) -> bool {
    // A workspace link needs a real range; tags/URLs never link.
    crate::semver::valid_range(range)
}

/// Edge values for a lockfile: workspace edges carry `link:<path>`,
/// everything else stays as declared (registry edges keep their ranges;
/// tarball edges their sources).
fn edge_map(
    deps: &HashMap<String, String>,
    by_name: &HashMap<&str, &crate::workspaces::Workspace>,
    _from_top: bool,
    warnings: &mut Vec<String>,
    owner: &str,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (dep, range) in deps {
        match workspace_edge(dep, range, by_name) {
            Some(ws) => {
                out.insert(dep.clone(), format!("link:{}", ws.path));
            }
            None => {
                // A bare range shadowed by a mismatched workspace goes to the
                // registry; say so once (upm reports the mismatch).
                if by_name.contains_key(dep.as_str()) && crate::semver::valid_range(range) {
                    warnings.push(format!(
                        "{owner}: {dep}@{range} does not match workspace {}, using the registry",
                        by_name[dep.as_str()].version
                    ));
                }
                out.insert(dep.clone(), range.clone());
            }
        }
    }
    out
}

fn is_url_spec(spec: &crate::spec::Spec) -> bool {
    spec.fetch_spec.starts_with("http://") || spec.fetch_spec.starts_with("https://")
}

#[allow(clippy::too_many_arguments)]
fn insert_package(
    packages: &mut HashMap<String, ResolvedPackage>,
    queue: &mut Vec<(String, String, bool, bool)>,
    spec: &crate::spec::Spec,
    manifest: &PackumentVersion,
    via_optional: bool,
    dev_reachable: &HashSet<String>,
    _opts: &ResolveOptions,
) -> Result<(), FlashnpmError> {
    use crate::registry::tarball_url;
    let name = manifest.name.clone();
    let version = manifest.version.clone();
    let key = key_of(&name, &version);
    if packages.contains_key(&key) {
        return Ok(());
    }
    let registry = "https://registry.npmjs.org"; // resolved URL fixed up in lock phase
    let resolved = manifest
        .dist
        .tarball
        .clone()
        .if_empty(|| tarball_url(registry, &name, &version));
    let integrity = manifest
        .dist
        .integrity
        .clone()
        .or_else(|| {
            manifest
                .dist
                .shasum
                .as_ref()
                .and_then(|s| crate::integrity::from_shasum(s).ok())
        })
        .ok_or_else(|| {
            fail(
                ErrorCode::Eintegrity,
                format!("{name}@{version} has no integrity"),
            )
        })?;

    let mut dependencies = BTreeMap::new();
    for (dep, range) in &manifest.dependencies {
        // resolve dep's final version later; record range now, rewrite in second pass
        dependencies.insert(dep.clone(), range.clone());
        queue.push((dep.clone(), range.clone(), via_optional, false));
    }
    // peer deps are installed (upm behavior); optional peers linked only if present — MVP installs them.
    for (dep, range) in &manifest.peer_dependencies {
        if !dependencies.contains_key(dep) {
            dependencies.insert(dep.clone(), range.clone());
            queue.push((dep.clone(), range.clone(), via_optional, false));
        }
    }
    let mut optional_dependencies = BTreeMap::new();
    for (dep, range) in &manifest.optional_dependencies {
        optional_dependencies.insert(dep.clone(), range.clone());
        queue.push((dep.clone(), range.clone(), true, false));
    }
    let _ = spec;
    packages.insert(
        key,
        ResolvedPackage {
            name: name.clone(),
            version,
            resolved,
            integrity,
            source: None,
            local: None,
            dependencies,
            optional_dependencies: if optional_dependencies.is_empty() {
                None
            } else {
                Some(optional_dependencies)
            },
            optional: via_optional,
            dev: dev_reachable.contains(&name),
            bin: normalize_bin(&name, &manifest.bin),
            os: manifest.os.clone(),
            cpu: manifest.cpu.clone(),
            libc: manifest.libc.clone(),
            peer_dependencies: if manifest.peer_dependencies.is_empty() {
                None
            } else {
                Some(
                    manifest
                        .peer_dependencies
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            },
        },
    );
    Ok(())
}

trait IfEmpty {
    fn if_empty(self, f: impl FnOnce() -> String) -> String;
}
impl IfEmpty for String {
    fn if_empty(self, f: impl FnOnce() -> String) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

/// Normalize `bin: "cmd"` | `{cmd: path}` into a map.
pub fn normalize_bin(name: &str, bin: &serde_json::Value) -> BTreeMap<String, String> {
    match bin {
        serde_json::Value::String(s) => {
            let short = name.rsplit('/').next().unwrap_or(name).to_string();
            BTreeMap::from([(short, s.clone())])
        }
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// Parse CLI specs for `resolve`/`fetch` output.
pub async fn resolve_specs(
    registry: &Registry,
    specs: &[String],
    opts: &ResolveOptions,
) -> Result<Vec<ResolvedPackage>, FlashnpmError> {
    let mut out = Vec::new();
    for raw in specs {
        let spec = parse_spec(raw, None)?;
        if matches!(spec.spec_type, SpecType::Tarball | SpecType::Workspace) {
            return Err(fail(
                ErrorCode::Eoption,
                format!("{} takes registry specs only: {raw}", "resolve"),
            ));
        }
        let doc = registry
            .packument(&spec.fetch_name, &spec.escaped_name, spec.scope.as_deref())
            .await?;
        let pick_opts = PickOptions {
            before: opts.before,
            exclude_from_age: excluded(&spec.fetch_name, &opts.release_age_exclude),
        };
        let version = pick_manifest(&doc, &spec, &pick_opts)?;
        let m = doc.versions.get(&version).unwrap();
        out.push(ResolvedPackage {
            name: m.name.clone(),
            version: m.version.clone(),
            resolved: m.dist.tarball.clone(),
            integrity: m.dist.integrity.clone().unwrap_or_default(),
            source: None,
            local: None,
            dependencies: BTreeMap::new(),
            optional_dependencies: None,
            optional: false,
            dev: false,
            bin: normalize_bin(&m.name, &m.bin),
            os: m.os.clone(),
            cpu: m.cpu.clone(),
            libc: m.libc.clone(),
            peer_dependencies: None,
        });
    }
    Ok(out)
}

/// Filter a resolution to the current platform (drops incompatible optional branches).
pub fn filter_platform(
    packages: &HashMap<String, ResolvedPackage>,
) -> HashMap<String, ResolvedPackage> {
    packages
        .iter()
        .filter(|(_, p)| runs_on(p))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn runs_on(pkg: &ResolvedPackage) -> bool {
    // MVP: honor `os`/`cpu` only; `libc` detection comes later.
    if let Some(os) = &pkg.os {
        if !os.iter().any(|o| o == std::env::consts::OS || o == "any") {
            return false;
        }
    }
    if let Some(cpu) = &pkg.cpu {
        if !cpu
            .iter()
            .any(|c| c == std::env::consts::ARCH || c == "any")
        {
            return false;
        }
    }
    true
}
