use std::path::Path;
use std::process::Command;
use anyhow::{Result, Context};

pub struct Git;

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
    pub fn pull(path: &Path) -> Result<()> {
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

    /// Adds, commits and pushes the change.
    pub fn commit_and_push(path: &Path, message: &str, file: &Path) -> Result<()> {
        if !Self::is_repo(path) {
            return Err(anyhow::anyhow!("Not inside a git repository: {}", path.display()));
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
