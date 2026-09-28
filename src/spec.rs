//! Replacement for `npm-package-arg`: version, range, tag, alias,
//! workspace and tarball specs.

use crate::error::{FlashnpmError, ErrorCode};
use crate::semver::{parse as parse_version, valid_range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecType {
    Version,
    Range,
    Tag,
    Workspace,
    Tarball,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub raw: String,
    /// Name the dep installs under. Aliases make it differ from `fetch_name`.
    pub name: String,
    /// Registry package to ask for (same unless alias).
    pub fetch_name: String,
    pub scope: Option<String>,
    pub spec_type: SpecType,
    /// Range / tag / workspace range / tarball url-or-`file:` path.
    pub fetch_spec: String,
    /// Registry path form: `@scope/foo` -> `@scope%2ffoo`.
    pub escaped_name: String,
}

const BLOCKED: &[&str] = &["node_modules", "favicon.ico"];
const ALIAS: &str = "npm:";
const WORKSPACE: &str = "workspace:";

fn is_url(s: &str) -> bool {
    s.len() > 8 && (s.starts_with("http://") || s.starts_with("https://"))
}

fn is_tarball_name(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.ends_with(".tgz") || l.ends_with(".tar.gz") || l.ends_with(".tar")
}

/// A bare CLI arg that is a tarball on its own (no `name@`), as its
/// `fetch_spec`; `None` otherwise. Only a caller that reads the tarball
/// can turn it into a full [`Spec`].
pub fn bare_tarball(arg: &str) -> Option<String> {
    if is_url(arg) || arg.starts_with("file:") || arg.starts_with("./") || arg.starts_with("../") {
        return tarball(arg, arg, None).ok().flatten();
    }
    if !arg.contains('@') && is_tarball_name(arg) {
        return tarball(&format!("file:{arg}"), arg, None).ok().flatten();
    }
    None
}

/// Where a tarball dep's bytes are, as a lockfile key spells it.
pub fn tarball_source(fetch_spec: &str, base: &str) -> String {
    if let Some(path) = fetch_spec.strip_prefix("file:") {
        format!("file:{}", join_path(base, path))
    } else {
        fetch_spec.to_string()
    }
}

/// Parse a CLI arg like `foo@^1.2`, `@scope/foo@latest`, `foo@npm:bar@^1`.
pub fn parse_spec(arg: &str, where_: Option<&str>) -> Result<Spec, FlashnpmError> {
    let (name, spec) = split_at(arg);
    build(&name, &spec, arg, where_)
}

/// Parse an already-split `package.json` deps entry.
pub fn parse_dep(name: &str, spec: &str, where_: Option<&str>) -> Result<Spec, FlashnpmError> {
    let raw = if spec.is_empty() {
        name.to_string()
    } else {
        format!("{name}@{spec}")
    };
    build(name, spec, &raw, where_)
}

/// Split `name@spec` on the `@` that is not a scope marker.
fn split_at(arg: &str) -> (String, String) {
    let at = arg[1..].find('@').map(|i| i + 1);
    match at {
        Some(i) if i > 0 => (arg[..i].to_string(), arg[i + 1..].to_string()),
        _ => (arg.to_string(), String::new()),
    }
}

fn build(name: &str, spec: &str, raw: &str, where_: Option<&str>) -> Result<Spec, FlashnpmError> {
    check_name(name, raw, where_)?;
    let mut fetch_name = name.to_string();
    let mut s = spec.trim().to_string();
    let mut local = false;
    let source = tarball(&s, raw, where_).ok().flatten();
    if s.starts_with(WORKSPACE) {
        local = true;
        let (n, r) = workspace(name, s[WORKSPACE.len()..].trim(), raw, where_)?;
        fetch_name = n;
        s = r;
    } else if s.starts_with(ALIAS) {
        let (n, r) = split_at(&s[ALIAS.len()..]);
        check_name(&n, raw, where_)?;
        let rest = r.trim().to_string();
        if rest.starts_with(ALIAS) || rest.starts_with(WORKSPACE) {
            return Err(fail(
                format!("Invalid alias of package \"{raw}\": an alias cannot point at an alias"),
                where_,
            ));
        }
        fetch_name = n;
        s = rest;
    }

    let base = Spec {
        raw: raw.to_string(),
        name: name.to_string(),
        scope: scope_of(&fetch_name),
        escaped_name: fetch_name.replace('/', "%2f"),
        fetch_name: fetch_name.clone(),
        fetch_spec: String::new(),
        spec_type: SpecType::Range,
    };

    if let Some(source) = source {
        return Ok(Spec {
            spec_type: SpecType::Tarball,
            fetch_spec: source,
            ..base
        });
    }
    if local {
        return Ok(Spec {
            spec_type: SpecType::Workspace,
            fetch_spec: s,
            ..base
        });
    }
    if s.is_empty() || s == "*" {
        return Ok(Spec {
            spec_type: SpecType::Range,
            fetch_spec: "*".to_string(),
            ..base
        });
    }
    if valid_range(&s) {
        let t = if parse_version(&s).is_some() {
            SpecType::Version
        } else {
            SpecType::Range
        };
        return Ok(Spec {
            spec_type: t,
            fetch_spec: s,
            ..base
        });
    }
    if urlencoding_safe(&s) != s {
        return Err(fail(
            format!("Invalid tag \"{s}\" of package \"{raw}\": tags must be url-safe"),
            where_,
        ));
    }
    Ok(Spec {
        spec_type: SpecType::Tag,
        fetch_spec: s,
        ..base
    })
}

fn scope_of(fetch_name: &str) -> Option<String> {
    fetch_name
        .strip_prefix('@')
        .and_then(|rest| rest.find('/').map(|i| format!("@{}", &rest[..i])))
        .or_else(|| {
            fetch_name
                .find('/')
                .map(|i| fetch_name[..i].to_string())
                .filter(|_| fetch_name.starts_with('@'))
        })
}

fn tarball(s: &str, raw: &str, where_: Option<&str>) -> Result<Option<String>, FlashnpmError> {
    if is_url(s) {
        url::Url::parse(s)
            .map_err(|_| fail(format!("Invalid url \"{s}\" of package \"{raw}\""), where_))?;
        return Ok(Some(s.to_string()));
    }
    let path = if let Some(p) = s.strip_prefix("file:") {
        Some(p.to_string())
    } else if s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with(".\\")
        || s.starts_with("..\\")
    {
        Some(s.to_string())
    } else {
        None
    };
    let Some(path) = path else { return Ok(None) };
    let clean = path.replace('\\', "/");
    if clean.starts_with('/') || clean.starts_with('~') || is_windows_drive(&clean) {
        return Err(fail(
            format!(
                "Invalid path \"{path}\" of package \"{raw}\": give it relative to package.json"
            ),
            where_,
        ));
    }
    if !is_tarball_name(&clean) {
        return Err(fail(
            format!(
                "Invalid path \"{path}\" of package \"{raw}\": only a tarball (.tgz, .tar.gz or .tar) installs from a path"
            ),
            where_,
        ));
    }
    Ok(Some(format!("file:{}", join_path("", &clean))))
}

fn is_windows_drive(_clean: &str) -> bool {
    _clean.len() >= 2 && _clean.as_bytes()[1] == b':' && _clean.as_bytes()[0].is_ascii_alphabetic()
}

pub fn join_path(base: &str, path: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in format!("{base}/{path}").split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." && !out.is_empty() && out.last().unwrap() != ".." {
            out.pop();
        } else {
            out.push(part.to_string());
        }
    }
    out.join("/")
}

fn workspace(
    name: &str,
    rest: &str,
    raw: &str,
    where_: Option<&str>,
) -> Result<(String, String), FlashnpmError> {
    let mut fetch_name = name.to_string();
    let mut s = rest.trim().to_string();
    if s[1..].find('@').map(|i| i + 1).unwrap_or(0) > 0 {
        let (n, r) = split_at(&s);
        check_name(&n, raw, where_)?;
        fetch_name = n;
        s = r.trim().to_string();
    }
    if s.is_empty() || s == "^" || s == "~" {
        s = "*".to_string();
    }
    if !valid_range(&s) {
        let why = if s.starts_with('.') || s.starts_with('/') {
            "a workspace is named, not given by path"
        } else {
            "not a range"
        };
        return Err(fail(
            format!("Invalid workspace spec \"{rest}\" of package \"{raw}\": {why}"),
            where_,
        ));
    }
    Ok((fetch_name, s))
}

fn check_name(name: &str, raw: &str, where_: Option<&str>) -> Result<(), FlashnpmError> {
    let bad = |why: &str| {
        fail(
            format!("Invalid package name \"{name}\" of package \"{raw}\": {why}"),
            where_,
        )
    };
    if name.is_empty() {
        return Err(bad("name is empty"));
    }
    if name.starts_with('.') || name.starts_with('_') {
        return Err(bad("name starts with . or _"));
    }
    if name.starts_with('-') {
        return Err(bad("name starts with a hyphen"));
    }
    if BLOCKED.contains(&name) {
        return Err(bad("name is reserved"));
    }
    // `(@scope/)?name`
    let (scope, pkg) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((s, p)) => (Some(s), p),
            None => return Err(bad("name is malformed")),
        },
        None => (None, name),
    };
    if pkg.is_empty() || pkg.contains('/') {
        return Err(bad("name is malformed"));
    }
    if let Some(scope) = scope {
        if scope.is_empty() || urlencoding_safe(scope) != scope {
            return Err(bad("scope has url-unsafe characters"));
        }
    }
    if urlencoding_safe(pkg) != pkg {
        return Err(bad("name has url-unsafe characters"));
    }
    if pkg == "." || pkg == ".." {
        return Err(bad("name is a path segment"));
    }
    Ok(())
}

fn urlencoding_safe(s: &str) -> String {
    // `encodeURIComponent` subset npm tags require: keep unreserved + a few marks.
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'\'' | b'(' | b')' | b'*'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn fail(message: String, where_: Option<&str>) -> FlashnpmError {
    let msg = match where_ {
        Some(w) => format!("{message} (at {w})"),
        None => message,
    };
    // Bad specs must stay distinguishable from network failures (optional deps).
    FlashnpmError::new(ErrorCode::Einvalidspec, msg)
}

#[allow(non_snake_case)]
pub fn flashnpm_err_code() -> ErrorCode {
    ErrorCode::Einvalidspec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_ranges_tags() {
        assert_eq!(parse_spec("vue@^3", None).unwrap().fetch_spec, "^3");
        assert_eq!(
            parse_spec("vue@^3", None).unwrap().spec_type,
            SpecType::Range
        );
        assert_eq!(
            parse_spec("vue@1.2.3", None).unwrap().spec_type,
            SpecType::Version
        );
        assert_eq!(
            parse_spec("vue@latest", None).unwrap().spec_type,
            SpecType::Tag
        );
        assert_eq!(parse_spec("vue", None).unwrap().fetch_spec, "*");
    }

    #[test]
    fn alias_and_tarball() {
        let s = parse_spec("sw@npm:string-width@^4", None).unwrap();
        assert_eq!(s.name, "sw");
        assert_eq!(s.fetch_name, "string-width");
        // bare URLs are tarball sources via `bare_tarball`, not `parse_spec`
        let bare = bare_tarball("https://example.com/lib-1.0.0.tgz").unwrap();
        assert!(bare.starts_with("https://"));
        let t = parse_spec("other@https://example.com/lib-1.0.0.tgz", None).unwrap();
        assert_eq!(t.spec_type, SpecType::Tarball);
    }

    #[test]
    fn rejects_bad_names() {
        assert!(parse_spec("@bad name@^1", None).is_err());
        assert!(parse_spec("_priv@^1", None).is_err());
    }
}
