use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loom_config::HarmfulConfig;
use loom_tools::env::CommandEnv;
use loom_tools::harmful::CommandAnalyzer;
use loom_tools::{ToolContext, Workspace, canonical_dir, patch_paths};
use serde_json::Value;

pub const POSSIBLY_HARMFUL: &str = "this tool call may be harmful";

fn analyzer(
    config: &HarmfulConfig,
    allowed: &[PathBuf],
    root: &Path,
    env: &CommandEnv,
) -> CommandAnalyzer {
    let vars: HashMap<String, String> = env
        .vars
        .iter()
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value.to_str()?.to_owned())))
        .collect();
    let home = vars
        .get("HOME")
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let allowed: Vec<String> = allowed
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    CommandAnalyzer::new(root, home, &allowed, &config.deny_paths, vars)
}

/// Whether a call may harm data outside the workspace, the root `.env`
/// files, the `.git` directory, or the git history; `Err` gives the reason.
/// Changes in `writable`, the sandbox's writable paths, are not harmful.
/// Changes by the file tools in `allowed_dirs` are not harmful either.
pub(crate) fn check(
    config: &HarmfulConfig,
    writable: &[PathBuf],
    allowed_dirs: &[PathBuf],
    workspace: Arc<Workspace>,
    env: &CommandEnv,
    tool: &str,
    args: &Value,
) -> Result<(), String> {
    let root = workspace.root().to_path_buf();
    let text = |name: &str| args.get(name).and_then(Value::as_str);
    if tool == "shell" {
        return match text("command") {
            Some(command) => analyzer(config, writable, &root, env).analyze(command),
            None => Ok(()),
        };
    }
    if tool != "write" && tool != "edit" {
        return Ok(());
    }
    let allowed: Vec<PathBuf> = allowed_dirs.iter().map(|dir| canonical_dir(dir)).collect();
    let ctx = ToolContext::new(workspace).with_allowed(Arc::new(allowed.clone()));
    let paths: Vec<PathBuf> = if tool == "write" {
        text("path")
            .and_then(|path| ctx.resolve(path).ok())
            .into_iter()
            .collect()
    } else {
        text("patch")
            .and_then(|patch| patch_paths(&ctx, patch).ok())
            .unwrap_or_default()
    };
    if paths.is_empty() {
        return Ok(());
    }
    let allowed: Vec<PathBuf> = writable.iter().cloned().chain(allowed).collect();
    let analyzer = analyzer(config, &allowed, &root, env);
    paths
        .iter()
        .try_for_each(|path| analyzer.validate_path(&path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn checks_commands_and_file_paths() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        let workspace = Arc::new(Workspace::new(dir.path().join("ws")).unwrap());
        let outside = dir.path().canonicalize().unwrap();
        let cache = outside.join("cache");
        let notes = outside.join("notes");
        let config = HarmfulConfig::default();
        let env = CommandEnv::default();
        let run = |writable: &[PathBuf], allowed: &[PathBuf], tool: &str, args: Value| {
            check(
                &config,
                writable,
                allowed,
                workspace.clone(),
                &env,
                tool,
                &args,
            )
        };
        let check = |tool: &str, args: Value| run(&[], &[], tool, args);
        assert!(check("shell", json!({"command": "git reset --hard"})).is_err());
        assert!(check("shell", json!({"command": "ls"})).is_ok());
        assert!(check("write", json!({"path": ".git/config", "content": ""})).is_err());
        assert!(check("write", json!({"path": "src/a.rs", "content": ""})).is_ok());
        let patch = "--- a/a.txt\n+++ b/.env.local\n@@\n-x\n+y\n";
        assert!(check("edit", json!({ "patch": patch })).is_err());
        assert!(check("read", json!({"path": ".git/config"})).is_ok());

        let command = json!({"command": format!("rm -rf {}/old", cache.display())});
        assert!(check("shell", command.clone()).is_err());
        assert!(run(&[cache], &[], "shell", command).is_ok());

        let note = notes.join("a.md").display().to_string();
        let allowed = [notes];
        let write = json!({"path": note, "content": "x"});
        assert!(run(&[], &allowed, "write", write).is_ok());
        let patch = format!("--- /dev/null\n+++ {note}\n@@\n+x\n");
        assert!(run(&[], &allowed, "edit", json!({ "patch": patch })).is_ok());
        let command = json!({"command": format!("rm {note}")});
        assert!(run(&[], &allowed, "shell", command).is_err());
    }
}
