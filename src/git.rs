use anyhow::{Context, Result};
use chrono::Local;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

pub struct Git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPullReport {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

pub struct GitLogEntry {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub message: String,
}

impl Git {
    pub fn repo_root(path: &Path) -> Option<std::path::PathBuf> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()?;
        output.status.success().then(|| {
            std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
    }

    pub fn head_sha(path: &Path) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .context("Failed to resolve Git HEAD")?;
        if !output.status.success() {
            return Err(anyhow::anyhow!("git rev-parse HEAD failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Checks if the given directory is inside a git repository.
    pub fn is_repo(path: &Path) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg("--is-inside-work-tree")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Performs a git pull.
    pub fn pull(path: &Path, dry_run: bool) -> Result<GitPullReport> {
        if dry_run {
            return Ok(GitPullReport {
                stdout: format!("Dry-run: git -C {} pull --ff-only\n", path.display()),
                stderr: String::new(),
                success: true,
            });
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["pull", "--ff-only"])
            .output()
            .context("Failed to execute git pull")?;

        Ok(GitPullReport {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            success: output.status.success(),
        })
    }

    /// Gets the last commit that modified a specific file.
    pub fn last_commit_for_file(repo_path: &Path, file_path: &Path) -> Result<Option<GitLogEntry>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .arg("log")
            .arg("-1")
            .arg("--format=%H%n%an%n%ai%n%s")
            .arg("--")
            .arg(file_path)
            .output()
            .context("Failed to execute git log")?;

        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.trim().lines().collect();
        if lines.len() < 4 {
            return Ok(None);
        }

        Ok(Some(GitLogEntry {
            hash: lines[0].to_string(),
            author: lines[1].to_string(),
            date: lines[2].to_string(),
            message: lines[3].to_string(),
        }))
    }

    /// Adds, commits and pushes the change.
    pub fn commit_and_push(path: &Path, message: &str, file: &Path, dry_run: bool) -> Result<()> {
        if dry_run {
            println!("Dry-run: git -C {} add {}", path.display(), file.display());
            println!(
                "Dry-run: git -C {} commit -m \"{}\"",
                path.display(),
                message
            );
            println!("Dry-run: git -C {} push", path.display());
            return Ok(());
        }

        if !Self::is_repo(path) {
            return Err(anyhow::anyhow!(
                "Not inside a git repository: {}",
                path.display()
            ));
        }

        // git add <file>
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("add")
            .arg(file)
            .status()
            .context("Failed to execute git add")?;

        if !status.success() {
            return Err(anyhow::anyhow!("git add failed"));
        }

        // git commit -m <message>
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("commit")
            .arg("-m")
            .arg(message)
            .status()
            .context("Failed to execute git commit")?;

        if !status.success() {
            return Err(anyhow::anyhow!("git commit failed"));
        }

        // git push
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("push")
            .status()
            .context("Failed to execute git push")?;

        if !status.success() {
            return Err(anyhow::anyhow!("git push failed"));
        }

        Ok(())
    }

    /// Creates and pushes an annotated release tag for HEAD.
    ///
    /// The configured format produces the base name. If that name already
    /// exists locally or on origin, a zero-padded increment is appended.
    pub fn create_and_push_release_tag(
        path: &Path,
        format: &str,
        message: &str,
        dry_run: bool,
    ) -> Result<String> {
        if !Self::is_repo(path) {
            return Err(anyhow::anyhow!(
                "Not inside a git repository: {}",
                path.display()
            ));
        }

        let base = Local::now().format(format).to_string();
        let mut existing = Self::release_tags(path)?;

        for _ in 0..10_000 {
            let tag = next_release_tag(&base, &existing);
            Self::validate_tag_name(path, &tag)?;

            if dry_run {
                println!(
                    "Dry-run: git -C {} tag -a {} -m \"{}\" HEAD",
                    path.display(),
                    tag,
                    message
                );
                println!(
                    "Dry-run: git -C {} push origin refs/tags/{}",
                    path.display(),
                    tag
                );
                return Ok(tag);
            }

            let tag_status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["tag", "-a", &tag, "-m", message, "HEAD"])
                .status()
                .context("Failed to execute git tag")?;
            if !tag_status.success() {
                return Err(anyhow::anyhow!("git tag failed for '{}'", tag));
            }

            let push = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["push", "origin", &format!("refs/tags/{tag}")])
                .output()
                .context("Failed to execute git push for release tag")?;
            if push.status.success() {
                return Ok(tag);
            }

            Self::delete_local_tag(path, &tag)?;
            let refreshed = Self::release_tags(path)?;
            if refreshed.contains(&tag) {
                existing = refreshed;
                continue;
            }

            let stderr = String::from_utf8_lossy(&push.stderr).trim().to_string();
            return Err(anyhow::anyhow!(
                "git push failed for release tag '{}': {}",
                tag,
                stderr
            ));
        }

        Err(anyhow::anyhow!(
            "Could not allocate a release tag based on '{}' after 10000 attempts",
            base
        ))
    }

    fn release_tags(path: &Path) -> Result<HashSet<String>> {
        let local = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["tag", "--list"])
            .output()
            .context("Failed to list local Git tags")?;
        if !local.status.success() {
            return Err(anyhow::anyhow!("git tag --list failed"));
        }

        let remote = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["ls-remote", "--tags", "--refs", "origin"])
            .output()
            .context("Failed to list remote Git tags")?;
        if !remote.status.success() {
            return Err(anyhow::anyhow!(
                "git ls-remote failed while checking release tags: {}",
                String::from_utf8_lossy(&remote.stderr).trim()
            ));
        }

        let mut tags = String::from_utf8_lossy(&local.stdout)
            .lines()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        for line in String::from_utf8_lossy(&remote.stdout).lines() {
            if let Some((_, reference)) = line.split_once('\t')
                && let Some(tag) = reference.strip_prefix("refs/tags/")
            {
                tags.insert(tag.to_string());
            }
        }
        Ok(tags)
    }

    fn validate_tag_name(path: &Path, tag: &str) -> Result<()> {
        let reference = format!("refs/tags/{tag}");
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["check-ref-format", &reference])
            .status()
            .context("Failed to validate release tag name")?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Release tag format produced invalid Git tag '{}'",
                tag
            ))
        }
    }

    fn delete_local_tag(path: &Path, tag: &str) -> Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["tag", "--delete", tag])
            .status()
            .context("Failed to clean up local release tag")?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to clean up local release tag '{}' after push rejection",
                tag
            ))
        }
    }
}

fn next_release_tag(base: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(base) {
        return base.to_string();
    }

    for increment in 1_u32.. {
        let candidate = format!("{base}_{increment:02}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 release tag suffixes exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn release_repository() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("remote.git");
        let repository = directory.path().join("repository");

        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare"]);
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init"]);
        git(&repository, &["config", "user.name", "Davit Test"]);
        git(
            &repository,
            &["config", "user.email", "davit@example.invalid"],
        );
        fs::write(repository.join("release.yaml"), "version: 1\n").unwrap();
        git(&repository, &["add", "release.yaml"]);
        git(&repository, &["commit", "-m", "initial"]);
        git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repository, &["push", "-u", "origin", "HEAD"]);

        (directory, repository, remote)
    }

    #[test]
    fn release_tag_uses_base_then_first_available_increment() {
        let mut existing = HashSet::new();
        assert_eq!(
            next_release_tag("prod_20260925", &existing),
            "prod_20260925"
        );

        existing.insert("prod_20260925".to_string());
        existing.insert("prod_20260925_01".to_string());
        assert_eq!(
            next_release_tag("prod_20260925", &existing),
            "prod_20260925_02"
        );
    }

    #[test]
    fn release_tag_suffix_expands_past_two_digits() {
        let existing = (0..=99)
            .map(|increment| {
                if increment == 0 {
                    "prod".to_string()
                } else {
                    format!("prod_{increment:02}")
                }
            })
            .collect();
        assert_eq!(next_release_tag("prod", &existing), "prod_100");
    }

    #[test]
    fn creates_annotated_tags_and_increments_from_remote_tags() {
        let (_directory, repository, remote) = release_repository();

        let first = Git::create_and_push_release_tag(
            &repository,
            "prod_test",
            "release(production): api 1.0.0",
            false,
        )
        .unwrap();
        assert_eq!(first, "prod_test");

        git(&repository, &["tag", "--delete", "prod_test"]);
        let second = Git::create_and_push_release_tag(
            &repository,
            "prod_test",
            "release(production): api 1.0.1",
            false,
        )
        .unwrap();
        assert_eq!(second, "prod_test_01");

        let tags = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["tag", "--list"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&tags.stdout),
            "prod_test\nprod_test_01\n"
        );
        let kind = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["cat-file", "-t", "refs/tags/prod_test"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&kind.stdout), "tag\n");
    }

    #[test]
    fn rejects_a_format_that_produces_an_invalid_git_tag() {
        let (_directory, repository, _remote) = release_repository();

        let error = Git::create_and_push_release_tag(
            &repository,
            "release with spaces",
            "release(production): api 1.0.0",
            true,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("invalid Git tag"), "{error}");
    }
}
