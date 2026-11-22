//! Embeds the git commit of the build as `LYNX_BUILD_COMMIT` (short hash, or empty).
//!
//! Taken from the `LYNX_COMMIT` env var when set (the Docker build passes it as a build arg, since
//! the image build context has no `.git`), otherwise from `git` when it is available.

use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LYNX_COMMIT");
    let commit = std::env::var("LYNX_COMMIT")
        .ok()
        .and_then(|c| clean(&c))
        .or_else(git_commit)
        .unwrap_or_default();
    println!("cargo:rustc-env=LYNX_BUILD_COMMIT={commit}");
}

/// Short form of a hash; anything that is not a hex hash is ignored.
fn clean(raw: &str) -> Option<String> {
    let c = raw.trim();
    (c.len() >= 7 && c.chars().all(|ch| ch.is_ascii_hexdigit())).then(|| c[..7].to_ascii_lowercase())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn git_commit() -> Option<String> {
    let commit = clean(&git(&["rev-parse", "HEAD"])?)?;
    // Rebuild when HEAD moves. Only existing files: a missing one would rerun this every build.
    let mut watch = vec![git(&["rev-parse", "--git-path", "HEAD"])];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(git(&["rev-parse", "--git-path", &head_ref]));
        watch.push(git(&["rev-parse", "--git-path", "packed-refs"]));
    }
    for path in watch.into_iter().flatten() {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    Some(commit)
}
