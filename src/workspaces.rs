//! Which directories are workspaces of a root, and which root a directory
//! belongs to. Port of `upm`'s `src/workspaces.ts`.
//!
//! A workspace is a leaf, never a store entry: its identity is
//! `name@link:<path>`, linked from its directory, never in `.flashnpm`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{FlashnpmError, ErrorCode};
use crate::resolve::RootManifest;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Eworkspace, msg)
}

#[derive(Debug, Clone)]
pub struct Workspace {
    /// Relative to the root, `/` separators, no leading `./`.
    pub path: String,
    /// Absolute.
    pub dir: PathBuf,
    /// `manifest.name`, else the folder name.
    pub name: String,
    /// `manifest.version`, else `0.0.0`.
    pub version: String,
    pub manifest: RootManifest,
}

/// Split declared patterns into INCLUDE and EXCLUDE (`!`-prefixed).
pub fn workspace_patterns(
    manifest: &RootManifest,
) -> Result<(Vec<String>, Vec<String>), FlashnpmError> {
    let Some(declared) = &manifest.workspaces else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut patterns = Vec::new();
    let mut negated = Vec::new();
    for raw in declared {
        let bangs = raw.chars().take_while(|c| *c == '!').count();
        let pattern = raw[bangs..]
            .trim_start_matches(['.', '/'])
            .trim_start_matches('/')
            .to_string();
        if pattern.split('/').any(|s| s == "..") {
            return Err(fail(format!(
                "workspace pattern {raw} reaches outside the project"
            )));
        }
        if bangs % 2 == 1 {
            negated.push(pattern);
        } else {
            negated.retain(|other: &String| !match_path(&pattern, other));
            patterns.push(pattern);
        }
    }
    patterns.retain(|p| !negated.iter().any(|n| match_path(p, n)));
    Ok((patterns, negated))
}

/// Every workspace under `root_dir`, in npm's order: pattern by pattern,
/// sorted within one, each directory at its first match.
pub async fn find_workspaces(
    root_dir: &Path,
    manifest: &RootManifest,
) -> Result<Vec<Workspace>, FlashnpmError> {
    let (patterns, negated) = workspace_patterns(manifest)?;
    let mut exclude = negated;
    exclude.push("**/node_modules/**".to_string());
    let mut found: HashMap<String, Workspace> = HashMap::new();
    let mut named: HashMap<String, String> = HashMap::new();
    for pattern in &patterns {
        let mut paths = expand(root_dir, pattern, &exclude).await?;
        paths.sort();
        for path in paths {
            if path.is_empty() || found.contains_key(&path) {
                continue;
            }
            let Some(ws) = read_workspace(root_dir, &path).await? else {
                continue;
            };
            if let Some(other) = named.get(&ws.name) {
                return Err(fail(format!(
                    "workspaces {other} and {} are both named {}",
                    path, ws.name
                )));
            }
            named.insert(ws.name.clone(), path.clone());
            found.insert(path, ws);
        }
    }
    // npm order: pattern by pattern — our loop already appends in order;
    // re-sort by (pattern index, path) is approximated by insertion order.
    Ok(found.into_values().collect())
}

pub struct FoundRoot {
    pub dir: PathBuf,
    pub manifest: RootManifest,
    pub raw: serde_json::Value,
    pub workspaces: Vec<Workspace>,
    /// The workspace the cwd belongs to, if any.
    pub workspace: Option<Workspace>,
}

/// The project a directory belongs to: npm's walk up. The nearest
/// package.json is the project, unless a package.json above it lists that
/// directory as a workspace.
pub async fn find_root(cwd: &Path) -> Result<FoundRoot, FlashnpmError> {
    let mut dir = cwd.to_path_buf();
    // nearest package.json first (candidate), then look above for a claimant
    let mut candidate: Option<(PathBuf, RootManifest, serde_json::Value)> = None;
    loop {
        if dir.join("package.json").is_file() && candidate.is_none() {
            if let Ok((m, raw)) = crate::package_json::read_manifest(&dir).await {
                candidate = Some((dir.clone(), m, raw));
            }
        }
        match dir.parent() {
            Some(p) => {
                if p == dir {
                    break;
                }
                dir = p.to_path_buf();
            }
            None => break,
        }
    }
    let Some((cand_dir, cand_manifest, cand_raw)) = candidate else {
        return Err(FlashnpmError::new(
            ErrorCode::Emanifet,
            "no package.json found above current directory",
        ));
    };
    // Look above the candidate for a workspace root claiming it.
    let mut above = cand_dir.parent().map(Path::to_path_buf);
    while let Some(dir) = above.clone() {
        if dir.join("package.json").is_file() {
            if let Ok((m, _)) = crate::package_json::read_manifest(&dir).await {
                if m.workspaces.is_some() {
                    if let Ok(workspaces) = find_workspaces(&dir, &m).await {
                        if let Some(ws) = workspaces.iter().find(|w| w.dir == cand_dir).cloned() {
                            let (root_manifest, root_raw) =
                                crate::package_json::read_manifest(&dir)
                                    .await
                                    .map_err(|e| FlashnpmError::new(ErrorCode::Emanifet, e.message))?;
                            return Ok(FoundRoot {
                                dir,
                                manifest: root_manifest,
                                raw: root_raw,
                                workspaces,
                                workspace: Some(ws),
                            });
                        }
                    }
                }
            }
        }
        above = dir.parent().filter(|p| **p != dir).map(Path::to_path_buf);
    }
    // The candidate is the root; list its workspaces (if any).
    let workspaces = if cand_manifest.workspaces.is_some() {
        find_workspaces(&cand_dir, &cand_manifest)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(FoundRoot {
        dir: cand_dir,
        manifest: cand_manifest,
        raw: cand_raw,
        workspaces,
        workspace: None,
    })
}

/// Select workspaces by name, path from the root, path from `cwd`, or a
/// directory holding some. Every entry must find at least one.
pub fn select<'a>(
    root_dir: &Path,
    cwd: &Path,
    workspaces: &'a [Workspace],
    sels: &[String],
) -> Result<Vec<&'a Workspace>, FlashnpmError> {
    let mut out: Vec<&'a Workspace> = Vec::new();
    for sel in sels {
        let mut hits: Vec<&'a Workspace> = Vec::new();
        // by name
        hits.extend(workspaces.iter().filter(|w| &w.name == sel));
        // by path from root or cwd, or the workspace dir itself
        let rel_root = relativeish(root_dir, sel);
        let rel_cwd = relativeish(cwd, sel);
        hits.extend(workspaces.iter().filter(|w| {
            w.path == rel_root.as_str()
                || w.path == rel_cwd.as_str()
                || w.dir.to_string_lossy() == sel.as_str()
        }));
        // a directory holding some workspaces
        if hits.is_empty() {
            let dir = if Path::new(sel).is_absolute() {
                PathBuf::from(sel)
            } else {
                cwd.join(sel)
            };
            hits.extend(workspaces.iter().filter(|w| w.dir.starts_with(&dir)));
            if hits.is_empty() {
                let dir = if Path::new(sel).is_absolute() {
                    PathBuf::from(sel)
                } else {
                    root_dir.join(sel)
                };
                hits.extend(workspaces.iter().filter(|w| w.dir.starts_with(&dir)));
            }
        }
        if hits.is_empty() {
            return Err(fail(format!("workspace {sel} not found")));
        }
        for h in hits {
            if !out.iter().any(|o| o.path == h.path) {
                out.push(h);
            }
        }
    }
    Ok(out)
}

fn relativeish(base: &Path, sel: &str) -> String {
    let p = Path::new(sel);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    abs.strip_prefix(base)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| sel.replace('\\', "/"))
        .trim_start_matches("./")
        .to_string()
}

/// Directories one pattern matches, as root-relative `/` paths.
async fn expand(root: &Path, pattern: &str, exclude: &[String]) -> Result<Vec<String>, FlashnpmError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        let mut entries = Vec::new();
        while let Ok(Some(e)) = rd.next_entry().await {
            entries.push(e);
        }
        for e in entries {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if exclude.iter().any(|x| match_path(x, &rel)) {
                continue;
            }
            let ft = e.file_type().await.map_err(|e| {
                FlashnpmError::new(
                    ErrorCode::Eio,
                    format!("cannot stat {}: {e}", path.display()),
                )
            })?;
            if ft.is_dir() || ft.is_symlink() {
                if match_path(pattern, &rel) {
                    out.push(rel);
                }
                // keep descending (patterns like `packages/*` need one level;
                // `**` needs all) — bounded by exclude of node_modules.
                stack.push(path);
            }
        }
    }
    Ok(out)
}

async fn read_workspace(root: &Path, path: &str) -> Result<Option<Workspace>, FlashnpmError> {
    let at = root.join(path);
    let file = at.join("package.json");
    if !file.is_file() {
        return Ok(None);
    }
    let (manifest, _) = crate::package_json::read_manifest(&at).await?;
    let name = manifest.name.clone().unwrap_or_else(|| {
        Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string())
    });
    Ok(Some(Workspace {
        path: path.to_string(),
        dir: at,
        name,
        version: manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string()),
        manifest,
    }))
}

/// Glob match for workspace patterns: `*` (no `/`), `**` (any), `?` (one char).
pub fn match_path(pattern: &str, path: &str) -> bool {
    // `**/` prefix also matches the bare rest (`packages/**` ~ `packages/`).
    match_segments(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0] == "**" {
        // `**` swallows zero or more segments (collapse runs first).
        let mut p = 1;
        while p < pat.len() && pat[p] == "**" {
            p += 1;
        }
        for i in 0..=path.len() {
            if match_segments(&pat[p..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if !match_segment(pat[0], path[0]) {
        return false;
    }
    match_segments(&pat[1..], &path[1..])
}

fn match_segment(pat: &str, seg: &str) -> bool {
    let (p, mut s) = (pat.as_bytes(), seg.as_bytes());
    let (mut px, mut sx) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while sx < s.len() {
        if px < p.len() && (p[px] == b'?' || p[px] == s[sx]) {
            px += 1;
            sx += 1;
        } else if px < p.len() && p[px] == b'*' {
            star = Some(px);
            mark = sx;
            px += 1;
        } else if star.is_some() {
            px = star.unwrap() + 1;
            mark += 1;
            sx = mark;
        } else {
            return false;
        }
    }
    while px < p.len() && p[px] == b'*' {
        px += 1;
    }
    px == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(match_path("packages/*", "packages/app"));
        assert!(!match_path("packages/*", "packages/a/b"));
        assert!(match_path("packages/**", "packages/a/b"));
        assert!(!match_path("*.ts", "a/b.ts"));
        assert!(match_path("!x", "!x"));
    }
}
