//! `flashnpm exec` lookups: which installed bin a command means, which bin a
//! package runs, and where a package installs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{FlashnpmError, ErrorCode};
use crate::resolve::normalize_bin;

fn fail(code: ErrorCode, msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(code, msg)
}

/// Where exec installs for the project at `root`: its
/// `node_modules/.flashnpm/.exec`, so packages go with that tree, or
/// `~/.flashnpm/exec` when it has none.
pub fn exec_home(root: &Path) -> PathBuf {
    if root.join("node_modules").is_dir() && root.join("package.json").is_file() {
        return root.join("node_modules/.flashnpm/.exec");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flashnpm/exec")
}

fn bins_of(manifest: &serde_json::Value, name: &str) -> BTreeMap<String, String> {
    let pkg_name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(name);
    normalize_bin(
        pkg_name,
        manifest.get("bin").unwrap_or(&serde_json::Value::Null),
    )
}

/// The command as a bin of the nearest package.json above `dir`.
/// A file without an executable bit runs in node.
pub async fn self_bin(dir: &Path, command: &str) -> Option<Vec<String>> {
    let mut at = dir.to_path_buf();
    loop {
        let file = at.join("package.json");
        if file.is_file() {
            if let Ok(text) = tokio::fs::read_to_string(&file).await {
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) {
                    let bins = bins_of(&manifest, "");
                    if bins.contains_key(command) {
                        let target = at.join(&bins[command]);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            if let Ok(m) = tokio::fs::metadata(&target).await {
                                if m.permissions().mode() & 0o111 != 0 {
                                    return Some(vec![target.to_string_lossy().to_string()]);
                                }
                            }
                            return Some(vec![
                                "node".to_string(),
                                target.to_string_lossy().to_string(),
                            ]);
                        }
                        #[cfg(windows)]
                        {
                            return Some(vec![target.to_string_lossy().to_string()]);
                        }
                    }
                    return None;
                }
            }
            return None;
        }
        match at.parent() {
            Some(p) => {
                if p == at {
                    return None;
                }
                at = p.to_path_buf();
            }
            None => return None,
        }
    }
}

/// The bin the command runs where installed, nearest first: a bin of that
/// name, or the bin of the package the spec names where linked.
pub async fn local_bin(dir: &Path, command: &str) -> Option<Vec<String>> {
    let bin_name_ok =
        !command.contains('/') && !command.contains('\\') && command != "." && command != "..";
    let spec = crate::spec::parse_spec(command, None).ok();
    let fits = |version: Option<&str>| -> bool {
        let Some(spec) = &spec else { return false };
        if spec.raw == spec.name {
            return true;
        }
        // a version or range takes an installed package that matches it; a tag
        // always asks the registry (never a local fit).
        match spec.spec_type {
            crate::spec::SpecType::Version | crate::spec::SpecType::Range => {
                spec.name == spec.fetch_name
                    && version.map_or(false, |v| {
                        crate::semver::satisfies_str(v, &spec.fetch_spec, false)
                    })
            }
            _ => false,
        }
    };
    for bins in crate::run::bin_dirs(dir) {
        #[cfg(unix)]
        let shim = bins.join(command);
        #[cfg(windows)]
        let shim = bins.join(format!("{command}.cmd"));
        if bin_name_ok && shim.is_file() {
            return Some(vec![shim.to_string_lossy().to_string()]);
        }
        let Some(spec) = &spec else { continue };
        let pkg_file = bins
            .parent()
            .and_then(|p| p.parent())
            .map(|m| m.join(&spec.name).join("package.json"));
        let Some(pkg_file) = pkg_file else { continue };
        let Ok(text) = tokio::fs::read_to_string(&pkg_file).await else {
            continue;
        };
        let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let version = pkg.get("version").and_then(|v| v.as_str());
        if !fits(version) {
            continue;
        }
        let own = pick_bin(&bins_of(&pkg, &spec.name), &spec.name).ok()?;
        #[cfg(unix)]
        let found = bins.join(&own);
        #[cfg(windows)]
        let found = bins.join(format!("{own}.cmd"));
        if found.is_file() {
            return Some(vec![found.to_string_lossy().to_string()]);
        }
    }
    None
}

/// As npm picks: the one bin, or several that are one file under other
/// names; else the one named after the package without its scope.
pub fn pick_bin(bins: &BTreeMap<String, String>, name: &str) -> Result<String, FlashnpmError> {
    let names: Vec<&String> = bins.keys().collect();
    let files: std::collections::HashSet<&String> = bins.values().collect();
    if files.len() == 1 && !names.is_empty() {
        return Ok(names[0].clone());
    }
    let short = name.rsplit('/').next().unwrap_or(name);
    if bins.contains_key(short) {
        return Ok(short.to_string());
    }
    let why = if names.is_empty() {
        "has no bin".to_string()
    } else {
        format!(
            "has bins {} and none is {short}",
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Err(fail(ErrorCode::Enobin, format!("{name} {why}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_single_file_bins() {
        let bins = BTreeMap::from([
            ("a".to_string(), "bin.js".to_string()),
            ("b".to_string(), "bin.js".to_string()),
        ]);
        assert_eq!(pick_bin(&bins, "pkg").unwrap(), "a");
    }
}
