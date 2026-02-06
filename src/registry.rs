use serde::Deserialize;
use std::process::Command;
use anyhow::{Result, Context};
use chrono::{DateTime, Utc};

#[derive(Debug, Deserialize, Clone)]
pub struct ImageMetadata {
    pub tags: Vec<String>,
    #[serde(rename = "updateTime")]
    pub update_time: DateTime<Utc>,
    pub name: String,
}

impl ImageMetadata {
    pub fn display_tag(&self) -> String {
        self.tags.join(", ")
    }

    pub fn short_hash(&self) -> String {
        self.name.split('@').last()
            .and_then(|h| h.strip_prefix("sha256:"))
            .and_then(|h| h.get(0..7))
            .unwrap_or("unknown")
            .to_string()
    }

    pub fn age_string(&self) -> String {
        let now = Utc::now();
        let duration = now.signed_duration_since(self.update_time);

        if duration.num_days() > 0 {
            format!("{}d ago", duration.num_days())
        } else if duration.num_hours() > 0 {
            format!("{}h ago", duration.num_hours())
        } else if duration.num_minutes() > 0 {
            format!("{}m ago", duration.num_minutes())
        } else {
            "just now".to_string()
        }
    }
}

pub struct Registry;

impl Registry {
    pub fn fetch_images(project: &str, location: &str, repository: &str, package: &str) -> Result<Vec<ImageMetadata>> {
        let image_path = format!("{}-docker.pkg.dev/{}/{}/{}", location, project, repository, package);
        
        let output = Command::new("gcloud")
            .args([
                "artifacts",
                "docker",
                "images",
                "list",
                &image_path,
                "--format=json",
                "--sort-by=~updateTime",
            ])
            .output()
            .context("Failed to execute gcloud command. Is gcloud installed and in PATH?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("gcloud command failed: {}", stderr));
        }

        let images: Vec<ImageMetadata> = serde_json::from_slice(&output.stdout)
            .context("Failed to parse gcloud JSON output")?;

        Ok(images)
    }
}
