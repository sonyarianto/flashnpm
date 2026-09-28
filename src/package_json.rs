//! `package.json` read/write helpers. Port of `upm`'s `src/package-json.ts`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{FlashnpmError, ErrorCode};
use crate::resolve::RootManifest;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Emanifet, msg)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    workspaces: Option<WorkspacesDecl>,
    #[serde(default)]
    bin: serde_json::Value,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// `workspaces` as npm reads it: a list, or `{ packages: [...] }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspacesDecl {
    List(Vec<String>),
    Map { packages: Vec<String> },
}

pub async fn read_manifest(dir: &Path) -> Result<(RootManifest, serde_json::Value), FlashnpmError> {
    let file = dir.join("package.json");
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|e| fail(format!("cannot read {}: {e}", file.display())))?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| fail(format!("{} is not valid JSON: {e}", file.display())))?;
    let manifest: PackageJson = serde_json::from_value(raw.clone())
        .map_err(|e| fail(format!("{} is invalid: {e}", file.display())))?;
    let bin = crate::resolve::normalize_bin(manifest.name.as_deref().unwrap_or(""), &manifest.bin);
    Ok((
        RootManifest {
            name: manifest.name,
            version: manifest.version,
            dependencies: manifest.dependencies.into_iter().collect(),
            dev_dependencies: manifest.dev_dependencies.into_iter().collect(),
            optional_dependencies: manifest.optional_dependencies.into_iter().collect(),
            workspaces: manifest.workspaces.map(|w| match w {
                WorkspacesDecl::List(l) => l,
                WorkspacesDecl::Map { packages } => packages,
            }),
            bin,
            peer_dependencies: manifest.peer_dependencies.into_iter().collect(),
        },
        raw,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Dependencies,
    Dev,
    Optional,
}

impl Group {
    pub fn key(self) -> &'static str {
        match self {
            Self::Dependencies => "dependencies",
            Self::Dev => "devDependencies",
            Self::Optional => "optionalDependencies",
        }
    }
}

/// Save `range` for `name` into `group` in `package.json` (moves groups).
pub async fn save_dep(dir: &Path, name: &str, range: &str, group: Group) -> Result<(), FlashnpmError> {
    let file = dir.join("package.json");
    let text = tokio::fs::read_to_string(&file)
        .await
        .map_err(|e| fail(format!("cannot read {}: {e}", file.display())))?;
    let mut raw: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| fail(format!("invalid package.json: {e}")))?;
    let obj = raw
        .as_object_mut()
        .ok_or_else(|| fail("package.json must be an object"))?;
    for key in ["dependencies", "devDependencies", "optionalDependencies"] {
        if key != group.key() {
            if let Some(deps) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
                deps.remove(name);
            }
        }
    }
    let entry = obj
        .entry(group.key())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    entry
        .as_object_mut()
        .ok_or_else(|| fail(format!("{} must be an object", group.key())))?;
    entry[name] = serde_json::Value::String(range.to_string());
    // keep keys sorted for clean diffs
    for key in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(deps) = obj.get(key).and_then(|v| v.as_object()).cloned() {
            let mut sorted = BTreeMap::new();
            for (k, v) in deps {
                sorted.insert(k, v);
            }
            obj[key] = serde_json::Value::Object(sorted.into_iter().collect());
        }
    }
    let out = serde_json::to_string_pretty(&raw).map_err(|e| fail(e.to_string()))?;
    tokio::fs::write(&file, format!("{out}\n"))
        .await
        .map_err(|e| fail(e.to_string()))?;
    Ok(())
}

pub async fn remove_dep(dir: &Path, name: &str) -> Result<bool, FlashnpmError> {
    let file = dir.join("package.json");
    let text = tokio::fs::read_to_string(&file)
        .await
        .map_err(|e| fail(format!("cannot read {}: {e}", file.display())))?;
    let mut raw: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| fail(format!("invalid package.json: {e}")))?;
    let obj = raw
        .as_object_mut()
        .ok_or_else(|| fail("package.json must be an object"))?;
    let mut removed = false;
    for key in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(deps) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
            if deps.remove(name).is_some() {
                removed = true;
            }
        }
    }
    if !removed {
        return Ok(false);
    }
    let out = serde_json::to_string_pretty(&raw).map_err(|e| fail(e.to_string()))?;
    tokio::fs::write(&file, format!("{out}\n"))
        .await
        .map_err(|e| fail(e.to_string()))?;
    Ok(true)
}

/// Range to save for an `add`: explicit ranges stay; names/tags save `^version`
/// (or exact with `--exact` / `save-exact`).
pub fn save_range(spec: &crate::spec::Spec, version: &str, exact: bool) -> String {
    use crate::spec::SpecType;
    match spec.spec_type {
        SpecType::Version => spec.fetch_spec.clone(),
        SpecType::Range => {
            // A bare name (`*`) saves a caret of the selected version;
            // an explicit range saves as written.
            if spec.fetch_spec == "*" || spec.raw == spec.name {
                if exact {
                    version.to_string()
                } else {
                    format!("^{version}")
                }
            } else {
                spec.fetch_spec.clone()
            }
        }
        SpecType::Tag => {
            if exact || spec.fetch_spec == "*" {
                version.to_string()
            } else {
                format!("^{version}")
            }
        }
        SpecType::Workspace | SpecType::Tarball => spec.fetch_spec.clone(),
    }
}

/// Find the project root: nearest `package.json` walking up from `cwd`.
pub fn find_root(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join("package.json").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn group_maps(root: &RootManifest) -> HashMap<String, HashMap<String, String>> {
    HashMap::from([
        ("dependencies".to_string(), root.dependencies.clone()),
        ("devDependencies".to_string(), root.dev_dependencies.clone()),
        (
            "optionalDependencies".to_string(),
            root.optional_dependencies.clone(),
        ),
    ])
}
