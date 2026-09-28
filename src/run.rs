//! `flashnpm run <script>`: one package.json script in a shell, with every
//! `node_modules/.bin` above the project first on `PATH`.
//! Port of `upm`'s `src/run.ts`. No pre/post scripts: what runs is named.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{FlashnpmError, ErrorCode};

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Emanifet, msg)
}

/// The `scripts` map, or a clear error for one that is not a map of strings.
pub fn read_scripts(
    manifest: &serde_json::Value,
    file: &str,
) -> Result<BTreeMap<String, String>, FlashnpmError> {
    let Some(scripts) = manifest.get("scripts") else {
        return Ok(BTreeMap::new());
    };
    if scripts.is_null() {
        return Ok(BTreeMap::new());
    }
    let obj = scripts
        .as_object()
        .ok_or_else(|| fail(format!("{file}: scripts is not a map of commands")))?;
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        let cmd = v
            .as_str()
            .ok_or_else(|| fail(format!("{file}: scripts is not a map of commands")))?;
        out.insert(k.clone(), cmd.to_string());
    }
    Ok(out)
}

/// `node_modules/.bin` of `dir` and of each directory above it, nearest first.
pub fn bin_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut at = dir.to_path_buf();
    loop {
        out.push(at.join("node_modules/.bin"));
        match at.parent() {
            Some(p) => {
                if p == at {
                    break;
                }
                at = p.to_path_buf();
            }
            None => break,
        }
    }
    out
}

/// Script environment: parent env + `PATH` + the `npm_*` names tools read.
pub fn script_env(
    dir: &Path,
    file: &Path,
    name: &str,
    command: &str,
    pkg_name: Option<&str>,
    pkg_version: Option<&str>,
) -> Vec<(String, String)> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    with_path(&bin_dirs(dir), env, |mut vars| {
        vars.push((
            "INIT_CWD".to_string(),
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        ));
        vars.push(("npm_lifecycle_event".to_string(), name.to_string()));
        vars.push(("npm_lifecycle_script".to_string(), command.to_string()));
        vars.push((
            "npm_package_json".to_string(),
            file.to_string_lossy().to_string(),
        ));
        vars.push((
            "npm_package_name".to_string(),
            pkg_name.unwrap_or("").to_string(),
        ));
        vars.push((
            "npm_package_version".to_string(),
            pkg_version.unwrap_or("").to_string(),
        ));
        vars
    })
}

/// Parent env with `dirs`, then the running node's directory, first on `PATH`.
pub fn with_path(
    dirs: &[PathBuf],
    env: Vec<(String, String)>,
    f: impl FnOnce(Vec<(String, String)>) -> Vec<(String, String)>,
) -> Vec<(String, String)> {
    let key = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "PATH".to_string());
    let old = env
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let mut parts: Vec<String> = dirs
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect();
    if !old.is_empty() {
        parts.push(old);
    }
    let joined = parts.join(path_delimiter());
    let mut out: Vec<(String, String)> = env.into_iter().filter(|(k, _)| *k != key).collect();
    out.push((key, joined));
    f(out)
}

#[cfg(unix)]
fn path_delimiter() -> &'static str {
    ":"
}

#[cfg(windows)]
fn path_delimiter() -> &'static str {
    ";"
}

/// Quote one argv word for `sh`.
pub fn quote_sh(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '/' | ':' | '=' | '@' | '+' | ',' | '-' | '_')
        })
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The command with args appended, quoted for the shell.
pub fn shell_line(command: &str, args: &[String]) -> String {
    let mut line = command.to_string();
    for arg in args {
        line.push(' ');
        line.push_str(&quote_sh(arg));
    }
    line
}

/// Run a shell line to completion, sharing stdio. Resolves to its exit code.
pub async fn run_shell(line: &str, cwd: &Path, env: &[(String, String)]) -> std::io::Result<i32> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let comspec = env
            .iter()
            .find(|(k, _)| k == "ComSpec")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "cmd.exe".to_string());
        let mut c = tokio::process::Command::new(comspec);
        c.arg("/d").arg("/s").arg("/c").arg(format!("\"{line}\""));
        c
    };
    cmd.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v)));
    let mut child = cmd.spawn()?;
    let status = child.wait().await?;
    Ok(status.code().unwrap_or(128))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        assert_eq!(quote_sh("build"), "build");
        assert_eq!(quote_sh("a b"), "'a b'");
    }

    #[test]
    fn bins_walk_up() {
        let dirs = bin_dirs(Path::new("/a/b"));
        assert_eq!(dirs[0], PathBuf::from("/a/b/node_modules/.bin"));
        assert_eq!(dirs[1], PathBuf::from("/a/node_modules/.bin"));
    }
}
