mod config;
mod registry;

use clap::{Parser, Subcommand};
use anyhow::{Result, Context};
use config::{Config, Environment};
use registry::{Registry, ImageMetadata};
use inquire::{Select, Confirm};
use chrono::Utc;

#[derive(Parser)]
#[command(name = "davit")]
#[command(about = "A safe Kubernetes deployment wrapper & TUI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Deploy a service to an environment
    Deploy {
        /// Target environment (e.g., staging, production)
        #[arg(short, long)]
        env: Option<String>,

        /// Service name to deploy
        #[arg(short, long)]
        service: Option<String>,

        /// Image tag to deploy
        #[arg(short, long)]
        tag: Option<String>,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Get path to configuration file
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load().context("Failed to load configuration")?;

    match cli.command {
        Commands::Deploy { env, service, tag } => {
            let selected_env = resolve_environment(&config, env)?;
            let selected_service = resolve_service(&selected_env, service)?;
            
            let selected_tag = if let Some(t) = tag {
                t
            } else {
                resolve_tag(&selected_env, &selected_service)?
            };

            println!("Ready to deploy:");
            println!("  Environment: {}", selected_env.name);
            println!("  Service:     {}", selected_service);
            println!("  Tag:         {}", selected_tag);
            
            // TODO: Phase 4 - YAML modification & Visual Diff
        }
        Commands::Config { command } => match command {
            ConfigCommands::Show => {
                println!("{:#?}", config);
            }
            ConfigCommands::Path => {
                let path = Config::get_config_path()?;
                println!("{}", path.display());
            }
        },
    }

    Ok(())
}

fn resolve_environment(config: &Config, input: Option<String>) -> Result<Environment> {
    let env_names: Vec<String> = config.environments.iter().map(|e| e.name.clone()).collect();
    
    let name = match input {
        Some(val) => resolve_from_list("Environment", &env_names, val)?,
        None => {
            Select::new("Select Environment:", env_names.clone())
                .prompt()
                .context("Environment selection was cancelled")?
        }
    };

    config.environments.iter()
        .find(|e| e.name == name)
        .cloned()
        .context("Environment not found in config")
}

fn resolve_service(env: &Environment, input: Option<String>) -> Result<String> {
    let services = env.list_services()
        .context("Failed to list services in repo_root")?;
    
    if services.is_empty() {
        return Err(anyhow::anyhow!("No services found in {}", env.repo_root.display()));
    }

    match input {
        Some(val) => resolve_from_list("Service", &services, val),
        None => {
            Select::new("Select Service:", services)
                .prompt()
                .context("Service selection was cancelled")
        }
    }
}

fn resolve_tag(env: &Environment, service: &String) -> Result<String> {
    let project = env.gcp_project.as_deref().unwrap_or("MOCK_PROJECT");
    let location = env.gcp_location.as_deref().unwrap_or("europe-west1");
    let repository = env.gcp_repository.as_deref().unwrap_or("MOCK_REPO");

    println!("Fetching images for {} from {}...", service, repository);
    
    // In a real scenario, this would call Registry::fetch_images.
    // For Phase 3 verification, we'll implement a fallback to mock data if gcloud fails or project is MOCK.
    let images = match Registry::fetch_images(project, location, repository, service) {
        Ok(imgs) => imgs,
        Err(e) => {
            if project == "MOCK_PROJECT" {
                mock_images()
            } else {
                return Err(e).context("Failed to fetch images from Artifact Registry");
            }
        }
    };

    if images.is_empty() {
        return Err(anyhow::anyhow!("No images found for service {}", service));
    }

    let options: Vec<String> = images.iter()
        .map(|img| {
            format!("{:<15} ({}) [{}]", 
                img.display_tag(), 
                img.age_string(), 
                img.short_hash())
        })
        .collect();

    let selection = Select::new("Select Image Tag:", options)
        .prompt()
        .context("Image selection was cancelled")?;

    // Extract the tag from the selection string. 
    // The tag part is before the first space, and we strip any trailing commas.
    let tag = selection.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(',')
        .to_string();
    Ok(tag)
}

fn mock_images() -> Vec<ImageMetadata> {
    use chrono::Duration;
    let now = Utc::now();
    vec![
        ImageMetadata {
            tags: vec!["v1.2.3".to_string(), "latest".to_string()],
            update_time: now - Duration::hours(2),
            name: "auth-service@sha256:abcdef123456789".to_string(),
        },
        ImageMetadata {
            tags: vec!["v1.2.2".to_string()],
            update_time: now - Duration::days(1),
            name: "auth-service@sha256:123456789abcdef".to_string(),
        },
        ImageMetadata {
            tags: vec!["v1.1.0".to_string()],
            update_time: now - Duration::days(5),
            name: "auth-service@sha256:987654321fedcba".to_string(),
        },
    ]
}

/// Generic disambiguation logic
fn resolve_from_list(label: &str, items: &[String], input: String) -> Result<String> {
    // 1. Exact match
    if items.contains(&input) {
        return Ok(input);
    }

    // 2. Partial matches
    let matches: Vec<&String> = items.iter()
        .filter(|&i| i.contains(&input))
        .collect();

    match matches.len() {
        0 => {
            println!("No {} matches '{}'.", label.to_lowercase(), input);
            Select::new(&format!("Select {}:", label), items.to_vec())
                .prompt()
                .context(format!("{} selection was cancelled", label))
        }
        1 => {
            let suggest = matches[0];
            if Confirm::new(&format!("Did you mean '{}'?", suggest))
                .with_default(true)
                .prompt()? 
            {
                Ok(suggest.clone())
            } else {
                Select::new(&format!("Select {}:", label), items.to_vec())
                    .prompt()
                    .context(format!("{} selection was cancelled", label))
            }
        }
        _ => {
            Select::new(&format!("Multiple matches for '{}'. Select {}:", input, label.to_lowercase()), 
                        matches.into_iter().cloned().collect())
                .prompt()
                .context(format!("{} selection was cancelled", label))
        }
    }
}
