//! Install-state cache: skip work when nothing changed.
//! Port of `upm`'s `src/state.ts` (MVP: hash of lockfile + manifest + platform + flags).

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{FlashnpmError, ErrorCode};
use crate::lock::Lockfile;
use crate::resolve::RootManifest;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Eio, msg)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub hash: String,
    pub inputs: StateInputs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateInputs {
    pub lock_hash: String,
    pub manifest_hash: String,
    pub platform: String,
    pub production: bool,
}

pub fn state_path(project_dir: &Path) -> std::path::PathBuf {
    project_dir.join("node_modules/.flashnpm/state.json")
}

pub fn hash_of(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

pub fn lock_hash(lock: &Lockfile) -> String {
    let text = serde_json::to_string(lock).unwrap_or_default();
    hash_of(&text)
}

pub fn manifest_hash(root: &RootManifest) -> String {
    let text = serde_json::to_string(&(
        &root.dependencies,
        &root.dev_dependencies,
        &root.optional_dependencies,
    ))
    .unwrap_or_default();
    hash_of(&text)
}

pub fn compute_state(
    lock: &Lockfile,
    root: &RootManifest,
    workspaces: &[crate::workspaces::Workspace],
    production: bool,
) -> InstallState {
    let lock_hash = lock_hash(lock);
    let manifest_hash = manifest_hash(root);
    let mut ws = workspaces
        .iter()
        .map(|w| format!("{}@{}:{}", w.name, w.version, w.path))
        .collect::<Vec<_>>();
    ws.sort();
    let platform = format!(
        "{}-{}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        production,
        ws.join(",")
    );
    let hash = hash_of(&format!("{lock_hash}:{manifest_hash}:{platform}"));
    InstallState {
        hash,
        inputs: StateInputs {
            lock_hash,
            manifest_hash,
            platform,
            production,
        },
    }
}

pub async fn read_state(project_dir: &Path) -> Option<InstallState> {
    let bytes = tokio::fs::read(state_path(project_dir)).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub async fn write_state(project_dir: &Path, state: &InstallState) -> Result<(), FlashnpmError> {
    let dir = state_path(project_dir);
    if let Some(parent) = dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| fail(e.to_string()))?;
    }
    let text = serde_json::to_vec_pretty(state).map_err(|e| fail(e.to_string()))?;
    tokio::fs::write(&dir, text)
        .await
        .map_err(|e| fail(e.to_string()))?;
    Ok(())
}

/// `true` when the recorded state matches this lock + manifest + flags.
pub async fn is_current(
    project_dir: &Path,
    lock: &Lockfile,
    root: &RootManifest,
    workspaces: &[crate::workspaces::Workspace],
    production: bool,
) -> bool {
    match read_state(project_dir).await {
        Some(state) => {
            let fresh = compute_state(lock, root, workspaces, production);
            state.hash == fresh.hash
        }
        None => false,
    }
}
