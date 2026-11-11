//! The version Loom's binaries report.

/// The package version, followed by the commit and its date when built from
/// a git checkout: `0.1.0 (1d60fb3a2 2026-11-11)`. A build from a source tree
/// without git history reports the package version alone.
pub const VERSION: &str = env!("LOOM_VERSION");

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    // Also checks that the crate is rebuilt when HEAD moves: run after a
    // commit, the version must name the new HEAD.
    #[test]
    fn names_the_checked_out_commit() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let in_checkout = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| {
            std::fs::canonicalize(top).ok() == std::fs::canonicalize(&workspace).ok()
        });
        let commit = git(&["log", "-1", "--abbrev=9", "--format=%h %cs"]);
        let expected = match (in_checkout, commit) {
            (true, Some(commit)) => format!("{} ({commit})", env!("CARGO_PKG_VERSION")),
            _ => env!("CARGO_PKG_VERSION").to_owned(),
        };
        assert_eq!(VERSION, expected);
    }
}
