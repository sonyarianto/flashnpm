//! Our own flat lockfile `flashnpm.lock`, keyed by identity (`name@version`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlashnpmError, ErrorCode};
use crate::registry::tarball_url;
use crate::resolve::{Resolution, ResolvedPackage, RootManifest};

pub const LOCKFILE: &str = "flashnpm.lock";
const VERSION: u32 = 1;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Elock, msg)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// Inner version of a tarball dep (its key ends in the source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    pub integrity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_dependencies: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockRoot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specs: Option<LockSpecs>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// The workspace patterns as declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockSpecs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BTreeMap<String, String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "devDependencies"
    )]
    pub dev_dependencies: Option<BTreeMap<String, String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "optionalDependencies"
    )]
    pub optional_dependencies: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    pub root: LockRoot,
    /// By root-relative path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<BTreeMap<String, WorkspaceEntry>>,
    pub packages: BTreeMap<String, LockEntry>,
}

/// A workspace: a top like the root, keyed by path. An edge to it reads
/// `"<name>": "link:<path>"`, so its identity is `<name>@link:<path>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specs: Option<LockSpecs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_dependencies: Option<BTreeMap<String, String>>,
}

pub fn to_lockfile(resolution: &Resolution, registry: &str) -> Lockfile {
    let mut packages = BTreeMap::new();
    for (key, pkg) in &resolution.packages {
        // Workspace leaves travel in `workspaces`, never as store entries.
        if pkg.local.is_some() {
            continue;
        }
        // A tarball's key says where it is; what is left is the version inside.
        if pkg.source.is_some() {
            packages.insert(
                key.clone(),
                LockEntry {
                    version: Some(pkg.version.clone()),
                    resolved: None,
                    integrity: pkg.integrity.clone(),
                    dependencies: none_if_empty(
                        pkg.dependencies
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    optional_dependencies: pkg
                        .optional_dependencies
                        .clone()
                        .and_then(none_if_empty),
                    bin: none_if_empty(pkg.bin.clone()),
                    os: pkg.os.clone(),
                    cpu: pkg.cpu.clone(),
                    libc: pkg.libc.clone(),
                    peer_dependencies: pkg.peer_dependencies.clone(),
                },
            );
            continue;
        }
        let derivable = pkg.resolved == tarball_url(registry, &pkg.name, &pkg.version);
        packages.insert(
            key.clone(),
            LockEntry {
                version: None,
                resolved: if derivable {
                    None
                } else {
                    Some(pkg.resolved.clone())
                },
                integrity: pkg.integrity.clone(),
                dependencies: none_if_empty(
                    pkg.dependencies
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
                optional_dependencies: pkg.optional_dependencies.clone().and_then(none_if_empty),
                bin: none_if_empty(pkg.bin.clone()),
                os: pkg.os.clone(),
                cpu: pkg.cpu.clone(),
                libc: pkg.libc.clone(),
                peer_dependencies: pkg.peer_dependencies.clone(),
            },
        );
    }
    Lockfile {
        lockfile_version: VERSION,
        root: LockRoot {
            name: resolution.root.name.clone(),
            version: resolution.root.version.clone(),
            specs: specs_of(
                &resolution.root.dependencies,
                &resolution.root.dev_dependencies,
                &resolution.root.optional_dependencies,
            ),
            dependencies: root_dependencies(resolution),
            workspaces: resolution.root.workspaces.clone().filter(|w| !w.is_empty()),
        },
        workspaces: {
            let mut map = BTreeMap::new();
            for pkg in resolution.packages.values() {
                if pkg.local.is_none() {
                    continue;
                }
                let path = pkg.local.clone().unwrap_or_default();
                map.insert(
                    path,
                    WorkspaceEntry {
                        name: pkg.name.clone(),
                        version: pkg.version.clone(),
                        specs: Some(LockSpecs {
                            dependencies: None,
                            dev_dependencies: None,
                            optional_dependencies: None,
                        }),
                        dependencies: none_if_empty(pkg.dependencies.clone()),
                        optional_dependencies: pkg
                            .optional_dependencies
                            .clone()
                            .and_then(none_if_empty),
                        bin: none_if_empty(pkg.bin.clone()),
                        peer_dependencies: pkg.peer_dependencies.clone(),
                    },
                );
            }
            // Fill each workspace's declared specs from the resolution's
            // workspace manifests.
            for ws in &resolution.workspaces {
                if let Some(entry) = map.get_mut(&ws.path) {
                    entry.specs = specs_of(
                        &ws.manifest.dependencies,
                        &ws.manifest.dev_dependencies,
                        &ws.manifest.optional_dependencies,
                    );
                }
            }
            if map.is_empty() {
                None
            } else {
                Some(map)
            }
        },
        packages,
    }
}

/// A direct edge's locked value: `link:<path>` for workspaces, the source
/// for tarballs, the version otherwise.
fn edge_value(pkg: &ResolvedPackage) -> String {
    if let Some(local) = &pkg.local {
        return format!("link:{local}");
    }
    if let Some(source) = &pkg.source {
        return source.clone();
    }
    pkg.version.clone()
}

/// The root's locked edges, one per declared dep: resolved against the
/// installed packages (never two values for one name — HashMap iteration
/// order must not leak into the lockfile).
fn root_dependencies(resolution: &Resolution) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for deps in [
        &resolution.root.dependencies,
        &resolution.root.dev_dependencies,
        &resolution.root.optional_dependencies,
    ] {
        for (dep, range) in deps {
            // Workspace and tarball edges have exact answers.
            if let Ok(spec) = crate::spec::parse_dep(dep, range, None) {
                use crate::spec::SpecType;
                match spec.spec_type {
                    SpecType::Workspace => {
                        if let Some(ws) = resolution
                            .workspaces
                            .iter()
                            .find(|w| w.name == *dep)
                            .filter(|w| {
                                crate::semver::satisfies_str(&w.version, &spec.fetch_spec, false)
                            })
                        {
                            out.insert(dep.clone(), format!("link:{}", ws.path));
                            continue;
                        }
                    }
                    SpecType::Tarball => {
                        let source = crate::spec::tarball_source(&spec.fetch_spec, "");
                        out.insert(dep.clone(), source);
                        continue;
                    }
                    _ => {}
                }
            }
            // Registry: the installed version satisfying the range (max on ties).
            let mut best: Option<&ResolvedPackage> = None;
            for pkg in resolution.packages.values() {
                if pkg.name != *dep || pkg.local.is_some() || pkg.source.is_some() {
                    continue;
                }
                if !crate::semver::satisfies_str(&pkg.version, range, false) {
                    continue;
                }
                if best.map_or(true, |b: &ResolvedPackage| {
                    crate::semver::parse(&pkg.version)
                        .zip(crate::semver::parse(&b.version))
                        .map(|(a, b)| crate::semver::compare(&a, &b) == std::cmp::Ordering::Greater)
                        .unwrap_or(false)
                }) {
                    best = Some(pkg);
                }
            }
            if let Some(pkg) = best {
                out.insert(dep.clone(), edge_value(pkg));
            }
        }
    }
    out
}

fn none_if_empty(m: BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    if m.is_empty() {
        None
    } else {
        Some(m)
    }
}

fn specs_of(
    deps: &HashMap<String, String>,
    dev: &HashMap<String, String>,
    opt: &HashMap<String, String>,
) -> Option<LockSpecs> {
    let specs = LockSpecs {
        dependencies: none_if_empty(deps.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        dev_dependencies: none_if_empty(dev.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        optional_dependencies: none_if_empty(
            opt.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ),
    };
    if specs.dependencies.is_none()
        && specs.dev_dependencies.is_none()
        && specs.optional_dependencies.is_none()
    {
        None
    } else {
        Some(specs)
    }
}

/// Rebuild a resolution from a lockfile (no network, no version picks).
pub fn from_lockfile(lock: &Lockfile, registry: &str) -> Result<Resolution, FlashnpmError> {
    let lock = validate(lock.clone())?;
    let mut packages: HashMap<String, ResolvedPackage> = HashMap::new();
    let mut workspaces: Vec<crate::workspaces::Workspace> = Vec::new();
    for (path, ws) in lock.workspaces.clone().unwrap_or_default() {
        packages.insert(
            crate::resolve::workspace_key(&ws.name, &path),
            ResolvedPackage {
                name: ws.name.clone(),
                version: ws.version.clone(),
                resolved: String::new(),
                integrity: String::new(),
                source: None,
                local: Some(path.clone()),
                dependencies: ws.dependencies.clone().unwrap_or_default(),
                optional_dependencies: ws.optional_dependencies.clone(),
                optional: false,
                dev: false,
                bin: ws.bin.clone().unwrap_or_default(),
                os: None,
                cpu: None,
                libc: None,
                peer_dependencies: ws.peer_dependencies.clone(),
            },
        );
        // Keep the workspace manifests so a lock round-trip is byte-stable
        // (specs feed `to_lockfile`; bins/peers feed the linker).
        let specs = ws.specs.clone().unwrap_or_default();
        workspaces.push(crate::workspaces::Workspace {
            path: path.clone(),
            dir: PathBuf::from(&path),
            name: ws.name.clone(),
            version: ws.version.clone(),
            manifest: RootManifest {
                name: Some(ws.name.clone()),
                version: Some(ws.version.clone()),
                dependencies: specs
                    .dependencies
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                dev_dependencies: specs
                    .dev_dependencies
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                optional_dependencies: specs
                    .optional_dependencies
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                workspaces: None,
                bin: ws.bin.clone().unwrap_or_default(),
                peer_dependencies: ws
                    .peer_dependencies
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            },
        });
    }
    for (key, entry) in &lock.packages {
        let at = key[1..]
            .find('@')
            .map(|i| i + 1)
            .ok_or_else(|| fail(format!("bad lock key {key:?}")))?;
        let name = key[..at].to_string();
        let tail = key[at + 1..].to_string();
        // Only a tarball entry carries its own version; the tail is then the source.
        let source = entry.version.as_ref().map(|_| tail.clone());
        let version = entry.version.clone().unwrap_or(tail);
        packages.insert(
            key.clone(),
            ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                resolved: source.clone().unwrap_or_else(|| {
                    entry
                        .resolved
                        .clone()
                        .unwrap_or_else(|| tarball_url(registry, &name, &version))
                }),
                integrity: entry.integrity.clone(),
                source,
                local: None,
                dependencies: entry.dependencies.clone().unwrap_or_default(),
                optional_dependencies: entry.optional_dependencies.clone(),
                optional: false,
                dev: false,
                bin: entry.bin.clone().unwrap_or_default(),
                os: entry.os.clone(),
                cpu: entry.cpu.clone(),
                libc: entry.libc.clone(),
                peer_dependencies: entry.peer_dependencies.clone(),
            },
        );
    }
    Ok(Resolution {
        root: RootManifest {
            name: lock.root.name.clone(),
            version: lock.root.version.clone(),
            dependencies: lock
                .root
                .specs
                .as_ref()
                .and_then(|s| s.dependencies.clone())
                .unwrap_or_default()
                .into_iter()
                .collect(),
            dev_dependencies: lock
                .root
                .specs
                .as_ref()
                .and_then(|s| s.dev_dependencies.clone())
                .unwrap_or_default()
                .into_iter()
                .collect(),
            optional_dependencies: lock
                .root
                .specs
                .as_ref()
                .and_then(|s| s.optional_dependencies.clone())
                .unwrap_or_default()
                .into_iter()
                .collect(),
            workspaces: lock.root.workspaces.clone(),
            bin: BTreeMap::new(),
            peer_dependencies: HashMap::new(),
        },
        packages,
        warnings: Vec::new(),
        workspaces,
    })
}

pub fn format_lockfile(lock: &Lockfile) -> Result<String, FlashnpmError> {
    let owned = validate(lock.clone())?;
    serde_json::to_string_pretty(&owned)
        .map(|s| format!("{s}\n"))
        .map_err(|e| fail(format!("cannot encode lockfile: {e}")))
}

pub fn parse_lockfile(text: &str) -> Result<Lockfile, FlashnpmError> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| fail(format!("{LOCKFILE} is not valid JSON: {e}")))?;
    let lock: Lockfile =
        serde_json::from_value(parsed).map_err(|e| fail(format!("{LOCKFILE} is invalid: {e}")))?;
    validate(lock)
}

pub async fn read_lockfile(dir: &Path) -> Result<Option<Lockfile>, FlashnpmError> {
    match tokio::fs::read(dir.join(LOCKFILE)).await {
        Ok(bytes) => Ok(Some(parse_lockfile(
            std::str::from_utf8(&bytes)
                .map_err(|e| fail(format!("{LOCKFILE} is not utf8: {e}")))?,
        )?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(fail(format!("cannot read {LOCKFILE}: {e}"))),
    }
}

pub async fn write_lockfile(dir: &Path, lock: &Lockfile) -> Result<(), FlashnpmError> {
    let text = format_lockfile(lock)?;
    let file = dir.join(LOCKFILE);
    let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
    tokio::fs::write(&tmp, text.as_bytes())
        .await
        .map_err(|e| fail(format!("cannot write {LOCKFILE}: {e}")))?;
    tokio::fs::rename(&tmp, &file)
        .await
        .map_err(|e| fail(format!("cannot rename {LOCKFILE}: {e}")))?;
    Ok(())
}

/// Whether the lockfile was made from this tree: same workspace patterns,
/// same workspaces at the same paths/names/versions, same declared ranges.
pub fn same_tree(
    lock: &Lockfile,
    root: &RootManifest,
    workspaces: &[crate::workspaces::Workspace],
) -> bool {
    let specs = lock.root.specs.as_ref();
    if !(eq_map(
        specs.and_then(|s| s.dependencies.as_ref()),
        &root.dependencies,
    ) && eq_map(
        specs.and_then(|s| s.dev_dependencies.as_ref()),
        &root.dev_dependencies,
    ) && eq_map(
        specs.and_then(|s| s.optional_dependencies.as_ref()),
        &root.optional_dependencies,
    )) {
        return false;
    }
    if serde_json::to_string(&lock.root.workspaces.clone().unwrap_or_default()).ok()
        != serde_json::to_string(&root.workspaces.clone().unwrap_or_default()).ok()
    {
        return false;
    }
    let locked = lock.workspaces.clone().unwrap_or_default();
    if locked.len() != workspaces.len() {
        return false;
    }
    workspaces.iter().all(|ws| match locked.get(&ws.path) {
        Some(entry) => {
            entry.name == ws.name
                && entry.version == ws.version
                && eq_map(
                    entry.specs.as_ref().and_then(|s| s.dependencies.as_ref()),
                    &ws.manifest.dependencies,
                )
                && eq_map(
                    entry
                        .specs
                        .as_ref()
                        .and_then(|s| s.dev_dependencies.as_ref()),
                    &ws.manifest.dev_dependencies,
                )
                && eq_map(
                    entry
                        .specs
                        .as_ref()
                        .and_then(|s| s.optional_dependencies.as_ref()),
                    &ws.manifest.optional_dependencies,
                )
        }
        None => false,
    })
}

fn eq_map(lock: Option<&BTreeMap<String, String>>, live: &HashMap<String, String>) -> bool {
    let a: BTreeMap<_, _> = lock.cloned().unwrap_or_default();
    let b: BTreeMap<_, _> = live.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    a == b
}

fn validate(lock: Lockfile) -> Result<Lockfile, FlashnpmError> {
    if lock.lockfile_version != VERSION {
        return Err(fail(format!(
            "unsupported lockfileVersion {}, expected {VERSION}",
            lock.lockfile_version
        )));
    }
    for (key, entry) in &lock.packages {
        if entry.integrity.trim().is_empty() {
            return Err(fail(format!("{key:?} has no integrity")));
        }
        crate::integrity::parse_integrity(&entry.integrity)
            .map_err(|e| fail(format!("{key:?}: {e}")))?;
    }
    for (path, ws) in lock.workspaces.clone().unwrap_or_default() {
        if ws.name.is_empty() || path.is_empty() {
            return Err(fail(format!("workspace {path:?} has no name")));
        }
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let lock = Lockfile {
            lockfile_version: 1,
            root: LockRoot {
                name: Some("app".to_string()),
                version: Some("1.0.0".to_string()),
                specs: None,
                dependencies: BTreeMap::from([("foo".to_string(), "1.0.0".to_string())]),
                workspaces: None,
            },
            workspaces: None,
            packages: BTreeMap::from([(
                "foo@1.0.0".to_string(),
                LockEntry {
                    version: None,
                    resolved: None,
                    integrity: crate::integrity::hash_sha512(b"foo"),
                    dependencies: None,
                    optional_dependencies: None,
                    bin: None,
                    os: None,
                    cpu: None,
                    libc: None,
                    peer_dependencies: None,
                },
            )]),
        };
        let text = format_lockfile(&lock).unwrap();
        let back = parse_lockfile(&text).unwrap();
        assert_eq!(back.packages.len(), 1);
    }
}
