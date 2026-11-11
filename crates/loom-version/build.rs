//! Sets `LOOM_VERSION` to the package version, followed by the commit and its
//! date when the crate is built from Loom's own git checkout, as in
//! `0.1.0 (1d60fb3a2 2026-11-11)`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The commit and date, if the build runs inside the checkout this crate
/// belongs to. A source tree unpacked inside some other repository must not
/// pick up that repository's commit.
fn commit(manifest: &Path) -> Option<String> {
    let workspace = manifest.join("../..");
    let toplevel = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    if toplevel.canonicalize().ok()? != workspace.canonicalize().ok()? {
        return None;
    }
    // Rebuild when HEAD moves. HEAD changes on a checkout or a detached
    // commit. A commit on a branch rewrites its ref under refs/heads, or
    // creates it there when the refs were packed, so the whole directory is
    // watched (cargo scans directories recursively). packed-refs changes on
    // `git gc` and `git pack-refs`.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    git(&["log", "-1", "--abbrev=9", "--format=%h %cs"])
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    println!("cargo:rerun-if-changed=build.rs");
    let full = match commit(&manifest) {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    println!("cargo:rustc-env=LOOM_VERSION={full}");
}
