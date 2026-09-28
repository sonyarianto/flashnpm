//! Tarball dependencies: `file:` paths and `http(s)` URLs.
//! Port of `upm`'s `src/tarball-deps.ts` + the `Tarball` half of `src/store.ts`.
//!
//! Identity is `name@<source>` where source is the URL as given or
//! `file:` + the root-relative `/` path. Bytes are pinned by integrity in
//! `flashnpm.lock`; URLs never re-read, local files re-read when replaced.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{FlashnpmError, ErrorCode};
use crate::registry::Registry;
use crate::spec::tarball_source;

fn fail(code: ErrorCode, msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(code, msg)
}

/// Lockfile key for a tarball dep.
pub fn key_of(name: &str, source: &str) -> String {
    format!("{name}@{source}")
}

fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Read a tarball's bytes: local file (relative to `root_dir`) or registry URL.
pub async fn read_bytes(
    source: &str,
    root_dir: &Path,
    registry: &Registry,
) -> Result<Vec<u8>, FlashnpmError> {
    if is_url(source) {
        return registry.download(source, None).await;
    }
    let rel = source.strip_prefix("file:").ok_or_else(|| {
        fail(
            ErrorCode::Einvalidspec,
            format!("bad tarball source {source:?}"),
        )
    })?;
    if rel.is_empty() || rel.starts_with('/') {
        return Err(fail(
            ErrorCode::Einvalidspec,
            format!("tarball path is not project-relative: {source:?}"),
        ));
    }
    let path = root_dir.join(rel);
    tokio::fs::read(&path).await.map_err(|e| {
        fail(
            ErrorCode::Emanifet,
            format!("cannot read tarball {}: {e}", path.display()),
        )
    })
}

#[derive(Debug, Clone)]
pub struct InnerManifest {
    pub name: String,
    pub version: String,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub bin: BTreeMap<String, String>,
    pub os: Option<Vec<String>>,
    pub cpu: Option<Vec<String>>,
    pub libc: Option<Vec<String>>,
}

/// Unpack bytes, read + validate the inner `package.json`.
pub fn inner_manifest(bytes: &[u8], source: &str) -> Result<InnerManifest, FlashnpmError> {
    let files = crate::store::unpack_tarball(bytes)?;
    let (_, (data, _)) = files
        .iter()
        .find(|(p, _)| p == "package.json")
        .ok_or_else(|| fail(ErrorCode::Emanifet, format!("{source} has no package.json")))?;
    let value: serde_json::Value = serde_json::from_slice(data).map_err(|e| {
        fail(
            ErrorCode::Emanifet,
            format!("package.json of {source} is not valid JSON: {e}"),
        )
    })?;
    manifest_from_value(&value, source)
}

pub fn manifest_from_value(
    value: &serde_json::Value,
    source: &str,
) -> Result<InnerManifest, FlashnpmError> {
    let obj = value.as_object().ok_or_else(|| {
        fail(
            ErrorCode::Emanifet,
            format!("package.json of {source} must be an object"),
        )
    })?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return Err(fail(
            ErrorCode::Emanifet,
            format!("package.json of {source} has no name"),
        ));
    }
    let version_raw = obj.get("version").and_then(|v| v.as_str()).unwrap_or("");
    let version = crate::semver::parse(version_raw)
        .map(|v| v.version)
        .ok_or_else(|| {
            fail(
                ErrorCode::Emanifet,
                format!("package.json of {source} has no valid version"),
            )
        })?;
    let bin = bin_map(&name, obj.get("bin"));
    Ok(InnerManifest {
        name,
        version,
        dependencies: str_map(obj.get("dependencies"), source, "dependencies")?,
        optional_dependencies: str_map(
            obj.get("optionalDependencies"),
            source,
            "optionalDependencies",
        )?,
        peer_dependencies: str_map(obj.get("peerDependencies"), source, "peerDependencies")?,
        bin,
        os: str_list(obj.get("os")),
        cpu: str_list(obj.get("cpu")),
        libc: str_list(obj.get("libc")),
    })
}

fn str_map(
    value: Option<&serde_json::Value>,
    source: &str,
    field: &str,
) -> Result<BTreeMap<String, String>, FlashnpmError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let obj = value.as_object().ok_or_else(|| {
        fail(
            ErrorCode::Emanifet,
            format!("package.json of {source}: {field} is not a map"),
        )
    })?;
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        let s = v.as_str().ok_or_else(|| {
            fail(
                ErrorCode::Emanifet,
                format!("package.json of {source}: {field}.{k} is not a string"),
            )
        })?;
        out.insert(k.clone(), s.to_string());
    }
    Ok(out)
}

fn str_list(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    match value {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(vec![s.clone()]),
        Some(serde_json::Value::Array(items)) => {
            let list: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if list.len() == items.len() {
                Some(list)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn bin_map(name: &str, bin: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    match bin {
        Some(serde_json::Value::String(s)) => {
            let short = name.rsplit('/').next().unwrap_or(name).to_string();
            BTreeMap::from([(short, s.clone())])
        }
        Some(serde_json::Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// The name a bare `add ./lib.tgz` calls itself.
pub async fn name_of(
    fetch_spec: &str,
    manifest_dir: &Path,
    cwd: &Path,
    registry: &Registry,
) -> Result<String, FlashnpmError> {
    // `fetch_spec` is cwd-relative; rebase to manifest-relative first.
    let rebased = from_cwd(manifest_dir, cwd, fetch_spec);
    let root_rel = manifest_dir.to_string_lossy().to_string();
    let _ = root_rel;
    // read relative to cwd-turned-absolute: resolve against manifest_dir's parent chain
    // by treating manifest_dir as the root.
    let source = tarball_source(&rebased, "");
    let bytes = read_bytes(&source, manifest_dir, registry).await?;
    Ok(inner_manifest(&bytes, &source)?.name)
}

/// Rewrite a cwd-relative `file:` spec to manifest-relative (like upm's `fromCwd`).
pub fn from_cwd(manifest_dir: &Path, cwd: &Path, fetch_spec: &str) -> String {
    let path = match fetch_spec.strip_prefix("file:") {
        Some(p) => p,
        None => return fetch_spec.to_string(),
    };
    let abs = cwd.join(path);
    let rel = pathdiff(&abs, manifest_dir);
    format!("file:{}", rel.replace('\\', "/"))
}

fn pathdiff(abs: &Path, dir: &Path) -> String {
    // Minimal relative-path computation without extra deps.
    let abs = normalize(abs);
    let dir = normalize(dir);
    let mut a = abs.components().peekable();
    let mut d = dir.components().peekable();
    // strip common prefix
    loop {
        match (a.peek(), d.peek()) {
            (Some(x), Some(y)) if x == y => {
                a.next();
                d.next();
            }
            _ => break,
        }
    }
    let mut out = String::new();
    for _ in d {
        out.push_str("../");
    }
    let rest: Vec<String> = a
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    out.push_str(&rest.join("/"));
    if out.is_empty() {
        ".".to_string()
    } else {
        out.trim_end_matches('/').to_string()
    }
}

fn normalize(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(c.as_os_str()),
        }
    }
    out
}

/// Stale-lock error for a URL tarball whose bytes changed (remove + add again).
pub fn stale(source: &str, inner: String) -> FlashnpmError {
    fail(
        ErrorCode::Eintegrity,
        format!("{source} changed since flashnpm.lock locked it ({inner}); remove it and add it again to lock the new one"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shapes() {
        assert_eq!(
            key_of("lib", "file:vendor/lib.tgz"),
            "lib@file:vendor/lib.tgz"
        );
    }

    #[test]
    fn from_cwd_rebases() {
        let manifest = Path::new("/proj");
        let cwd = Path::new("/proj");
        assert_eq!(
            from_cwd(manifest, cwd, "file:vendor/lib.tgz"),
            "file:vendor/lib.tgz"
        );
    }
}
