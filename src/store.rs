//! Content-addressed store at `~/.flashnpm/store` (`files/`, `index/`, `metadata/`).
//!
//! Layout:
//! - `files/ab/cd/<base64url-sha512>` — raw file bytes, read-only
//! - `index/<name>/<version>.json` — file list + hashes per package
//! - `metadata/...` — registry docs (owned by `registry.rs`)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64U, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use crate::error::{FlashnpmError, ErrorCode};
use crate::registry::Registry;
use crate::resolve::ResolvedPackage;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Eio, msg)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub integrity: String,
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub integrity: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn default_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("FLASHNPM_STORE") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".flashnpm/store")
    }

    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    pub fn index_dir(&self) -> PathBuf {
        self.dir.join("index")
    }

    fn index_path(&self, name: &str, version: &str) -> PathBuf {
        self.index_dir()
            .join(sanitize(name))
            .join(format!("{version}.json"))
    }

    /// Tarball indexes live under `index/__tarball__/` keyed by source hash,
    /// so two sources with the same inner version never collide.
    fn tarball_index_path(&self, name: &str, source: &str) -> PathBuf {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(format!("{name}@{source}").as_bytes());
        self.index_dir()
            .join("__tarball__")
            .join(hex::encode(digest))
            .with_extension("json")
    }

    pub fn index_path_for(&self, pkg: &ResolvedPackage) -> PathBuf {
        match &pkg.source {
            Some(source) => self.tarball_index_path(&pkg.name, source),
            None => self.index_path(&pkg.name, &pkg.version),
        }
    }

    pub async fn has(&self, pkg: &ResolvedPackage) -> bool {
        tokio::fs::metadata(self.index_path_for(pkg)).await.is_ok()
    }

    /// Fetch + unpack every missing package. Returns count fetched.
    pub async fn fill(
        &self,
        registry: &Registry,
        packages: &[&ResolvedPackage],
        log: &dyn Fn(&str),
    ) -> Result<usize, FlashnpmError> {
        let mut fetched = 0;
        // bound concurrency; downloads are the bottleneck, not CPU
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
        let mut jobs = Vec::new();
        for pkg in packages {
            // Workspace leaves are directories, never store entries.
            if pkg.local.is_some() {
                continue;
            }
            if self.has(pkg).await {
                continue;
            }
            let store = self.clone();
            let reg = registry.clone();
            let pkg = (*pkg).clone();
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| fail(e.to_string()))?;
            jobs.push(tokio::spawn(async move {
                let _permit = permit;
                store.fetch_one(&reg, &pkg).await
            }));
        }
        for job in jobs {
            job.await
                .map_err(|e| fail(format!("fetch task failed: {e}")))??;
            fetched += 1;
            if fetched % 20 == 0 {
                log(&format!("fetched {fetched} packages"));
            }
        }
        Ok(fetched)
    }

    async fn fetch_one(&self, registry: &Registry, pkg: &ResolvedPackage) -> Result<(), FlashnpmError> {
        if let Some(source) = &pkg.source {
            // Tarball: bytes come from the source itself, pinned by lock integrity.
            let bytes = if source.starts_with("file:") {
                // `resolved` for a file: source is the source key; bytes live in the
                // project — but fill() has no project handle, so expect them adopted
                // during resolve. If missing, surface a clear error.
                return Err(fail(format!(
                    "missing store entry for {} (local tarballs are adopted at resolve time)",
                    crate::tarball::key_of(&pkg.name, source)
                )));
            } else {
                registry.download(source, Some(&pkg.integrity)).await?
            };
            return self.adopt_pkg(pkg, &bytes).await;
        }
        let bytes = registry
            .download(&pkg.resolved, Some(&pkg.integrity))
            .await?;
        self.adopt(&pkg.name, &pkg.version, &pkg.integrity, &bytes)
            .await
    }

    /// Adopt already-read bytes under a package's own index path
    /// (registry or tarball alike).
    pub async fn adopt_pkg(&self, pkg: &ResolvedPackage, bytes: &[u8]) -> Result<(), FlashnpmError> {
        crate::integrity::verify_bytes(bytes, &pkg.integrity)
            .map_err(|e| FlashnpmError::new(ErrorCode::Eintegrity, e.message))?;
        let files = unpack_tarball(bytes)?;
        self.write_blobs_and_index(&self.index_path_for(pkg), &pkg.integrity, &files)
            .await
    }

    /// Verify integrity, unpack the tarball, write blobs + index atomically.
    pub async fn adopt(
        &self,
        name: &str,
        version: &str,
        integrity: &str,
        bytes: &[u8],
    ) -> Result<(), FlashnpmError> {
        crate::integrity::verify_bytes(bytes, integrity)
            .map_err(|e| FlashnpmError::new(ErrorCode::Eintegrity, e.message))?;
        let files = unpack_tarball(bytes)?;
        self.write_blobs_and_index(&self.index_path(name, version), integrity, &files)
            .await
    }

    async fn write_blobs_and_index(
        &self,
        index_path: &Path,
        integrity: &str,
        files: &[(String, (Vec<u8>, u32))],
    ) -> Result<(), FlashnpmError> {
        if let Some(parent) = index_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| fail(format!("cannot create index dir: {e}")))?;
        }
        for (path, (data, _mode)) in files.iter() {
            let digest = Sha512::digest(data);
            let blob = self.blob_path(&digest);
            if tokio::fs::metadata(&blob).await.is_err() {
                if let Some(parent) = blob.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| fail(format!("cannot create files dir: {e}")))?;
                }
                tokio::fs::write(&blob, data)
                    .await
                    .map_err(|e| fail(format!("cannot write blob for {path}: {e}")))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ =
                        tokio::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o444))
                            .await;
                }
            }
        }
        let index = IndexEntry {
            integrity: integrity.to_string(),
            files: files
                .iter()
                .map(|(path, (data, mode))| {
                    let digest = Sha512::digest(data);
                    (
                        path.clone(),
                        FileMeta {
                            integrity: format!(
                                "sha512-{}",
                                base64::Engine::encode(
                                    &base64::engine::general_purpose::STANDARD,
                                    digest
                                )
                            ),
                            size: data.len() as u64,
                            mode: *mode,
                        },
                    )
                })
                .collect(),
        };
        let tmp = index_path.with_extension("tmp");
        let text = serde_json::to_vec_pretty(&index).map_err(|e| fail(e.to_string()))?;
        tokio::fs::write(&tmp, &text)
            .await
            .map_err(|e| fail(e.to_string()))?;
        tokio::fs::rename(&tmp, index_path)
            .await
            .map_err(|e| fail(e.to_string()))?;
        Ok(())
    }

    fn blob_path(&self, digest: &[u8]) -> PathBuf {
        let s = B64U.encode(digest);
        self.files_dir().join(&s[..2]).join(&s[2..4]).join(s)
    }

    pub async fn read_index(&self, name: &str, version: &str) -> Result<IndexEntry, FlashnpmError> {
        let bytes = tokio::fs::read(self.index_path(name, version))
            .await
            .map_err(|e| fail(format!("missing index for {name}@{version}: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| fail(format!("corrupt index for {name}@{version}: {e}")))
    }

    pub async fn read_index_for(&self, pkg: &ResolvedPackage) -> Result<IndexEntry, FlashnpmError> {
        let path = self.index_path_for(pkg);
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            let key = match &pkg.source {
                Some(s) => crate::tarball::key_of(&pkg.name, s),
                None => format!("{}@{}", pkg.name, pkg.version),
            };
            fail(format!("missing index for {key}: {e}"))
        })?;
        serde_json::from_slice(&bytes).map_err(|e| fail(format!("corrupt store index: {e}")))
    }

    pub fn blob_path_for_digest(&self, integrity: &str) -> Result<PathBuf, FlashnpmError> {
        use base64::Engine as _;
        let parsed = crate::integrity::parse_integrity(integrity)
            .map_err(|e| FlashnpmError::new(ErrorCode::Eintegrity, e.message))?;
        if !matches!(parsed.algorithm, crate::integrity::Algorithm::Sha512) {
            return Err(fail("only sha512 blobs are addressable in flashnpm 0.1"));
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&parsed.digest)
            .map_err(|e| fail(e.to_string()))?;
        Ok(self.blob_path(&raw))
    }
}

fn sanitize(name: &str) -> String {
    name.replace('/', "@").replace('%', "@")
}

/// Unpack `.tgz`/`.tar.gz`/`.tar` into `path -> (bytes, mode)`.
/// Strips the leading `package/` prefix; rejects escapes.
pub fn unpack_tarball(bytes: &[u8]) -> Result<Vec<(String, (Vec<u8>, u32))>, FlashnpmError> {
    // gzip first; an empty result falls back to plain tar.
    match read_archive(flate2::read::GzDecoder::new(bytes)) {
        Ok(files) if !files.is_empty() => Ok(files),
        _ => {
            let files = read_archive(bytes)?;
            if files.is_empty() {
                return Err(fail("tarball holds no files"));
            }
            Ok(files)
        }
    }
}

fn read_archive<R: std::io::Read>(reader: R) -> Result<Vec<(String, (Vec<u8>, u32))>, FlashnpmError> {
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    let entries = archive
        .entries()
        .map_err(|e| fail(format!("cannot read tarball: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| fail(format!("bad tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| fail(format!("bad tar path: {e}")))?
            .to_string_lossy()
            .to_string();
        let rel = path.strip_prefix("package/").unwrap_or(&path).to_string();
        if rel.is_empty() || rel == "/" {
            continue;
        }
        if rel.contains("..") || rel.starts_with('/') {
            return Err(FlashnpmError::new(
                ErrorCode::Eintegrity,
                format!("tarball escapes its directory: {rel:?}"),
            ));
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let mode = entry.header().mode().unwrap_or(0o644);
        let mut data = Vec::new();
        use std::io::Read as _;
        entry
            .read_to_end(&mut data)
            .map_err(|e| fail(format!("cannot read {rel}: {e}")))?;
        out.push((rel, (data, mode)));
    }
    Ok(out)
}

pub fn store_dir_override(explicit: Option<&Path>) -> PathBuf {
    explicit.map_or_else(Store::default_dir, Path::to_path_buf)
}
