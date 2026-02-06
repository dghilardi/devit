use std::path::{Path, PathBuf};
use std::fs;
use anyhow::{Result, Context};
use regex::Regex;
use similar::{ChangeTag, TextDiff};
use console::{style, Term};

pub struct Blueprint;

impl Blueprint {
    /// Finds the deployment YAML file for a given service in the environment repo root.
    pub fn find_deployment_yaml(repo_root: &Path, service: &str) -> Result<PathBuf> {
        let service_dir = repo_root.join(service);
        if !service_dir.exists() {
            return Err(anyhow::anyhow!("Service directory not found: {}", service_dir.display()));
        }

        let candidates = ["deployment.yaml", "deployment.yml", "deploy.yaml", "deploy.yml"];
        for candidate in candidates {
            let path = service_dir.join(candidate);
            if path.exists() {
                return Ok(path);
            }
        }

        Err(anyhow::anyhow!("Could not find a deployment YAML file in {}", service_dir.display()))
    }

    /// Modifies the image tag in the YAML content while preserving formatting/comments.
    /// It searches for 'image: ...:<old_tag>' and replaces it.
    pub fn update_image_tag(content: &str, new_tag: &str) -> Result<String> {
        // This regex looks for 'image:' followed by some characters (the registry/image name),
        // then a colon, and then a tag. We want to replace the tag.
        // We assume the image line doesn't have comments on the same line after the tag for simplicity, 
        // or we handle it by not matching past the end of the tag.
        
        // Pattern: image: (anything up to a colon) : (anything that looks like a tag)
        // Note: We need to be careful with images that use SHAs instead of tags (e.g. image@sha256:...)
        // But the requirement says "update the tag".
        
        let re = Regex::new(r"(?m)^(\s*image:\s*[^:\s]+):[^\s#]+").unwrap();
        
        if !re.is_match(content) {
            return Err(anyhow::anyhow!("Could not find 'image:' field in the YAML content"));
        }

        let new_content = re.replace_all(content, format!("$1:{}", new_tag)).to_string();
        Ok(new_content)
    }

    /// Displays a colored diff between old and new content.
    pub fn show_diff(old: &str, new: &str, filename: &str) {
        println!("\n{} {}", style("---").dim(), style(filename).bold());
        println!("{} {}", style("+++").dim(), style(filename).bold());

        let diff = TextDiff::from_lines(old, new);

        for change in diff.iter_all_changes() {
            let (sign, color) = match change.tag() {
                ChangeTag::Delete => ("-", "red"),
                ChangeTag::Insert => ("+", "green"),
                ChangeTag::Equal => (" ", "white"),
            };
            
            let line = change.to_string();
            let styled_line = if color == "red" {
                style(format!("{}{}", sign, line)).red()
            } else if color == "green" {
                style(format!("{}{}", sign, line)).green()
            } else {
                style(format!("{}{}", sign, line)).dim()
            };

            print!("{}", styled_line);
        }
        println!();
    }
}
