//! Link resolved packages into `node_modules/.flashnpm` + parent links.
//! Port of `upm`'s `src/link.ts` (MVP: no workspaces, no link workers).
//!
//! Layout mirrors upm: each `name@version` gets `node_modules/.flashnpm/<safe>/<files…>`,
//! and `node_modules/<name>` symlinks to it. Bins link into `node_modules/.bin`.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{FlashnpmError, ErrorCode};
use crate::resolve::ResolvedPackage;
use crate::store::Store;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Eio, msg)
}

fn safe_key(name: &str, version: &str) -> String {
    format!("{}@{}", name.replace('/', "+"), version.replace('/', "+"))
}

/// Root parent links: each declared dep name resolves to one target.
fn link_root_deps(
    modules: &Path,
    root: &crate::resolve::RootManifest,
    packages: &HashMap<String, ResolvedPackage>,
    by_name: &HashMap<&str, Vec<&ResolvedPackage>>,
    dot: &Path,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<(), FlashnpmError> {
    for (dep, edge) in root
        .dependencies
        .iter()
        .chain(root.dev_dependencies.iter())
        .chain(root.optional_dependencies.iter())
    {
        let optional = root.optional_dependencies.contains_key(dep);
        match resolve_edge_target(dep, edge, packages, by_name, dot, workspaces) {
            Some(target) => replace_symlink(&modules.join(dep.as_str()), &target)?,
            None if optional => continue,
            None => {
                return Err(fail(format!(
                    "cannot link {dep}: no installed package matches {edge}"
                )));
            }
        }
    }
    Ok(())
}

/// Entry dir for a package: registry `name@version`, tarball `name@source`
/// (slashes flattened so the source is never a path segment).
pub fn entry_dir(dot: &Path, pkg: &ResolvedPackage) -> std::path::PathBuf {
    match &pkg.source {
        Some(source) => dot.join(safe_key(&pkg.name, source)),
        None => dot.join(safe_key(&pkg.name, &pkg.version)),
    }
}

/// Link the full tree. Returns `(packages_linked, files_linked)`.
///
/// Registry and tarball packages land in `node_modules/.flashnpm` with per-entry
/// `node_modules` isolation; the root links only its direct deps (a missing
/// dependency fails instead of working by accident, as in upm). Workspaces
/// link by directory with their own dep links and `.bin`.
pub async fn link_tree(
    project_dir: &Path,
    root: &crate::resolve::RootManifest,
    packages: &HashMap<String, ResolvedPackage>,
    store: &Store,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<(usize, u64), FlashnpmError> {
    let modules = project_dir.join("node_modules");
    let dot = modules.join(".flashnpm");
    tokio::fs::create_dir_all(&dot)
        .await
        .map_err(|e| fail(format!("cannot create {}: {e}", dot.display())))?;

    let mut files_linked: u64 = 0;
    let mut linked = 0;
    // name -> candidate packages, versions descending (built once; edge
    // resolution below is a scan, no syscalls until the link itself).
    let mut by_name: HashMap<&str, Vec<&ResolvedPackage>> = HashMap::new();
    for pkg in packages.values() {
        if pkg.local.is_some() {
            continue;
        }
        by_name.entry(pkg.name.as_str()).or_default().push(pkg);
    }
    for list in by_name.values_mut() {
        list.sort_by(|a, b| {
            crate::semver::parse(&b.version)
                .zip(crate::semver::parse(&a.version))
                .map(|(x, y)| crate::semver::compare(&x, &y))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    for pkg in packages.values() {
        // Workspace leaves are directories, not store entries.
        if pkg.local.is_some() {
            continue;
        }
        if !crate::resolve::runs_on(pkg) {
            continue;
        }
        let entry = entry_dir(&dot, pkg);
        link_package(&entry, pkg, store).await?;
        link_isolation(&entry, pkg, packages, &by_name, &dot, workspaces)?;
        files_linked += 1;
        linked += 1;
    }
    // The root sees its direct deps resolved to single winners — never two
    // versions of one name.
    link_root_deps(&modules, root, packages, &by_name, &dot, workspaces)?;
    link_workspaces(project_dir, packages, &by_name, workspaces)?;
    sweep(&dot, &modules, packages, workspaces).await?;
    link_bins(project_dir, packages).await?;
    for ws in workspaces {
        link_workspace_bins(ws).await?;
    }
    write_state_sentinel(project_dir).await?;
    Ok((linked, files_linked))
}

/// A workspace's own links: `node_modules/<name>` -> the workspace dir for
/// every workspace the root sees, plus each workspace's dep links.
fn link_workspaces(
    project_dir: &Path,
    packages: &HashMap<String, ResolvedPackage>,
    by_name: &HashMap<&str, Vec<&ResolvedPackage>>,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<(), FlashnpmError> {
    let modules = project_dir.join("node_modules");
    let dot = modules.join(".flashnpm");
    for ws in workspaces {
        // Root link to the workspace directory itself.
        let dest = modules.join(&ws.name);
        replace_symlink(&dest, &ws.dir)?;
        // The workspace's own node_modules: its direct deps (registry ranges
        // resolve to the installed match, workspace edges to directories).
        let ws_modules = ws.dir.join("node_modules");
        std::fs::create_dir_all(&ws_modules).map_err(|e| fail(e.to_string()))?;
        let mut deps: Vec<(&String, &String)> = Vec::new();
        if let Some(pkg) = packages.get(&crate::resolve::workspace_key(&ws.name, &ws.path)) {
            deps.extend(pkg.dependencies.iter());
            if let Some(opt) = &pkg.optional_dependencies {
                deps.extend(opt.iter());
            }
        }
        for (dep, edge) in deps {
            let Some(target) = resolve_edge_target(dep, edge, packages, by_name, &dot, workspaces)
            else {
                continue;
            };
            let _ = replace_symlink(&ws_modules.join(dep.as_str()), &target);
        }
    }
    Ok(())
}

/// Resolve one dependency edge to the directory it links to: a workspace
/// dir, a `.flashnpm` entry, or nothing when the target isn't installed
/// (optional deps may be absent).
fn resolve_edge_target(
    dep: &str,
    edge: &str,
    packages: &HashMap<String, ResolvedPackage>,
    by_name: &HashMap<&str, Vec<&ResolvedPackage>>,
    dot: &Path,
    workspaces: &[crate::workspaces::Workspace],
) -> Option<std::path::PathBuf> {
    if let Some(path) = edge.strip_prefix("link:") {
        return workspaces
            .iter()
            .find(|w| w.path == path)
            .map(|w| w.dir.clone());
    }
    if edge.starts_with("file:") || edge.starts_with("http") {
        return packages
            .values()
            .find(|p| p.name == dep && p.source.as_ref() == Some(&edge.to_string()))
            .map(|p| entry_dir(dot, p));
    }
    by_name
        .get(dep)
        .and_then(|list| {
            list.iter().find(|p| p.version == edge).or_else(|| {
                list.iter().find(|p| {
                    crate::semver::parse(&p.version)
                        .map_or(false, |v| crate::semver::satisfies(&v, edge, false))
                })
            })
        })
        .map(|p| entry_dir(dot, p))
}

/// Per-entry isolation: `entry/node_modules/<dep>` for each direct dep, so a
/// package always sees its own declared deps (pnpm-style) even when the flat
/// root links another version. Sync and cheap: one symlink per edge, skipped
/// when already correct.
fn link_isolation(
    entry: &Path,
    pkg: &ResolvedPackage,
    packages: &HashMap<String, ResolvedPackage>,
    by_name: &HashMap<&str, Vec<&ResolvedPackage>>,
    dot: &Path,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<(), FlashnpmError> {
    let node_modules = entry.join("node_modules");
    let edges = pkg
        .dependencies
        .iter()
        .chain(pkg.optional_dependencies.iter().flatten());
    for (dep, edge) in edges {
        let Some(target) = resolve_edge_target(dep, edge, packages, by_name, dot, workspaces)
        else {
            continue;
        };
        if target == *entry {
            continue;
        }
        let dest = node_modules.join(dep.as_str());
        // Skip when already correct — relinks stay a single readlink.
        if let Ok(cur) = std::fs::read_link(&dest) {
            let abs = if cur.is_absolute() {
                cur
            } else {
                dest.parent().unwrap_or(&node_modules).join(&cur)
            };
            if abs == target {
                continue;
            }
        }
        let _ = replace_symlink(&dest, &target);
    }
    Ok(())
}

fn replace_symlink(dest: &Path, target: &Path) -> Result<(), FlashnpmError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(dest).map_err(|e| fail(e.to_string()))?;
        } else if meta.is_dir() {
            // Only our own empty dirs get replaced; user content is refused.
            if std::fs::read_dir(dest)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                std::fs::remove_dir(dest).map_err(|e| fail(e.to_string()))?;
            } else {
                return Err(fail(format!(
                    "refusing to replace non-empty directory {}",
                    dest.display()
                )));
            }
        } else {
            return Err(fail(format!("refusing to replace file {}", dest.display())));
        }
    }
    symlink_dir(target, dest).map_err(|e| fail(format!("cannot link {}: {e}", dest.display())))?;
    Ok(())
}

/// Remove `.flashnpm` entries (and their parent links) that the resolution no
/// longer contains. Only our own layout is touched: directories inside
/// `.flashnpm` and symlinks in `node_modules` pointing at them.
async fn sweep(
    dot: &Path,
    modules: &Path,
    packages: &HashMap<String, ResolvedPackage>,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<(), FlashnpmError> {
    use std::collections::HashSet;
    let mut keep: HashSet<String> = HashSet::new();
    for pkg in packages.values() {
        if pkg.local.is_some() {
            continue;
        }
        keep.insert(
            entry_dir(dot, pkg)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
    }
    // Workspace names are linked dirs, not entries: never sweep them here.
    let ws_names: HashSet<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
    let Ok(mut rd) = tokio::fs::read_dir(dot).await else {
        return Ok(());
    };
    let mut gone: Vec<String> = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        if name == "state.json" || name == ".complete" || name == ".exec" {
            continue;
        }
        if !keep.contains(&name) {
            gone.push(name);
        }
    }
    for name in &gone {
        let _ = tokio::fs::remove_dir_all(dot.join(name)).await;
    }
    if gone.is_empty() {
        return Ok(());
    }
    // Drop parent links that pointed into `.flashnpm` at a removed entry.
    // Workspace directory links point outside `.flashnpm` and are never touched.
    let Ok(mut rd) = tokio::fs::read_dir(modules).await else {
        return Ok(());
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let path = e.path();
        if path == *dot {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if ws_names.contains(name) {
                continue;
            }
        }
        let Ok(target) = tokio::fs::read_link(&path).await else {
            continue;
        };
        let abs = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(modules).join(&target)
        };
        if abs.parent() != Some(dot) {
            continue;
        }
        if let Some(base) = abs.file_name().and_then(|s| s.to_str()) {
            if gone.iter().any(|g| g == base) {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    Ok(())
}

async fn link_package(entry: &Path, pkg: &ResolvedPackage, store: &Store) -> Result<(), FlashnpmError> {
    let index = store.read_index_for(pkg).await?;
    tokio::fs::create_dir_all(entry)
        .await
        .map_err(|e| fail(format!("cannot create {}: {e}", entry.display())))?;
    for (rel, meta) in &index.files {
        let dest = entry.join(rel);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| fail(format!("cannot create dir: {e}")))?;
        }
        let blob = store.blob_path_for_digest(&meta.integrity)?;
        if tokio::fs::metadata(&dest).await.is_ok() {
            tokio::fs::remove_file(&dest)
                .await
                .map_err(|e| fail(e.to_string()))?;
        }
        if hardlink_or_copy(&blob, &dest).await.is_err() {
            return Err(fail(format!("cannot place {}", dest.display())));
        }
        #[cfg(unix)]
        if meta.mode & 0o111 != 0 {
            use std::os::unix::fs::PermissionsExt as _;
            let _ =
                tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(meta.mode)).await;
        }
    }
    // package.json bins are inside the entry already (part of the tarball).
    Ok(())
}

async fn hardlink_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    match tokio::fs::hard_link(src, dest).await {
        Ok(()) => Ok(()),
        Err(_) => tokio::fs::copy(src, dest).await.map(|_| ()),
    }
}

#[cfg(unix)]
fn symlink_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dest)
}

#[cfg(windows)]
fn symlink_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dest)
}

/// A workspace's own bins: `node_modules/.bin/<name>` -> files in the
/// workspace directory itself.
async fn link_workspace_bins(ws: &crate::workspaces::Workspace) -> Result<(), FlashnpmError> {
    if ws.manifest.bin.is_empty() {
        return Ok(());
    }
    let bin_dir = ws.dir.join("node_modules/.bin");
    tokio::fs::create_dir_all(&bin_dir)
        .await
        .map_err(|e| fail(e.to_string()))?;
    for (bin_name, rel) in &ws.manifest.bin {
        if bin_name.is_empty() || rel.is_empty() {
            continue;
        }
        let target = ws.dir.join(rel);
        let link = bin_dir.join(bin_name);
        let _ = tokio::fs::remove_file(&link).await;
        if tokio::fs::metadata(&target).await.is_ok() {
            #[cfg(unix)]
            let _ = tokio::fs::symlink(&target, &link).await;
            #[cfg(windows)]
            let _ = tokio::fs::copy(&target, &link.with_extension("cmd")).await;
        }
    }
    Ok(())
}

async fn link_bins(
    project_dir: &Path,
    packages: &HashMap<String, ResolvedPackage>,
) -> Result<(), FlashnpmError> {
    let bin_dir = project_dir.join("node_modules/.bin");
    tokio::fs::create_dir_all(&bin_dir)
        .await
        .map_err(|e| fail(e.to_string()))?;
    // Rebuilt from scratch so bins of removed packages disappear.
    if let Ok(mut rd) = tokio::fs::read_dir(&bin_dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
    for pkg in packages.values() {
        // Workspace bins live in their own `.bin` (linked per directory below).
        if pkg.local.is_some() {
            continue;
        }
        let entry = entry_dir(&project_dir.join("node_modules/.flashnpm"), pkg);
        for (bin_name, rel) in &pkg.bin {
            if bin_name.is_empty() || rel.is_empty() {
                continue;
            }
            let target = entry.join(rel);
            let link = bin_dir.join(bin_name);
            let _ = tokio::fs::remove_file(&link).await;
            #[cfg(unix)]
            {
                if tokio::fs::metadata(&target).await.is_ok() {
                    let _ = tokio::fs::symlink(&target, &link).await;
                    // ensure executable
                    use std::os::unix::fs::PermissionsExt as _;
                    if let Ok(m) = tokio::fs::metadata(&target).await {
                        let mut perm = m.permissions();
                        perm.set_mode(perm.mode() | 0o111);
                        let _ = tokio::fs::set_permissions(&target, perm).await;
                    }
                }
            }
            #[cfg(windows)]
            {
                // MVP on Windows: copy a cmd-shim-like placeholder.
                if let Ok(bytes) = tokio::fs::read(&target).await {
                    let _ = tokio::fs::write(&link.with_extension("cmd"), &bytes).await;
                }
            }
        }
    }
    Ok(())
}

async fn write_state_sentinel(project_dir: &Path) -> Result<(), FlashnpmError> {
    // Real install-state hashing lives in `state.rs`; the sentinel keeps
    // concurrent readers from seeing a half-linked tree.
    let dot = project_dir.join("node_modules/.flashnpm");
    tokio::fs::write(dot.join(".complete"), b"ok")
        .await
        .map_err(|e| fail(e.to_string()))?;
    Ok(())
}

/// Audit the tree against the store without trusting install state: file
/// sizes, package links and bins (not file hashes — same-size damage is out
/// of scope, as in upm). Repairs what it can from the store and returns the
/// number of repaired paths.
pub async fn verify_tree(
    project_dir: &Path,
    root: &crate::resolve::RootManifest,
    packages: &HashMap<String, ResolvedPackage>,
    store: &Store,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<u64, FlashnpmError> {
    let modules = project_dir.join("node_modules");
    let dot = modules.join(".flashnpm");
    let mut by_name: HashMap<&str, Vec<&ResolvedPackage>> = HashMap::new();
    for pkg in packages.values() {
        if pkg.local.is_some() {
            continue;
        }
        by_name.entry(pkg.name.as_str()).or_default().push(pkg);
    }
    let mut repaired: u64 = 0;
    // Entry files: size check per file (one stat each, no byte reads).
    for pkg in packages.values() {
        if pkg.local.is_some() {
            continue;
        }
        let entry = entry_dir(&dot, pkg);
        let Ok(index) = store.read_index_for(pkg).await else {
            continue;
        };
        for (rel, meta) in &index.files {
            let dest = entry.join(rel);
            let bad = tokio::fs::metadata(&dest)
                .await
                .map(|m| m.len() != meta.size)
                .unwrap_or(true);
            if !bad {
                continue;
            }
            let blob = store.blob_path_for_digest(&meta.integrity)?;
            let _ = tokio::fs::remove_file(&dest).await;
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| fail(e.to_string()))?;
            }
            hardlink_or_copy(&blob, &dest)
                .await
                .map_err(|e| fail(format!("cannot repair {}: {e}", dest.display())))?;
            repaired += 1;
        }
        // Entry isolation links.
        let node_modules = entry.join("node_modules");
        for (dep, edge) in pkg
            .dependencies
            .iter()
            .chain(pkg.optional_dependencies.iter().flatten())
        {
            let Some(target) = resolve_edge_target(dep, edge, packages, &by_name, &dot, workspaces)
            else {
                continue;
            };
            if target == entry {
                continue;
            }
            let dest = node_modules.join(dep.as_str());
            if link_ok(&dest, &target).await {
                continue;
            }
            let _ = replace_symlink(&dest, &target);
            repaired += 1;
        }
    }
    // Root + workspace links.
    repaired += verify_links(&modules, root, packages, &by_name, &dot, workspaces).await?;
    // Bins are rebuilt wholesale (one dir scan + symlinks).
    link_bins(project_dir, packages).await?;
    for ws in workspaces {
        link_workspace_bins(ws).await?;
    }
    Ok(repaired)
}

async fn link_ok(dest: &Path, target: &Path) -> bool {
    let Ok(cur) = tokio::fs::read_link(dest).await else {
        return false;
    };
    let abs = if cur.is_absolute() {
        cur
    } else {
        dest.parent().map(|p| p.join(&cur)).unwrap_or(cur)
    };
    abs == *target
}

async fn verify_links(
    modules: &Path,
    root: &crate::resolve::RootManifest,
    packages: &HashMap<String, ResolvedPackage>,
    by_name: &HashMap<&str, Vec<&ResolvedPackage>>,
    dot: &Path,
    workspaces: &[crate::workspaces::Workspace],
) -> Result<u64, FlashnpmError> {
    let mut repaired = 0;
    for (dep, edge) in root
        .dependencies
        .iter()
        .chain(root.dev_dependencies.iter())
        .chain(root.optional_dependencies.iter())
    {
        let optional = root.optional_dependencies.contains_key(dep);
        let target = resolve_edge_target(dep, edge, packages, by_name, dot, workspaces);
        match target {
            Some(t) => {
                if !link_ok(&modules.join(dep.as_str()), &t).await {
                    replace_symlink(&modules.join(dep.as_str()), &t)?;
                    repaired += 1;
                }
            }
            None if optional => {}
            None => {
                return Err(fail(format!(
                    "cannot verify {dep}: no installed package matches {edge}"
                )));
            }
        }
    }
    for ws in workspaces {
        if !link_ok(&modules.join(&ws.name), &ws.dir).await {
            replace_symlink(&modules.join(&ws.name), &ws.dir)?;
            repaired += 1;
        }
    }
    Ok(repaired)
}

/// Check the tree is standing (direct links present) without reading bytes.
pub async fn tree_standing(
    project_dir: &Path,
    root: &crate::resolve::RootManifest,
    packages: &HashMap<String, ResolvedPackage>,
    workspaces: &[crate::workspaces::Workspace],
) -> bool {
    let _ = packages;
    let modules = project_dir.join("node_modules");
    for dep in root
        .dependencies
        .keys()
        .chain(root.dev_dependencies.keys())
        .chain(root.optional_dependencies.keys())
    {
        if tokio::fs::symlink_metadata(modules.join(dep.as_str()))
            .await
            .is_err()
        {
            return false;
        }
    }
    for ws in workspaces {
        if tokio::fs::symlink_metadata(modules.join(&ws.name))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}
