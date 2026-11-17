use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use loom_tools::harmful::CommandAnalyzer;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    outside: PathBuf,
    analyzer: CommandAnalyzer,
}

impl Fixture {
    fn new() -> Self {
        Self::with_paths(&[], &[])
    }

    fn with_paths(allow: &[&str], deny: &[&str]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("harmful-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let outside = root.join("outside");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("nested")).unwrap();
        std::fs::create_dir_all(outside.join("directory")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(project.join("source.txt"), "source").unwrap();
        std::fs::write(outside.join(".env"), "SECRET=value").unwrap();
        symlink(outside.join("directory"), project.join("escape")).unwrap();
        symlink(
            outside.join("not-created-yet"),
            project.join("dangling-escape"),
        )
        .unwrap();
        symlink("/tmp/loom-harmful-env-target", project.join(".env")).unwrap();
        let strings = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let analyzer = CommandAnalyzer::new(
            &project,
            root.join("home"),
            &strings(allow),
            &strings(deny),
            HashMap::new(),
        );
        Self {
            _dir: dir,
            root,
            project,
            outside,
            analyzer,
        }
    }

    fn outside(&self, path: &str) -> String {
        self.outside.join(path).display().to_string()
    }

    fn blocked(&self, command: &str) -> String {
        match self.analyzer.analyze(command) {
            Err(reason) => reason,
            Ok(()) => panic!("expected command to be blocked: {command}"),
        }
    }

    fn blocks(&self, command: &str, reason: &str) {
        let actual = self.blocked(command);
        assert!(actual.contains(reason), "{command}: {actual}");
    }

    fn allows(&self, command: &str) {
        if let Err(reason) = self.analyzer.analyze(command) {
            panic!("expected command to be allowed: {command}\n{reason}");
        }
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[test]
fn blocks_guarded_commands_that_change_paths_outside_the_workspace() {
    let f = Fixture::new();
    f.blocked(&format!("rm -rf {}", f.outside("directory")));
    f.blocked(&format!("mv ./source.txt {}", f.outside("moved.txt")));
    f.blocked(&format!("cp ./source.txt {}", f.outside("copied.txt")));
    f.blocked(&format!("chmod 600 {}", f.outside("file.txt")));
    f.blocked(&format!("tee -a {}", f.outside("output.txt")));
    f.blocked(&format!("chown user:group {}", f.outside("file.txt")));
    f.blocked(&format!("touch {}", f.outside("new.txt")));
    f.blocked(&format!("rmdir {}", f.outside("directory")));
    f.blocked(&format!("unlink {}", f.outside("file")));
    f.blocked(&format!("shred {}", f.outside("file")));
    f.blocked(&format!("chgrp users {}", f.outside("file")));
    f.blocked(&format!("truncate -s 0 {}", f.outside("file")));
    f.blocked(&format!("dd if=/dev/zero of={}", f.outside("disk.img")));
    f.blocked(&format!("ln -s ./source.txt {}", f.outside("link")));
}

#[test]
fn allows_guarded_commands_in_the_workspace_and_temporary_paths() {
    let f = Fixture::new();
    f.allows("rm -rf ./nested/cache");
    f.allows("mv ./source.txt ./nested/moved.txt");
    f.allows("cp ./source.txt ./nested/copied.txt");
    f.allows("chmod 600 ./source.txt");
    f.allows("tee -a ./nested/output.txt");
    f.allows("chown user:group ./source.txt");
    f.allows("touch ./nested/new.txt");
    f.allows("rm -rf /tmp/loom-cache");
    f.allows("touch /var/tmp/loom-cache");
    f.allows("tee /dev/null");
    f.allows("truncate -s 0 /dev/null");
}

#[test]
fn allows_only_changes_that_do_not_delete_device_paths() {
    let f = Fixture::new();
    f.allows("touch /dev/null");
    f.allows("chmod 666 /dev/null");
    f.allows("ln -sf /dev/null ./nested/nul");
    f.allows("cp ./source.txt /dev/null");
    f.allows("mv ./source.txt /dev/null");
    f.allows("dd if=/dev/zero of=/dev/null");
    f.allows("cat ./source.txt > /dev/stdout");
    f.blocked("rm /dev/null");
    f.blocked("rm -f /dev/stdout");
    f.blocked("unlink /dev/null");
    f.blocked("mv /dev/null ./nested/moved");
    f.blocks("find /dev/null -delete", "find -delete");
    f.blocks("echo /dev/null | xargs rm", "xargs");
}

#[test]
fn handles_copy_and_move_arguments() {
    let f = Fixture::new();
    f.allows(&format!("cp {} ./nested/copied.txt", f.outside("file.txt")));
    f.blocked(&format!("mv {} ./nested/moved.txt", f.outside("file.txt")));
    f.blocked(&format!("cp -v -t {} ./source.txt", display(&f.outside)));
    f.blocked(&format!("mv -vt{} ./source.txt", display(&f.outside)));
    f.allows("cp -t ./nested ./source.txt");
    f.blocks(
        &format!("cp {} .", f.outside(".env")),
        "protected path .env files",
    );
}

#[test]
fn protects_env_files_and_the_git_directory() {
    let f = Fixture::new();
    f.blocks("rm .env", "protected path .env files");
    f.allows("rm .env.example");
    f.blocks("tee .env.production", "protected path .env files");
    f.blocks("cp source.txt .env.local", "protected path .env files");
    f.blocks("chmod 600 .git/config", "protected path .git directory");
    f.blocks("touch .git/HEAD", "protected path .git directory");
    f.blocks("rm -rf .*", "protected path");
    f.allows("rm -rf *");
    f.allows("touch .gitignore");
    f.allows("touch nested/.env.local");
}

#[test]
fn blocks_destructive_git_commands() {
    let f = Fixture::new();
    f.blocks("git checkout -- source.txt", "git checkout --");
    f.blocks("git restore source.txt", "git restore");
    f.blocks("git reset --hard HEAD~1", "git reset --hard");
    f.blocks("git reset --merge", "git reset --merge");
    f.blocks("git -C /tmp push origin main --force", "git push --force");
    f.blocks(
        "git push --force-with-lease origin main",
        "git push --force",
    );
    f.blocks("sudo git clean -fd", "git clean -f");
    f.blocks("git push -uf origin main", "git push --force");
    f.blocks("git branch -D old-branch", "git branch -D");
    f.blocks("git stash drop", "git stash drop");
    f.blocks("git stash clear", "git stash clear");
    f.allows("git reset --soft HEAD~1");
    f.allows("git clean -n");
    f.allows("git push origin main");
    f.allows("git restore --staged .");
}

#[test]
fn checks_every_command_of_a_chain() {
    let f = Fixture::new();
    f.blocked(&format!("echo ok && rm {}", f.outside("a")));
    f.blocked(&format!("false || touch {}", f.outside("b")));
    f.blocked(&format!("echo ok; chmod 600 {}", f.outside("c")));
    f.blocked(&format!("printf data | tee {}", f.outside("d")));
    f.blocked("echo ok\ngit clean -f");
    f.blocked(&format!("(echo ok && rm {})", f.outside("e")));
    f.allows(&format!(
        "echo \"rm {}\" && touch ./nested/safe",
        f.outside("quoted")
    ));
    f.allows("(cd ./nested && touch safe)");
}

#[test]
fn checks_output_redirects() {
    let f = Fixture::new();
    f.blocks(
        &format!("echo data > {}", f.outside("output.txt")),
        "redirect",
    );
    f.blocks("echo secret >> .env", "protected path");
    f.allows("echo example > .env.example");
    f.allows("echo cache > /tmp/loom-output");
    f.allows("echo ignored 2>/dev/null");
    f.allows("(echo ignored > /dev/null)");
    f.allows("(cd ./nested && echo ignored >/dev/null 2>&1)");
    f.blocks(
        &format!("(echo data > {})", f.outside("output.txt")),
        "redirect",
    );
    f.allows(
        "probe() { out=$(timeout 3 curl -s \"http://$1:8000/v1/models\" 2>/dev/null); if [ -z \"$out\" ]; then echo none; else echo \"$out\" | python3 -c \"\nimport json,sys\nprint(json.load(sys.stdin))\n\"; fi; }\nprobe 198.18.128.1",
    );
}

#[test]
fn resolves_variables_assigned_earlier_in_the_chain() {
    let f = Fixture::new();
    f.allows("P=/dev; echo hello | tee \"${P}/null\"");
    f.allows("export OUT=./nested; echo hi > \"$OUT/file.txt\"");
    f.allows("OUT=./nested; cd $OUT && touch safe");
    f.allows("A=./nested B=/dev; touch \"$A/one\"; echo hi > \"$B/null\"");
    f.allows("P=/etc; P=./nested; touch \"$P/two\"");
    f.allows("P=./nested; (touch \"$P/three\")");
    f.blocks(
        "P=/etc; echo hi > \"$P/passwd\"",
        "outside the working directory",
    );
    f.blocks(
        "P=/dev; echo hi > \"$P/../etc/passwd\"",
        "outside the working directory",
    );
    f.blocks(
        &format!("P={}; rm -rf \"$P/tree\"", display(&f.outside)),
        "outside the working directory",
    );
}

#[test]
fn blocks_targets_that_cannot_be_resolved() {
    let f = Fixture::new();
    let reason = "cannot be safely resolved";
    f.blocks("echo hi > \"$LOOM_UNSET_VAR/file\"", reason);
    f.blocks("P=$(pwd); rm -rf \"$P/nested\"", reason);
    f.blocks("P=./nest*; rm -rf \"$P/file\"", reason);
    f.blocks("P=./nested; P+=/deeper; rm -rf \"$P\"", reason);
    f.blocks("echo hi | P=./nested tee \"${P}/file\"", reason);
    f.blocks("P=./nested | cat; rm -rf \"$P/file\"", reason);
    f.blocks("P=./nested & rm -rf \"$P/file\"", reason);
    f.blocks("(P=./nested); rm -rf \"$P/file\"", reason);
    f.blocks("rm \"$(printf /etc/passwd)\"", reason);
    f.blocks("rm $'/etc/passwd'", reason);
}

#[test]
fn blocks_destructive_find_xargs_and_rsync() {
    let f = Fixture::new();
    let outside = display(&f.outside);
    f.blocks(
        &format!("find {outside} -name '*.tmp' -delete"),
        "find -delete",
    );
    f.blocks(&format!("find {outside} -exec rm {{}} \\;"), "find -exec");
    f.blocks(&format!("find {outside} -print | xargs rm"), "xargs");
    f.blocks(
        &format!("rsync -a --delete ./source.txt {}", f.outside("backup")),
        "rsync --delete",
    );
    f.blocks("find . -name .env -delete", "protected path");
    f.allows("find ./nested -name '*.tmp' -delete");
}

#[test]
fn validates_file_paths() {
    let f = Fixture::new();
    assert!(f.analyzer.validate_path(&f.outside("written.txt")).is_err());
    assert!(f.analyzer.validate_path(".env").is_err());
    assert!(f.analyzer.validate_path(".git/config").is_err());
    assert!(f.analyzer.validate_path("./escape/new-file").is_err());
    assert!(f.analyzer.validate_path(".env.example").is_ok());
    assert!(f.analyzer.validate_path("./nested/file.txt").is_ok());
    assert!(f.analyzer.validate_path("/tmp/loom-file").is_ok());
}

#[test]
fn recognizes_wrappers_and_absolute_programs() {
    let f = Fixture::new();
    f.blocked(&format!("sudo -u root /bin/rm {}", f.outside("wrapped")));
    f.blocked(&format!(
        "env MODE=test command rm {}",
        f.outside("env-wrapped")
    ));
    f.blocked(&format!("exec touch {}", f.outside("exec-wrapped")));
    f.blocked(&format!("time -f %E rm {}", f.outside("time-wrapped")));
}

#[test]
fn follows_cd_and_symlinks() {
    let f = Fixture::new();
    f.blocked(&format!("cd {} && rm relative-file", display(&f.outside)));
    f.allows("cd ./nested && rm relative-file");
    f.allows(&format!(
        "cd {} | true; rm ./nested/cache",
        display(&f.outside)
    ));
    let outside = "outside the working directory";
    f.blocks("touch ./escape/new-file", outside);
    f.blocks("tee ./escape/missing/output.txt", outside);
    f.blocks("touch ./escape/../escaped-parent.txt", outside);
    f.blocks("touch ./dangling-escape", outside);
    f.blocked(&format!("rm \\\n{}", f.outside("continued")));
}

#[test]
fn skips_heredoc_bodies_quotes_and_comments() {
    let f = Fixture::new();
    f.allows(&format!(
        "cat <<'EOF'\nrm {}; git clean -f\nEOF",
        f.outside("example")
    ));
    f.allows("echo '<<EOF'\nrm ./nested/safe");
    f.allows("echo 'first line\n<<EOF'\nrm ./nested/safe");
    f.allows(&format!("echo ok # ; rm {}", f.outside("commented")));
    f.blocked(&format!("echo '<<EOF'\nrm {}", f.outside("real-command")));
    f.blocked(&format!(
        "cat <<'EOF'\nexample\nEOF\ntouch {}",
        f.outside("after-heredoc")
    ));
}

#[test]
fn uses_the_home_directory_and_environment() {
    let f = Fixture::new();
    f.blocks("touch ~/file", "outside the working directory");
    f.blocked(&format!("rm -rf {}", f.outside("directory")));
    let env = HashMap::from([
        ("TARGET".to_owned(), display(&f.project.join("nested"))),
        ("TMPDIR".to_owned(), display(&f.outside)),
    ]);
    let analyzer = CommandAnalyzer::new(&f.project, f.root.join("home"), &[], &[], env);
    assert!(analyzer.analyze("rm -rf \"$TARGET/cache\"").is_ok());
    let temporary = format!("rm -rf {}", f.outside("directory"));
    assert!(analyzer.analyze(&temporary).is_ok());
    assert!(analyzer.analyze("rm -rf ~/file").is_err());
}

#[test]
fn honors_allowed_paths() {
    let f = Fixture::new();
    let configured = Fixture::with_paths(&["../outside", "~/allowed", "./.env.vault"], &[]);
    let allowed = |f: &Fixture, command: &str| f.analyzer.analyze(command).is_ok();
    assert!(!allowed(&f, &format!("rm -rf {}", f.outside("directory"))));
    let c = &configured;
    assert!(allowed(c, &format!("rm -rf {}", c.outside("directory"))));
    assert!(allowed(
        c,
        &format!("mv ./source.txt {}", c.outside("moved"))
    ));
    assert!(allowed(c, "touch ~/allowed/file.txt"));
    assert!(allowed(c, "rm ./.env.vault"));
    assert!(allowed(c, "rm ./escape/inside-allowed-target"));
    assert!(!allowed(c, "rm /dev/null"));
    assert!(
        c.analyzer
            .validate_path(&display(&c.root.join("other/file.txt")))
            .is_err()
    );
    let devices = Fixture::with_paths(&["/dev/null"], &[]);
    assert!(allowed(&devices, "rm /dev/null"));
}

#[test]
fn honors_denied_paths_over_allowed_paths() {
    let f = Fixture::with_paths(
        &["../outside"],
        &["./nested", "../outside/directory", "~/denied"],
    );
    f.blocks("rm ./nested/cache", "denied path");
    f.blocks(&format!("rm -rf {}", f.outside("directory")), "denied path");
    f.blocks("tee nested/output.txt", "denied path");
    f.blocks("echo data > ./nested/output.txt", "denied path");
    f.blocks("touch ~/denied/file.txt", "denied path");
    f.blocks("rm -rf .", "denied path");
    f.blocks("rm -rf ./nes*", "denied path");
    assert!(f.analyzer.validate_path("./nested/file.txt").is_err());
    f.allows("touch ./source.txt");
    assert!(f.analyzer.validate_path("./other.txt").is_ok());
}
