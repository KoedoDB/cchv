//! Build information for `--version`: what this binary was built from.
//!
//! The version is, in this order: the environment (`CLOUDHV_VERSION`, `CLOUDHV_COMMIT`), the file `BUILD-INFO` at the top of the
//! checkout (lines `version=` and `commit=`; the release workflow writes it, because the build containers have no `.git`; no leading dot, as `cp -a /host/*` in the build scripts skips dot files), `git
//! describe`, and at last the crate's version with `-unknown`. The pinned versions of the dependencies and of the programs the
//! release bundles are read from `Cargo.lock` and `hacks/versions.env`, so they cannot differ from what is built.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..");

    println!("cargo:rerun-if-env-changed=CLOUDHV_VERSION");
    println!("cargo:rerun-if-env-changed=CLOUDHV_COMMIT");
    // Only files that exist: a path that does not exist makes Cargo run this script on every build.
    for p in [
        "BUILD-INFO",
        "Cargo.lock",
        "hacks/versions.env",
        ".git/HEAD",
    ] {
        if root.join(p).exists() {
            println!("cargo:rerun-if-changed=../../{p}");
        }
    }
    if let Ok(head) = std::fs::read_to_string(root.join(".git/HEAD")) {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            if root.join(".git").join(r).exists() {
                println!("cargo:rerun-if-changed=../../.git/{r}");
            }
        }
    }

    let file = std::fs::read_to_string(root.join("BUILD-INFO")).unwrap_or_default();
    let from_file = |key: &str| {
        file.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let from_env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());

    let version = from_env("CLOUDHV_VERSION")
        .or_else(|| from_file("version"))
        .or_else(|| git(&root, &["describe", "--tags", "--always", "--dirty"]))
        .unwrap_or_else(|| format!("{}-unknown", std::env::var("CARGO_PKG_VERSION").unwrap()));
    let commit = from_env("CLOUDHV_COMMIT")
        .or_else(|| from_file("commit"))
        .or_else(|| git(&root, &["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());

    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap_or_default();
    let shimkit = lock_entry(&lock, "containerd-shimkit")
        .map(|(v, src)| match src.rsplit_once('#') {
            Some((_, c)) if c.len() >= 7 => format!("{v} ({})", &c[..7]),
            _ => v,
        })
        .unwrap_or_else(|| "unknown".to_string());
    let shim = lock_entry(&lock, "containerd-shim")
        .map(|(v, _)| v)
        .unwrap_or_else(|| "unknown".to_string());

    let env = std::fs::read_to_string(root.join("hacks/versions.env")).unwrap_or_default();
    let pinned = |name: &str| versions_env(&env, name).unwrap_or_else(|| "unknown".to_string());

    for (k, v) in [
        ("CLOUDHV_BUILD_VERSION", version),
        ("CLOUDHV_BUILD_COMMIT", commit),
        ("CLOUDHV_SHIMKIT_VERSION", shimkit),
        ("CLOUDHV_CONTAINERD_SHIM_VERSION", shim),
        ("CLOUDHV_CH_VERSION", pinned("CH_VERSION")),
        ("CLOUDHV_CRUN_VERSION", pinned("CRUN_VERSION")),
        ("CLOUDHV_KERNEL_VERSION", pinned("KERNEL_VERSION")),
    ] {
        println!("cargo:rustc-env={k}={v}");
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The `version` and `source` of the package `name` in the text of a Cargo.lock.
fn lock_entry(lock: &str, name: &str) -> Option<(String, String)> {
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == format!("name = \"{name}\"") {
            let version = lines
                .next()?
                .trim()
                .strip_prefix("version = \"")?
                .trim_end_matches('"')
                .to_string();
            let source = lines
                .next()
                .and_then(|s| s.trim().strip_prefix("source = \""))
                .map(|s| s.trim_end_matches('"').to_string())
                .unwrap_or_default();
            return Some((version, source));
        }
    }
    None
}

/// The default of `: "${NAME:=value}"` in hacks/versions.env.
fn versions_env(env: &str, name: &str) -> Option<String> {
    let key = format!("${{{name}:=");
    let line = env.lines().find(|l| l.contains(&key))?;
    let rest = &line[line.find(&key)? + key.len()..];
    Some(rest[..rest.find('}')?].to_string())
}
