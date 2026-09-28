//! Minimal npm semver: parse, compare and range matching.
//!
//! Port of `upm`'s `src/semver.ts`. Supports `^ ~ >= <= > < =`,
//! hyphen ranges, `||`, `x/*` wildcards and prerelease gating.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<PrereleaseId>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereleaseId {
    Num(u64),
    Str(String),
}

fn parse_ids(s: &str) -> Vec<PrereleaseId> {
    s.split('.')
        .map(|p| {
            if p.chars().all(|c| c.is_ascii_digit()) {
                // npm treats all-digit ids as numbers (leading zeros are still ids,
                // but comparison stays numeric-vs-string as in upm).
                p.parse::<u64>()
                    .map_or_else(|_| PrereleaseId::Str(p.to_string()), PrereleaseId::Num)
            } else {
                PrereleaseId::Str(p.to_string())
            }
        })
        .collect()
}

/// Parse an exact `1.2.3[-pre][+build]` version. Build metadata is dropped
/// from `version`, matching npm. Returns `None` when invalid.
pub fn parse(v: &str) -> Option<Version> {
    let s = v
        .trim()
        .trim_start_matches(|c| c == ' ' || c == '=' || c == 'v');
    // strip build metadata
    let (core, _) = match s.split_once('+') {
        Some((a, _)) => (a, true),
        None => (s, false),
    };
    let (core, pre) = match core.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (core, None),
    };
    // pre must be dot-separated identifiers
    if let Some(pre) = pre {
        if pre.is_empty()
            || !pre.split('.').all(|id| {
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        {
            return None;
        }
    }
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // reject leading zeros like npm ("01.2.3" invalid)
    for part in [major, minor, patch] {
        let _ = part;
    }
    if has_leading_zero(core) {
        return None;
    }
    let prerelease = pre.map_or_else(Vec::new, parse_ids);
    let version = format!(
        "{major}.{minor}.{patch}{}",
        pre.map_or_else(String::new, |p| format!("-{p}"))
    );
    Some(Version {
        major,
        minor,
        patch,
        prerelease,
        version,
    })
}

fn has_leading_zero(core: &str) -> bool {
    core.split('.')
        .any(|p| p.len() > 1 && p.starts_with('0') && p.chars().all(|c| c.is_ascii_digit()))
}

fn cmp_num(a: u64, b: u64) -> Ordering {
    a.cmp(&b)
}

fn cmp_pre(a: &[PrereleaseId], b: &[PrereleaseId]) -> Ordering {
    if a.is_empty() || b.is_empty() {
        if a.len() == b.len() {
            return Ordering::Equal;
        }
        // no-prerelease > prerelease
        return if a.is_empty() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    let mut i = 0;
    loop {
        match (a.get(i), b.get(i)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x != y {
                    return match (x, y) {
                        (PrereleaseId::Num(nx), PrereleaseId::Num(ny)) => nx.cmp(ny),
                        (PrereleaseId::Num(_), PrereleaseId::Str(_)) => Ordering::Less,
                        (PrereleaseId::Str(_), PrereleaseId::Num(_)) => Ordering::Greater,
                        (PrereleaseId::Str(sx), PrereleaseId::Str(sy)) => sx.cmp(sy),
                    };
                }
            }
        }
        i += 1;
    }
}

pub fn compare(a: &Version, b: &Version) -> Ordering {
    cmp_num(a.major, b.major)
        .then(cmp_num(a.minor, b.minor))
        .then(cmp_num(a.patch, b.patch))
        .then(cmp_pre(&a.prerelease, &b.prerelease))
}

// ---- ranges ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
}

#[derive(Debug, Clone)]
struct Comparator {
    op: Op,
    v: Version,
}

#[derive(Debug, Clone)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Option<Vec<PrereleaseId>>,
    has_pre: bool,
}

fn mk(major: u64, minor: u64, patch: u64, pre: Vec<PrereleaseId>) -> Version {
    let version = if pre.is_empty() {
        format!("{major}.{minor}.{patch}")
    } else {
        let s = pre
            .iter()
            .map(|id| match id {
                PrereleaseId::Num(n) => n.to_string(),
                PrereleaseId::Str(s) => s.clone(),
            })
            .collect::<Vec<_>>()
            .join(".");
        format!("{major}.{minor}.{patch}-{s}")
    };
    Version {
        major,
        minor,
        patch,
        prerelease: pre,
        version,
    }
}

fn ge(major: u64, minor: u64, patch: u64, pre: Vec<PrereleaseId>) -> Comparator {
    Comparator {
        op: Op::Gte,
        v: mk(major, minor, patch, pre),
    }
}

fn lt(major: u64, minor: u64, patch: u64) -> Comparator {
    Comparator {
        op: Op::Lt,
        v: mk(major, minor, patch, vec![PrereleaseId::Num(0)]),
    }
}

fn is_wild(s: &str) -> bool {
    s == "x" || s == "X" || s == "*"
}

fn parse_partial(s: &str) -> Option<Partial> {
    let t = s.trim().trim_start_matches(['=', 'v']);
    if t.is_empty() || t == "*" {
        return Some(Partial {
            major: None,
            minor: None,
            patch: None,
            pre: None,
            has_pre: false,
        });
    }
    // split off prerelease (only valid with full x.y.z)
    let (core, pre) = match t.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    // build metadata ignored
    let core = core.split('+').next().unwrap_or(core);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut nums: Vec<Option<u64>> = Vec::new();
    for p in &parts {
        if p.is_empty() {
            return None;
        }
        if is_wild(p) {
            nums.push(None);
        } else if p.chars().all(|c| c.is_ascii_digit()) {
            if p.len() > 1 && p.starts_with('0') {
                return None;
            }
            nums.push(Some(p.parse::<u64>().ok()?));
        } else {
            return None;
        }
    }
    let major = nums.first().copied().flatten();
    // missing trailing parts stay absent (wildcard-ish) — handled by caller
    let minor = if nums.len() > 1 { nums[1] } else { None };
    let patch = if nums.len() > 2 { nums[2] } else { None };
    // A concrete part may not follow a wildcard.
    if major.is_none() && (minor.is_some() || patch.is_some()) {
        return None;
    }
    if minor.is_none() && patch.is_some() {
        return None;
    }
    // prerelease needs all three parts
    if pre.is_some() && patch.is_none() {
        return None;
    }
    if let Some(pre) = pre {
        if pre.is_empty() {
            return None;
        }
        let pre_core = pre.split('+').next().unwrap_or(pre);
        if !pre_core
            .split('.')
            .all(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return None;
        }
        return Some(Partial {
            major,
            minor,
            patch,
            pre: Some(parse_ids(pre_core)),
            has_pre: true,
        });
    }
    Some(Partial {
        major,
        minor,
        patch,
        pre: None,
        has_pre: false,
    })
}

fn expand(op: &str, q: Partial, inc_pr: bool) -> Vec<Comparator> {
    let pre = if q.has_pre {
        q.pre.clone().unwrap_or_default()
    } else if inc_pr && (q.minor.is_none() || q.patch.is_none()) {
        vec![PrereleaseId::Num(0)]
    } else {
        Vec::new()
    };
    let (Some(major), minor, patch) = (q.major, q.minor, q.patch) else {
        // `*`
        return if op == ">" || op == "<" {
            vec![lt(0, 0, 0)]
        } else {
            vec![]
        };
    };
    if op == "^" || op == "~" {
        let low = ge(major, minor.unwrap_or(0), patch.unwrap_or(0), pre);
        if minor.is_none() {
            return vec![low, lt(major + 1, 0, 0)];
        }
        let minor = minor.unwrap();
        if op == "~" {
            return vec![low, lt(major, minor + 1, 0)];
        }
        if major != 0 {
            return vec![low, lt(major + 1, 0, 0)];
        }
        return if minor == 0 && patch.is_some() {
            vec![low, lt(0, 0, patch.unwrap() + 1)]
        } else {
            vec![low, lt(0, minor + 1, 0)]
        };
    }
    if minor.is_none() || patch.is_none() {
        if op.is_empty() || op == "=" {
            return if minor.is_none() {
                vec![ge(major, 0, 0, pre), lt(major + 1, 0, 0)]
            } else {
                vec![
                    ge(major, minor.unwrap(), 0, pre),
                    lt(major, minor.unwrap() + 1, 0),
                ]
            };
        }
        let mut mj = major;
        let mut mn = minor.unwrap_or(0);
        let mut o = op;
        if op == ">" || op == "<=" {
            o = if op == ">" { ">=" } else { "<" };
            if minor.is_none() {
                mj += 1;
            } else {
                mn += 1;
            }
        }
        let op_enum = match o {
            "<" => Op::Lt,
            "<=" => Op::Lte,
            ">" => Op::Gt,
            ">=" => Op::Gte,
            _ => Op::Eq,
        };
        let pre = if o == "<" {
            vec![PrereleaseId::Num(0)]
        } else {
            pre
        };
        return vec![Comparator {
            op: op_enum,
            v: mk(mj, mn, 0, pre),
        }];
    }
    let op_enum = match op {
        "<" => Op::Lt,
        "<=" => Op::Lte,
        ">" => Op::Gt,
        ">=" => Op::Gte,
        _ => Op::Eq,
    };
    vec![Comparator {
        op: op_enum,
        v: mk(
            major,
            minor.unwrap(),
            patch.unwrap(),
            q.pre.unwrap_or_default(),
        ),
    }]
}

fn hyphen(a: Partial, b: Partial, inc_pr: bool) -> Vec<Comparator> {
    let mut out = Vec::new();
    let low = if a.has_pre {
        a.pre.clone().unwrap_or_default()
    } else if inc_pr {
        vec![PrereleaseId::Num(0)]
    } else {
        Vec::new()
    };
    if let Some(ma) = a.major {
        out.push(ge(ma, a.minor.unwrap_or(0), a.patch.unwrap_or(0), low));
    }
    if let Some(mb) = b.major {
        if b.minor.is_none() {
            out.push(lt(mb + 1, 0, 0));
        } else if b.patch.is_none() {
            out.push(lt(mb, b.minor.unwrap() + 1, 0));
        } else if !b.has_pre && inc_pr {
            out.push(lt(mb, b.minor.unwrap(), b.patch.unwrap() + 1));
        } else {
            out.push(Comparator {
                op: Op::Lte,
                v: mk(
                    mb,
                    b.minor.unwrap(),
                    b.patch.unwrap(),
                    b.pre.unwrap_or_default(),
                ),
            });
        }
    }
    out
}

fn parse_set(branch: &str, inc_pr: bool) -> Option<Vec<Comparator>> {
    // normalize `op <space> version` -> `opversion`
    let re = regex::Regex::new(r"(~>|[<>]=?|[~^]|=)\s+").unwrap();
    let normalized = re.replace_all(branch.trim(), "$1");
    let tokens: Vec<&str> = normalized
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .collect();
    let mut set = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens.get(i + 1) == Some(&"-") {
            let a = parse_partial(tokens[i])?;
            let b = parse_partial(tokens.get(i + 2).copied().unwrap_or(""))?;
            set.extend(hyphen(a, b, inc_pr));
            i += 3;
            continue;
        }
        let tok = tokens[i];
        let (op, rest) = split_op(tok);
        let q = parse_partial(rest)?;
        set.extend(expand(op, q, inc_pr));
        i += 1;
    }
    Some(set)
}

fn split_op(tok: &str) -> (&str, &str) {
    for op in ["~>", ">=", "<=", "<", ">", "~", "^", "="] {
        if let Some(rest) = tok.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", tok)
}

type RangeSets = Vec<Vec<Comparator>>;

fn cache() -> &'static Mutex<HashMap<String, Option<RangeSets>>> {
    static C: OnceLock<Mutex<HashMap<String, Option<RangeSets>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_range(range: &str, inc_pr: bool) -> Option<RangeSets> {
    let key = format!("{}{}", if inc_pr { 'p' } else { '-' }, range);
    if let Some(v) = cache().lock().unwrap().get(&key) {
        return v.clone();
    }
    let mut sets: Option<RangeSets> = Some(Vec::new());
    for branch in range.split("||") {
        // empty branch (e.g. "") means "*"
        let set = parse_set(branch, inc_pr);
        match (&mut sets, set) {
            (Some(s), Some(set)) => s.push(set),
            (s, _) => {
                *s = None;
                break;
            }
        }
    }
    cache().lock().unwrap().insert(key, sets.clone());
    sets
}

fn holds(ord: Ordering, op: Op) -> bool {
    match ord {
        Ordering::Equal => op != Op::Lt && op != Op::Gt,
        Ordering::Less => op == Op::Lt || op == Op::Lte,
        Ordering::Greater => op == Op::Gt || op == Op::Gte,
    }
}

fn test_set(v: &Version, set: &[Comparator], include_pre: bool) -> bool {
    for c in set {
        if !holds(compare(v, &c.v), c.op) {
            return false;
        }
    }
    if !v.prerelease.is_empty() && !include_pre {
        return set.iter().any(|c| {
            !c.v.prerelease.is_empty()
                && c.v.major == v.major
                && c.v.minor == v.minor
                && c.v.patch == v.patch
        });
    }
    true
}

/// `true` when `range` parses as an npm range (including `*`).
pub fn valid_range(range: &str) -> bool {
    parse_range(range, false).is_some()
}

pub fn satisfies(version: &Version, range: &str, include_prerelease: bool) -> bool {
    parse_range(range, include_prerelease).map_or(false, |sets| {
        sets.iter()
            .any(|s| test_set(version, s, include_prerelease))
    })
}

pub fn satisfies_str(version: &str, range: &str, include_prerelease: bool) -> bool {
    parse(version).map_or(false, |v| satisfies(&v, range, include_prerelease))
}

/// Highest version in `versions` satisfying `range`, or `None`.
pub fn max_satisfying<'a>(
    versions: impl IntoIterator<Item = &'a str>,
    range: &str,
    include_prerelease: bool,
) -> Option<String> {
    let sets = parse_range(range, include_prerelease)?;
    let mut best: Option<Version> = None;
    let mut raw: Option<String> = None;
    for candidate in versions {
        let Some(v) = parse(candidate) else { continue };
        if !sets.iter().any(|s| test_set(&v, s, include_prerelease)) {
            continue;
        }
        if best
            .as_ref()
            .map_or(true, |b| compare(&v, b) == Ordering::Greater)
        {
            raw = Some(candidate.to_string());
            best = Some(v);
        }
    }
    raw
}

/// Parseable versions sorted ascending; unparseable dropped.
pub fn sort_versions(versions: &[String]) -> Vec<String> {
    let mut vs: Vec<Version> = versions.iter().filter_map(|v| parse(v)).collect();
    vs.sort_by(compare);
    vs.into_iter().map(|v| v.version).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic() {
        assert_eq!(parse("1.2.3").unwrap().version, "1.2.3");
        assert!(parse("1.2").is_none());
        assert!(parse("foo").is_none());
    }

    #[test]
    fn caret_zero() {
        assert!(satisfies_str("0.2.3", "^0.2.0", false));
        assert!(!satisfies_str("0.3.0", "^0.2.0", false));
        assert!(satisfies_str("1.5.0", "^1.2.0", false));
    }

    #[test]
    fn tilde_star_hyphen_or() {
        assert!(satisfies_str("1.2.9", "~1.2.3", false));
        assert!(!satisfies_str("1.3.0", "~1.2.3", false));
        assert!(satisfies_str("2.0.0", "*", false));
        assert!(satisfies_str("1.2.5", "1.2.3 - 1.2.7", false));
        assert!(satisfies_str("2.0.0", "^1.0.0 || ^2.0.0", false));
    }

    #[test]
    fn max_pick() {
        let vs = ["1.0.0", "1.2.0", "2.0.0"];
        assert_eq!(
            max_satisfying(vs, "^1.0.0", false).as_deref(),
            Some("1.2.0")
        );
    }

    #[test]
    fn prerelease_gating() {
        // same tuple opts in: beta satisfies ^alpha without the flag
        assert!(satisfies_str("1.0.0-beta.1", "^1.0.0-alpha", false));
        // different tuple stays out without the flag
        assert!(!satisfies_str("2.0.0-beta.1", "^1.0.0-alpha", false));
        assert!(satisfies_str("1.0.0-beta.1", "^1.0.0-alpha", true));
    }
}
