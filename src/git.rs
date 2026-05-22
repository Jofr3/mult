use std::{
    path::Path,
    process::{Command, Stdio},
};

pub fn current_branch(cwd: &Path) -> Option<String> {
    run_git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    first_nonempty_line(&output.stdout)
}

fn first_nonempty_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_nonempty_line_trims_git_output() {
        assert_eq!(
            first_nonempty_line(b"\n  feature/work  \n"),
            Some("feature/work".to_string())
        );
    }

    #[test]
    fn first_nonempty_line_ignores_blank_output() {
        assert_eq!(first_nonempty_line(b"\n  \t\n"), None);
    }
}
