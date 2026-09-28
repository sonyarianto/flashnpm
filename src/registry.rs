//! npm registry client: packuments with ETag revalidation and a
//! content cache under `<store>/metadata`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{FlashnpmError, ErrorCode};
use crate::types::Packument;

const MAX_AGE_SECS: u64 = 300;

fn fail(code: ErrorCode, msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(code, msg)
}

/// `https://registry/` + `@scope%2fname` for metadata.
pub fn packument_url(registry: &str, escaped_name: &str) -> String {
    format!("{}/{escaped_name}", registry.trim_end_matches('/'))
}

/// Conventional tarball URL, omitted from the lockfile when it matches.
pub fn tarball_url(registry: &str, name: &str, version: &str) -> String {
    let base = registry.trim_end_matches('/');
    let basename = name.rsplit('/').next().unwrap_or(name);
    format!("{base}/{name}/-/{basename}-{version}.tgz")
}

pub fn registry_base(url: Option<&str>) -> String {
    crate::config::DEFAULT_REGISTRY
        .to_string()
        .as_str()
        .to_string()
        .replace(
            crate::config::DEFAULT_REGISTRY,
            url.unwrap_or(crate::config::DEFAULT_REGISTRY),
        )
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Clone)]
pub struct Registry {
    client: reqwest::Client,
    pub config: Config,
    pub store_meta: PathBuf,
}

impl Registry {
    pub fn new(config: Config, store_dir: &Path) -> Result<Self, FlashnpmError> {
        let client = reqwest::Client::builder()
            .user_agent(format!("flashnpm/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .http2_adaptive_window(true)
            .tcp_nodelay(true)
            .build()
            .map_err(|e| {
                fail(
                    ErrorCode::Enetwork,
                    format!("cannot build http client: {e}"),
                )
            })?;
        Ok(Self {
            client,
            config,
            store_meta: store_dir.join("metadata"),
        })
    }

    pub fn registry_for(&self, scope: Option<&str>) -> String {
        match scope {
            Some(s) if self.config.scopes.contains_key(s) => self.config.scopes[s].clone(),
            _ => self.config.registry.clone(),
        }
    }

    fn cache_path(&self, registry: &str, escaped_name: &str) -> PathBuf {
        let host = url::Url::parse(registry)
            .map(|u| u.host_str().unwrap_or("registry").to_string())
            .unwrap_or_else(|_| "registry".to_string());
        self.store_meta
            .join(host)
            .join(escaped_name.replace("%2f", "@"))
    }

    /// Fetch a packument, using the on-disk copy when fresh.
    pub async fn packument(
        &self,
        name: &str,
        escaped_name: &str,
        scope: Option<&str>,
    ) -> Result<Packument, FlashnpmError> {
        // Slow-fetch diagnostics: only visible with FLASHNPM_PROFILE=1.
        let f0 = std::time::Instant::now();
        let out = self.packument_inner(name, escaped_name, scope).await;
        let ms = f0.elapsed().as_millis();
        if ms > 400 {
            crate::profile::mark(&format!("slow packument {name} {ms}ms"));
        }
        out
    }

    async fn packument_inner(
        &self,
        name: &str,
        escaped_name: &str,
        scope: Option<&str>,
    ) -> Result<Packument, FlashnpmError> {
        if self.config.offline {
            return self.cached(escaped_name, scope, true).await;
        }
        let registry = self.registry_for(scope);
        let url = packument_url(&registry, escaped_name);

        // Fresh cache wins (5 min, or indefinitely under prefer-offline / past-cutoff fetch).
        if let Ok(Some(doc)) = self.fresh_cache(&registry, escaped_name).await {
            if self.config.prefer_offline {
                return Ok(doc);
            }
            // fall through to revalidate below
        } else if self.config.prefer_offline {
            if let Ok(doc) = self.cached(escaped_name, scope, false).await {
                return Ok(doc);
            }
        }

        let etag = self.cached_etag(&registry, escaped_name).await;
        // Plain JSON, not the abbreviated manifest: the registry computes
        // the abbreviated view on the fly, which is an order of magnitude
        // slower than serving the full document for large packuments.
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/json");
        if let Some(auth) = crate::config::auth_header_for(&self.config.auth, &url) {
            req = req.header("Authorization", auth);
        }
        if let Some(etag) = etag {
            req = req.header("If-None-Match", etag);
        }
        let res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                fail(
                    ErrorCode::Etimedout,
                    format!("registry request timed out for {name}: {e}"),
                )
            } else {
                fail(
                    ErrorCode::Enetwork,
                    format!("registry request failed for {name}: {e}"),
                )
            }
        })?;

        if res.status() == reqwest::StatusCode::NOT_MODIFIED {
            return self.cached(escaped_name, scope, true).await;
        }
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(fail(ErrorCode::E404, format!("package not found: {name}")));
        }
        if !res.status().is_success() {
            return Err(fail(
                ErrorCode::Eregistry,
                format!("registry answered {} for {name}", res.status()),
            ));
        }
        let etag = res
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut doc: Packument = res.json().await.map_err(|e| {
            fail(
                ErrorCode::Eregistry,
                format!("invalid registry document for {name}: {e}"),
            )
        })?;
        doc.etag = etag.clone();
        self.write_cache(&registry, escaped_name, &doc).await?;
        // cut down to fields resolve reads (keep full versions map; drop readme etc.)
        Ok(doc)
    }

    async fn fresh_cache(
        &self,
        registry: &str,
        escaped_name: &str,
    ) -> Result<Option<Packument>, FlashnpmError> {
        let path = self.cache_path(registry, escaped_name);
        let meta = tokio::fs::metadata(&path).await.ok();
        let fresh = meta.and_then(|m| m.modified().ok()).map_or(false, |t| {
            SystemTime::now()
                .duration_since(t)
                .map_or(false, |d| d < Duration::from_secs(MAX_AGE_SECS))
        });
        if !fresh {
            return Ok(None);
        }
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(serde_json::from_slice::<CachedDoc>(&bytes)
                .ok()
                .map(|c| c.packument)),
            Err(_) => Ok(None),
        }
    }

    async fn cached(
        &self,
        escaped_name: &str,
        scope: Option<&str>,
        required: bool,
    ) -> Result<Packument, FlashnpmError> {
        let registry = self.registry_for(scope);
        let path = self.cache_path(&registry, escaped_name);
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<CachedDoc>(&bytes)
                .map(|c| c.packument)
                .map_err(|_| fail(ErrorCode::Eoffline, "cached registry document is corrupt")),
            Err(_) if required && self.config.offline => Err(fail(
                ErrorCode::Eoffline,
                "offline and no cached registry document",
            )),
            Err(_) if required => Err(fail(
                ErrorCode::Enetwork,
                "no cached registry document after 304",
            )),
            Err(_) => Err(fail(ErrorCode::Eoffline, "no cached registry document")),
        }
    }

    async fn cached_etag(&self, registry: &str, escaped_name: &str) -> Option<String> {
        let path = self.cache_path(registry, escaped_name);
        let bytes = tokio::fs::read(&path).await.ok()?;
        serde_json::from_slice::<CachedDoc>(&bytes).ok()?.etag
    }

    async fn write_cache(
        &self,
        registry: &str,
        escaped_name: &str,
        doc: &Packument,
    ) -> Result<(), FlashnpmError> {
        let path = self.cache_path(registry, escaped_name);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| fail(ErrorCode::Eio, format!("cannot create metadata dir: {e}")))?;
        }
        let cached = CachedDoc {
            etag: doc.etag.clone(),
            packument: doc.clone(),
        };
        let bytes = serde_json::to_vec(&cached)
            .map_err(|e| fail(ErrorCode::Eio, format!("cannot encode packument: {e}")))?;
        // atomic write via temp + rename
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| fail(ErrorCode::Eio, format!("cannot write metadata: {e}")))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| fail(ErrorCode::Eio, format!("cannot rename metadata: {e}")))?;
        Ok(())
    }

    /// Download a tarball, verifying integrity when known.
    pub async fn download(
        &self,
        url: &str,
        expected_integrity: Option<&str>,
    ) -> Result<Vec<u8>, FlashnpmError> {
        if self.config.offline {
            return Err(fail(
                ErrorCode::Eoffline,
                format!("offline: cannot fetch {url}"),
            ));
        }
        let mut req = self.client.get(url);
        if let Some(auth) = crate::config::auth_header_for(&self.config.auth, url) {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                fail(ErrorCode::Etimedout, format!("download timed out: {url}"))
            } else {
                fail(
                    ErrorCode::Enetwork,
                    format!("download failed for {url}: {e}"),
                )
            }
        })?;
        if !res.status().is_success() {
            return Err(fail(
                ErrorCode::Eregistry,
                format!("download answered {} for {url}", res.status()),
            ));
        }
        let bytes = res.bytes().await.map_err(|e| {
            fail(
                ErrorCode::Enetwork,
                format!("download body failed for {url}: {e}"),
            )
        })?;
        if let Some(integrity) = expected_integrity {
            crate::integrity::verify_bytes(&bytes, integrity)?;
        }
        Ok(bytes.to_vec())
    }
}

/// On-disk metadata envelope.
#[derive(Debug, Serialize, Deserialize)]
struct CachedDoc {
    etag: Option<String>,
    #[serde(flatten)]
    packument: Packument,
}

/// Registry hosts touched by a resolution (for install-state inputs).
pub fn hosts_for(packages: &HashMap<String, String>, _default: &str) -> Vec<String> {
    let mut hosts: Vec<String> = packages.keys().cloned().collect();
    hosts.sort();
    hosts.dedup();
    hosts
}
