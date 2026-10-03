//! A real git repository in a `TempDir`, so the git paths are exercised
//! against the real binary with no network.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Writes a minimal valid pack into `dir`.
pub fn write_pack(dir: &Path, name: &str, rule: &str) {
    fs::create_dir_all(dir.join("rules")).expect("rules dir");
    fs::write(dir.join("pack.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n"))
        .expect("pack.toml");
    fs::write(dir.join("rules").join("main.toml"), rule).expect("rule file");
}

pub fn rule(rule_name: &str, disposition: &str) -> String {
    format!(
        "[[rule]]\ntitle = \"{rule_name}\"\ndescription = \"a `marker` file sits inside it\"\n\
         kind = \"cache/v1\"\ndisposition = \"{disposition}\"\n\
         filter = '''\n$d has marker\nthen $d/\n'''\n"
    )
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Commits everything in `repo` and answers the commit SHA.
pub fn commit(repo: &Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.email=test@nomnom.invalid",
            "-c",
            "user.name=nomnom test",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
    git(repo, &["rev-parse", "HEAD"])
}

pub fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("repo dir");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet", "--initial-branch", "main"])
        .output()
        .expect("git init");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// A `file://` URL git accepts on every platform: forward slashes, and no
/// `\\?\` verbatim prefix.
pub fn file_url(path: &Path) -> String {
    let text = path.display().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).replace('\\', "/");
    format!("file:///{}", text.trim_start_matches('/'))
}
