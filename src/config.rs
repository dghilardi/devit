use serde::Deserialize;
use std::path::PathBuf;
use anyhow::{Result, Context};
use directories::ProjectDirs;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub defaults: Option<Defaults>,
    pub environments: Vec<Environment>,
}

#[derive(Debug, Deserialize)]
pub struct Defaults {
    pub interactive: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Environment {
    pub name: String,
    pub repo_root: PathBuf,
    pub kubectl_context: String,
    pub gcp_project: Option<String>,
    pub gcp_location: Option<String>,
    pub gcp_repository: Option<String>,
    pub protected: Option<bool>,
}

impl Environment {
    pub fn list_services(&self) -> Result<Vec<String>> {
        let mut services = Vec::new();
        if !self.repo_root.exists() {
            return Ok(services);
        }

        for entry in fs::read_dir(&self.repo_root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        services.push(name.to_string());
                    }
                }
            }
        }
        services.sort();
        Ok(services)
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::get_config_path()?;
        
        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found at {}. Please create it based on documentation.",
                config_path.display()
            ));
        }

        let content = fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file at {}", config_path.display()))?;
        
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML config at {}", config_path.display()))?;

        Ok(config)
    }

    pub fn get_config_path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("DAVIT_CONFIG") {
            return Ok(PathBuf::from(path));
        }

        let proj_dirs = ProjectDirs::from("com", "davit", "davit")
            .context("Could not determine project directories")?;
        
        let mut config_path = proj_dirs.config_dir().to_path_buf();
        config_path.push("config.toml");
        
        Ok(config_path)
    }
}
