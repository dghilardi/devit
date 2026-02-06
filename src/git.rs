use std::path::Path;
use std::process::Command;
use anyhow::{Result, Context};

pub struct Git;

impl Git {
    /// Checks if the given directory is a git repository.
    pub fn is_repo(path: &Path) -> bool {
        path.join(".git").exists()
    }

    /// Adds, commits and pushes the change.
    pub fn commit_and_push(repo_root: &Path, message: &str, file: &Path) -> Result<()> {
        if !Self::is_repo(repo_root) {
            return Err(anyhow::anyhow!("Not a git repository: {}", repo_root.display()));
        }

        // git add <file>
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_root)
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
            .arg(repo_root)
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
            .arg(repo_root)
            .arg("push")
            .status()
            .context("Failed to execute git push")?;

        if !status.success() {
            return Err(anyhow::anyhow!("git push failed"));
        }

        Ok(())
    }
}
