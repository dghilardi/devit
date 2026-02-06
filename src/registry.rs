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
    pub fn fetch_images(image_path: &str) -> Result<Vec<ImageMetadata>> {
        let base_image = image_path.split(':').next().unwrap_or(image_path);
        
        if base_image.contains("gcr.io") {
            let output = Command::new("gcloud")
                .args([
                    "container",
                    "images",
                    "list-tags",
                    base_image,
                    "--format=json",
                    "--sort-by=~timestamp",
                ])
                .output()
                .context("Failed to execute gcloud command for GCR. Is gcloud installed?")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!("gcloud command failed for GCR: {}", stderr));
            }

            let gcr_images: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
                .context("Failed to parse GCR JSON output")?;

            let images = gcr_images.into_iter().filter_map(|v| {
                let tags = v.get("tags")?.as_array()?.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect::<Vec<_>>();
                if tags.is_empty() { return None; }
                
                let digest = v.get("digest")?.as_str()?.to_string();
                
                // Try to parse timestamp. GCR format can be tricky.
                // Output example: "2026-02-05 19:49:35+01:00"
                let update_time = if let Some(ts) = v.get("timestamp") {
                    if let Some(dt) = ts.get("datetime").and_then(|d| d.as_str()) {
                        // Try parsing with timezone offset first (e.g., +01:00)
                        if let Ok(dt_parsed) = DateTime::parse_from_str(dt, "%Y-%m-%d %H:%M:%S%:z") {
                            dt_parsed.with_timezone(&Utc)
                        } else if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(dt, "%Y-%m-%d %H:%M:%S") {
                            DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc)
                        } else {
                            // Last resort: use individual fields if present
                            let year = ts.get("year").and_then(|y| y.as_i64()).unwrap_or(1970) as i32;
                            let month = ts.get("month").and_then(|m| m.as_u64()).unwrap_or(1) as u32;
                            let day = ts.get("day").and_then(|d| d.as_u64()).unwrap_or(1) as u32;
                            let hour = ts.get("hour").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
                            let minute = ts.get("minute").and_then(|m| m.as_u64()).unwrap_or(0) as u32;
                            let second = ts.get("second").and_then(|s| s.as_u64()).unwrap_or(0) as u32;
                            
                            let ndt = chrono::NaiveDate::from_ymd_opt(year, month, day)?
                                .and_hms_opt(hour, minute, second)?;
                            DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc)
                        }
                    } else {
                        return None;
                    }
                } else {
                    return None;
                };

                Some(ImageMetadata {
                    tags,
                    update_time,
                    name: format!("{}@{}", base_image, digest),
                })
            }).collect();

            Ok(images)
        } else {
            let output = Command::new("gcloud")
                .args([
                    "artifacts",
                    "docker",
                    "images",
                    "list",
                    base_image,
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
}
