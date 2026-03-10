use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct Git;

pub struct GitLogEntry {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub message: String,
}

impl Git {
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
    pub fn pull(path: &Path, dry_run: bool) -> Result<()> {
        if dry_run {
            println!("Dry-run: git -C {} pull", path.display());
            return Ok(());
        }

        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("pull")
            .status()
            .context("Failed to execute git pull")?;

        if !status.success() {
            return Err(anyhow::anyhow!("git pull failed"));
        }

        Ok(())
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
}
