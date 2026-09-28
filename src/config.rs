//! `.npmrc` loading: global < user < project < env < CLI flags.
//! Port of `upm`'s `src/config.ts` (MVP: single-project, no workspaces).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{FlashnpmError, ErrorCode};

pub const NPMRC: &str = ".npmrc";
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Debug, Clone)]
pub struct Config {
    /// Default registry, no trailing slash.
    pub registry: String,
    /// `@scope` -> registry override.
    pub scopes: HashMap<String, String>,
    /// `//host/path/` -> `Authorization` header value.
    pub auth: HashMap<String, String>,
    pub save_exact: bool,
    /// Release cutoff in epoch ms; `None` disables the age gate.
    pub before: Option<i64>,
    pub release_age_exclude: Vec<String>,
    pub offline: bool,
    pub prefer_offline: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FlagOverrides {
    pub registry: Option<String>,
    pub min_release_age: Option<f64>,
    pub before: Option<String>,
    pub min_release_age_exclude: Option<Vec<String>>,
    pub offline: Option<bool>,
    pub prefer_offline: Option<bool>,
}

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Econfig, msg)
}

const AUTH_FIELDS: &[&str] = &["_authtoken", "_auth", "username", "_password"];

/// Parse `key=value` lines as npm's ini does. `env` supplies `${VAR}` expansion.
pub fn parse_npmrc(
    text: &str,
    env: &HashMap<String, String>,
) -> Result<HashMap<String, String>, FlashnpmError> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw in text.split_inclusive('\n') {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || line.starts_with('[')
        {
            continue;
        }
        let (written, mut value) = match line.split_once('=') {
            Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
            None => (line.to_string(), "true".to_string()),
        };
        let key = normalize_key(&written);
        // strip quotes or trailing `;`/`#` comments
        if value.len() > 1
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        } else if let Some(idx) = value.find([';', '#']) {
            value = value[..idx].trim().to_string();
        }
        if AUTH_FIELDS.contains(&key.as_str()) && !value.is_empty() {
            return Err(fail(format!(
                "{written} in .npmrc must be keyed by its registry: //host/path/:{written}"
            )));
        }
        value = expand_env(&value, env);
        if let Some(list) = key.strip_suffix("[]") {
            let entry = out.entry(list.to_string()).or_default();
            if !entry.is_empty() && !value.is_empty() {
                *entry = format!("{entry},{value}");
            } else if entry.is_empty() {
                *entry = value;
            }
        } else {
            out.insert(key, value);
        }
    }
    Ok(out)
}

fn expand_env(value: &str, env: &HashMap<String, String>) -> String {
    // `${VAR}` / `${VAR?}` with `\`-escapes, matching npm.
    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            // count preceding backslashes already emitted? simplify: no escape tracking
            // beyond a literal `\$` right before.
            if out.ends_with('\\') {
                out.pop();
                out.push_str("${");
                i += 2;
                continue;
            }
            if let Some(end) = value[i + 2..].find('}') {
                let inner = &value[i + 2..i + 2 + end];
                let (name, optional) = match inner.strip_suffix('?') {
                    Some(n) => (n, true),
                    None => (inner, false),
                };
                match env.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        if !optional {
                            out.push_str(&format!("${{{inner}}}"));
                        }
                    }
                }
                i += 2 + end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `npm_config_*` env plus `FLASHNPM_REGISTRY` fallback.
pub fn env_config(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in env {
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("npm_config_") || value.is_empty() {
            continue;
        }
        let key = &name["npm_config_".len()..];
        let normalized = if key.starts_with("//") {
            normalize_key(key)
        } else {
            normalize_key(&key.replace('_', "-"))
        };
        if AUTH_FIELDS.contains(&normalized.as_str()) {
            continue;
        }
        out.insert(normalized, value.clone());
    }
    if !out.contains_key("registry") {
        if let Some(r) = env.get("FLASHNPM_REGISTRY").or_else(|| env.get("UPM_REGISTRY")) {
            if !r.is_empty() {
                out.insert("registry".to_string(), r.clone());
            }
        }
    }
    out
}

pub fn registry_base(url: Option<&str>) -> String {
    let base = url.unwrap_or(DEFAULT_REGISTRY);
    base.trim_end_matches('/').to_string()
}

/// Merge layers (last wins; empty values ignored) into a [`Config`].
pub fn to_config(
    layers: &[HashMap<String, String>],
    registry_override: Option<&str>,
) -> Result<Config, FlashnpmError> {
    let mut merged: HashMap<String, String> = HashMap::new();
    for layer in layers {
        for (k, v) in layer {
            if !v.is_empty() {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    let base =
        registry_base(registry_override.or_else(|| merged.get("registry").map(String::as_str)));
    let mut scopes = HashMap::new();
    let mut fields: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (key, value) in &merged {
        if key.starts_with('@') && key.ends_with(":registry") {
            scopes.insert(
                key[..key.len() - ":registry".len()].to_string(),
                registry_base(Some(value)),
            );
        } else if key.starts_with("//") {
            if let Some(colon) = key.rfind(':') {
                let dart = key[..colon].to_string();
                fields
                    .entry(dart)
                    .or_default()
                    .insert(key[colon + 1..].to_string(), value.clone());
            }
        }
    }
    let mut auth = HashMap::new();
    for (dart, found) in &fields {
        if let Some(header) = authorization(found) {
            auth.insert(dart.clone(), header);
        }
    }
    // A registry credential also covers the rest of its host (npm behavior).
    for url in std::iter::once(&base).chain(scopes.values()) {
        if let Some(host) = host_of(url) {
            if !auth.contains_key(&host) {
                if let Some(found) = auth_for(&auth, &format!("{url}/")) {
                    auth.insert(host, found);
                }
            }
        }
    }
    Ok(Config {
        registry: base,
        scopes,
        auth,
        save_exact: merged
            .get("save-exact")
            .map(|s| s == "true")
            .unwrap_or(false),
        before: cutoff(layers)?,
        release_age_exclude: merged
            .get("min-release-age-exclude")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        offline: merged.get("offline").map(|s| s == "true").unwrap_or(false),
        prefer_offline: merged
            .get("prefer-offline")
            .map(|s| s == "true")
            .unwrap_or(false),
    })
}

fn cutoff(layers: &[HashMap<String, String>]) -> Result<Option<i64>, FlashnpmError> {
    const DAY_MS: i64 = 86_400_000;
    let now = chrono::Utc::now().timestamp_millis();
    let mut before = Some(now - DAY_MS);
    for layer in layers {
        if let Some(b) = layer.get("before").filter(|s| !s.is_empty()) {
            let parsed = chrono::DateTime::parse_from_rfc3339(b)
                .map(|d| d.timestamp_millis())
                .or_else(|_| {
                    chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
                })
                .map_err(|_| fail(format!("before={b} is not a date")))?;
            before = Some(parsed);
        } else if let Some(m) = layer.get("min-release-age").filter(|s| !s.is_empty()) {
            let days: f64 = m
                .parse()
                .map_err(|_| fail(format!("min-release-age={m} is not a number of days")))?;
            if days < 0.0 || !days.is_finite() {
                return Err(fail(format!("min-release-age={m} is not a number of days")));
            }
            before = if days == 0.0 {
                None
            } else {
                Some(chrono::Utc::now().timestamp_millis() - (days * 86_400_000.0) as i64)
            };
        }
    }
    Ok(before)
}

fn authorization(fields: &HashMap<String, String>) -> Option<String> {
    if let Some(t) = fields.get("_authtoken").filter(|s| !s.is_empty()) {
        return Some(format!("Bearer {t}"));
    }
    if let Some(a) = fields.get("_auth").filter(|s| !s.is_empty()) {
        return Some(format!("Basic {a}"));
    }
    match (fields.get("username"), fields.get("_password")) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => {
            use base64::Engine as _;
            let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(p) else {
                return None;
            };
            let password = String::from_utf8_lossy(&raw);
            let header =
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{password}"));
            Some(format!("Basic {header}"))
        }
        _ => None,
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| format!("//{}/", u.host_str().unwrap_or("")))
}

/// Longest-prefix `Authorization` match for a request URL.
pub fn auth_for(auth: &HashMap<String, String>, url: &str) -> Option<String> {
    let mut best: Option<(&String, &String)> = None;
    for (dart, header) in auth {
        let prefix = dart_to_prefix(dart);
        if url.starts_with(&prefix) && best.map_or(true, |(d, _)| dart.len() > d.len()) {
            best = Some((dart, header));
        }
    }
    best.map(|(_, h)| h.clone())
}

fn dart_to_prefix(dart: &str) -> String {
    // `//host/path/` -> `https://host/path/` and `http://host/path/`
    // Matching is done against both schemes by comparing host+path suffix.
    dart.to_string()
}

pub fn auth_header_for(auth: &HashMap<String, String>, url: &str) -> Option<String> {
    // match `//host/path/` darts against any scheme
    let parsed = url::Url::parse(url).ok()?;
    let host_path = format!("//{}{}", parsed.host_str().unwrap_or(""), parsed.path());
    let mut best: Option<(&String, &String)> = None;
    for (dart, header) in auth {
        if host_path.starts_with(dart.as_str())
            || format!("{host_path}/").starts_with(dart.as_str())
        {
            if best.map_or(true, |(d, _)| dart.len() > d.len()) {
                best = Some((dart, header));
            }
        }
    }
    // host-wide fallback: `//host/` covers the rest of the host
    best.map(|(_, h)| h.clone())
}

/// Read config for `dir`: global < user < project < env < flags.
pub fn read_config(dir: &Path, flags: &FlagOverrides) -> Result<Config, FlashnpmError> {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    read_config_with_env(dir, flags, &env_map, dirs::home_dir())
}

pub fn read_config_with_env(
    dir: &Path,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
    home: Option<PathBuf>,
) -> Result<Config, FlashnpmError> {
    let from_env = env_config(env);
    let expand_home = |p: &str| {
        if let Some(rest) = p.strip_prefix("~/") {
            home.as_ref()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| PathBuf::from(p))
        } else {
            PathBuf::from(p)
        }
    };
    let user_path = from_env
        .get("userconfig")
        .map(|p| expand_home(p))
        .or_else(|| home.clone().map(|h| h.join(NPMRC)))
        .unwrap_or_else(|| PathBuf::from(NPMRC));
    let user = parse_npmrc(&read_file(&user_path), env)?;
    let global_path = from_env
        .get("globalconfig")
        .map(|p| expand_home(p))
        .or_else(|| {
            user.get("globalconfig")
                .map(|p| expand_home(p))
                .or_else(|| {
                    let prefix = from_env
                        .get("prefix")
                        .cloned()
                        .or_else(|| user.get("prefix").cloned())
                        .or_else(|| env.get("PREFIX").cloned());
                    Some(global_file(prefix.as_deref()))
                })
        })
        .unwrap_or_else(|| PathBuf::from("/usr/local/etc/npmrc"));
    let global = parse_npmrc(&read_file(&global_path), env)?;
    let project = parse_npmrc(&read_file(&dir.join(NPMRC)), env)?;
    let mut cli = HashMap::new();
    if let Some(v) = flags.min_release_age {
        cli.insert("min-release-age".to_string(), v.to_string());
    }
    if let Some(b) = &flags.before {
        cli.insert("before".to_string(), b.clone());
    }
    if let Some(ex) = &flags.min_release_age_exclude {
        cli.insert("min-release-age-exclude".to_string(), ex.join(","));
    }
    if let Some(o) = flags.offline {
        cli.insert("offline".to_string(), o.to_string());
    }
    if let Some(p) = flags.prefer_offline {
        cli.insert("prefer-offline".to_string(), p.to_string());
    }
    to_config(
        &[global, user, project, from_env, cli],
        flags.registry.as_deref(),
    )
}

fn global_file(prefix: Option<&str>) -> PathBuf {
    let base = prefix.unwrap_or("/usr/local");
    PathBuf::from(base).join("etc/npmrc")
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn normalize_key(key: &str) -> String {
    if !key.starts_with("//") {
        return key.to_ascii_lowercase();
    }
    match key.rfind(':') {
        Some(i) => format!("{}{}", &key[..i], key[i..].to_ascii_lowercase()),
        None => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn parses_basic_npmrc() {
        let m = parse_npmrc("registry=https://r.example/\nsave-exact=true\n", &env()).unwrap();
        assert_eq!(m["registry"], "https://r.example/");
        assert_eq!(m["save-exact"], "true");
    }

    #[test]
    fn refuses_bare_credential() {
        assert!(parse_npmrc("_authToken=abc", &env()).is_err());
    }

    #[test]
    fn merges_layers() {
        let cfg = to_config(
            &[
                HashMap::from([("registry".to_string(), "https://a.example/".to_string())]),
                HashMap::from([("save-exact".to_string(), "true".to_string())]),
            ],
            None,
        )
        .unwrap();
        assert_eq!(cfg.registry, "https://a.example");
        assert!(cfg.save_exact);
    }
}
