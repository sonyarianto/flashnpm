//! Shared registry shapes: packuments, manifests, dist.
//! Port of `upm`'s `src/types.ts`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dist {
    #[serde(default)]
    pub tarball: String,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub shasum: Option<String>,
    #[serde(default)]
    pub file_count: Option<u64>,
    #[serde(default)]
    pub unpacked_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: HashMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    pub optional_dependencies: HashMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    pub peer_dependencies: HashMap<String, String>,
    #[serde(default, rename = "peerDependenciesMeta")]
    pub peer_dependencies_meta: HashMap<String, PeerMeta>,
    #[serde(default)]
    pub bin: serde_json::Value,
    #[serde(default)]
    pub os: Option<Vec<String>>,
    #[serde(default)]
    pub cpu: Option<Vec<String>>,
    #[serde(default)]
    pub libc: Option<Vec<String>>,
    #[serde(default)]
    pub dist: Option<Dist>,
    #[serde(default)]
    pub engines: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

/// Abbreviated packument version entry plus the fields `pick` needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackumentVersion {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    pub optional_dependencies: HashMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    pub peer_dependencies: HashMap<String, String>,
    #[serde(default)]
    pub dist: Dist,
    #[serde(default)]
    pub bin: serde_json::Value,
    #[serde(default)]
    pub os: Option<Vec<String>>,
    #[serde(default)]
    pub cpu: Option<Vec<String>>,
    #[serde(default)]
    pub libc: Option<Vec<String>>,
}

/// Full registry document for one package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Packument {
    pub name: String,
    #[serde(default, rename = "dist-tags")]
    pub dist_tags: HashMap<String, String>,
    #[serde(default)]
    pub versions: HashMap<String, PackumentVersion>,
    #[serde(default)]
    pub time: HashMap<String, String>,
    #[serde(default)]
    pub etag: Option<String>,
}
