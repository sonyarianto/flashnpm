//! Pick the registry version a spec resolves to. Port of `upm`'s `src/pick.ts`.
//!
//! Rules (same as upm):
//! - exact versions win as written (no age gate on re-locked pins — caller decides);
//! - tags fall back to the highest version at/below the tagged one that is old enough;
//! - ranges fail when nothing old enough matches, naming the cutoff.

use chrono::{DateTime, Utc};

use crate::error::{FlashnpmError, ErrorCode};
use crate::semver::{max_satisfying, parse as parse_version};
use crate::spec::{Spec, SpecType};
use crate::types::Packument;

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Etarget, msg)
}

#[derive(Debug, Clone)]
pub struct PickOptions {
    /// Epoch ms cutoff; versions published after it are skipped (unless excluded).
    pub before: Option<i64>,
    pub exclude_from_age: bool,
}

/// Choose a version for `spec` from `doc`.
pub fn pick_manifest(
    doc: &Packument,
    spec: &Spec,
    opts: &PickOptions,
) -> Result<String, FlashnpmError> {
    match spec.spec_type {
        SpecType::Version => {
            if !doc.versions.contains_key(&spec.fetch_spec) {
                return Err(fail(format!(
                    "No version {} for {}",
                    spec.fetch_spec, spec.fetch_name
                )));
            }
            Ok(spec.fetch_spec.clone())
        }
        SpecType::Tag => {
            let tagged = doc.dist_tags.get(&spec.fetch_spec);
            // Fall back to highest at-or-below tagged when too new.
            let mut candidates: Vec<&String> = doc.versions.keys().collect();
            candidates.sort_by(|a, b| {
                let (va, vb) = (parse_version(a), parse_version(b));
                match (va, vb) {
                    (Some(x), Some(y)) => crate::semver::compare(&x, &y),
                    _ => std::cmp::Ordering::Equal,
                }
            });
            if let Some(t) = tagged {
                let t_parsed = parse_version(t);
                candidates.retain(|v| {
                    parse_version(v).map_or(false, |pv| {
                        t_parsed.as_ref().map_or(true, |tv| {
                            crate::semver::compare(&pv, tv) != std::cmp::Ordering::Greater
                        })
                    })
                });
            }
            // newest-first, first old-enough wins
            candidates.reverse();
            for v in candidates {
                if old_enough(doc, v, opts) {
                    // tag should ideally equal dist-tag, but age may force an older one
                    return Ok(v.clone());
                }
            }
            Err(fail(format!(
                "No version of {} old enough for tag {}",
                spec.fetch_name, spec.fetch_spec
            )))
        }
        SpecType::Range => {
            let versions: Vec<&str> = doc.versions.keys().map(String::as_str).collect();
            // filter by age first (unless caller excluded the package)
            let eligible: Vec<&str> = if opts.exclude_from_age || opts.before.is_none() {
                versions
            } else {
                versions
                    .into_iter()
                    .filter(|v| old_enough(doc, v, opts))
                    .collect()
            };
            max_satisfying(eligible, &spec.fetch_spec, false).ok_or_else(|| {
                if doc.versions.is_empty() {
                    FlashnpmError::new(
                        ErrorCode::Enoversions,
                        format!("No versions for {}", spec.fetch_name),
                    )
                } else {
                    fail(format!(
                        "No version of {} matches {}",
                        spec.fetch_name, spec.fetch_spec
                    ))
                }
            })
        }
        SpecType::Workspace | SpecType::Tarball => Err(FlashnpmError::new(
            ErrorCode::Einvalidspec,
            "workspace/tarball specs never reach the registry",
        )),
    }
}

fn old_enough(doc: &Packument, version: &str, opts: &PickOptions) -> bool {
    if opts.exclude_from_age {
        return true;
    }
    let Some(cutoff) = opts.before else {
        return true;
    };
    let Some(published) = doc.time.get(version) else {
        return true; // missing time is not permission to block
    };
    let Ok(dt) = published.parse::<DateTime<Utc>>() else {
        return true;
    };
    dt.timestamp_millis() <= cutoff
}

/// Glob-match `min-release-age-exclude` (`*`, `**`, `?`).
pub fn excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

fn glob_match(pattern: &str, name: &str) -> bool {
    // tiny glob: `*` (no `/`), `**` (any), `?` (one char)
    fn rec(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        if p.len() >= 2 && p[0] == b'*' && p[1] == b'*' {
            // `**` — collapse runs, optional `/` skip
            let mut q = 2;
            while q < p.len() && p[q] == b'*' {
                q += 1;
            }
            for i in 0..=n.len() {
                if rec(&p[q..], &n[i..]) {
                    return true;
                }
            }
            return false;
        }
        if p[0] == b'*' {
            for i in 0..=n.len() {
                if i > 0 && n[i - 1] == b'/' {
                    break;
                }
                if rec(&p[1..], &n[i..]) {
                    return true;
                }
            }
            return false;
        }
        if n.is_empty() {
            return false;
        }
        if p[0] == b'?' || p[0] == n[0] {
            return rec(&p[1..], &n[1..]);
        }
        false
    }
    rec(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Dist, PackumentVersion};
    use std::collections::HashMap;

    fn doc() -> Packument {
        let mut versions = HashMap::new();
        for v in ["1.0.0", "1.2.0", "2.0.0"] {
            versions.insert(
                v.to_string(),
                PackumentVersion {
                    name: "foo".to_string(),
                    version: v.to_string(),
                    dependencies: HashMap::new(),
                    optional_dependencies: HashMap::new(),
                    peer_dependencies: HashMap::new(),
                    dist: Dist {
                        tarball: format!("https://r/foo/-/foo-{v}.tgz"),
                        integrity: None,
                        shasum: None,
                        file_count: None,
                        unpacked_size: None,
                    },
                    bin: serde_json::Value::Null,
                    os: None,
                    cpu: None,
                    libc: None,
                },
            );
        }
        Packument {
            name: "foo".to_string(),
            dist_tags: HashMap::from([("latest".to_string(), "2.0.0".to_string())]),
            versions,
            time: HashMap::new(),
            etag: None,
        }
    }

    #[test]
    fn picks_range() {
        let d = doc();
        let spec = crate::spec::parse_spec("foo@^1.0.0", None).unwrap();
        let v = pick_manifest(
            &d,
            &spec,
            &PickOptions {
                before: None,
                exclude_from_age: false,
            },
        )
        .unwrap();
        assert_eq!(v, "1.2.0");
    }
}
