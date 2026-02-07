mod config;
mod registry;
mod blueprint;
mod dashboard;
mod git;

use clap::{Parser, Subcommand};
use anyhow::{Result, Context};
use config::{Config, Environment, ServiceSource};
use registry::{Registry, ImageMetadata};
use blueprint::Blueprint;
use dashboard::Dashboard;
use git::Git;
use inquire::{Select, Confirm, Text};
use std::process::Command;
use chrono::Utc;
use std::fs;

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
            
            // Phase 6.2 - Git Pull before deployment
            println!("🔄 Checking for updates in {}...", selected_env.env_yaml_dir.display());
            if let Err(e) = Git::pull(&selected_env.env_yaml_dir) {
                println!("⚠️  Git pull failed: {}", e);
                if !Confirm::new("Do you want to continue with the deployment anyway?")
                    .with_default(false)
                    .prompt()? 
                {
                    return Err(anyhow::anyhow!("Deployment aborted by user after git pull failure."));
                }
            }

            let selected_service = resolve_service(&selected_env, service)?;
            
            let selected_tag = if let Some(t) = tag {
                t
            } else {
                resolve_tag(&selected_env, &selected_service)?
            };

            // 6.3 Production Protection
            if selected_env.protected.unwrap_or(false) {
                println!("⚠️  WARNING: Deployment to {} is PROTECTED!", selected_env.name);
                let confirmation = Text::new(&format!("Type the environment name '{}' to confirm:", selected_env.name))
                    .prompt()
                    .context("Production confirmation was cancelled")?;
                
                if confirmation != selected_env.name {
                    return Err(anyhow::anyhow!("Confirmation failed. Deployment aborted."));
                }
            }

            // Phase 4 - YAML modification & Visual Diff
            let yaml_path = selected_service.yaml_path.clone();
            
            let original_content = fs::read_to_string(&yaml_path)
                .with_context(|| format!("Failed to read YAML file at {}", yaml_path.display()))?;
            
            let base_image = selected_service.image_path.split([':', '@']).next().unwrap_or(&selected_service.image_path);
            
            let updated_content = Blueprint::update_image_tag(&original_content, base_image, &selected_tag)
                .context("Failed to update image tag in YAML")?;

            let mut show_unified = true;
            let filename = yaml_path.file_name().and_then(|n| n.to_str()).unwrap_or("deployment.yaml");

            loop {
                Blueprint::show_diff(&original_content, &updated_content, filename, show_unified);

                let choices = if show_unified {
                    vec!["Apply", "Show full diff", "Dismiss"]
                } else {
                    vec!["Apply", "Show unified diff", "Dismiss"]
                };

                let selection = Select::new("Action:", choices).prompt()?;

                match selection {
                    "Apply" => {
                        fs::write(&yaml_path, &updated_content)
                            .with_context(|| format!("Failed to write updated YAML to {}", yaml_path.display()))?;
                        println!("Local YAML updated. Executing kubectl apply...");
                        break;
                    }
                    "Show full diff" => show_unified = false,
                    "Show unified diff" => show_unified = true,
                    _ => {
                        println!("Deployment cancelled. No changes made.");
                        return Ok(());
                    }
                }
            }
            
            let output = Command::new("kubectl")
                    .args(["--context", &selected_env.kubectl_context, "apply", "-f", yaml_path.to_str().unwrap()])
                    .output()
                    .context("Failed to execute kubectl apply")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    println!("❌ kubectl apply failed: {}", stderr);
                    if Confirm::new("Revert local YAML changes?").with_default(true).prompt()? {
                        fs::write(&yaml_path, &original_content)?;
                        println!("YAML reverted.");
                    }
                    return Err(anyhow::anyhow!("kubectl apply failed"));
                }

                println!("Deployment applied. Starting dashboard...");
                
                let mut dashboard = Dashboard::new(
                    selected_service.name.clone(),
                    selected_env.name.clone(),
                    selected_tag.clone(),
                    selected_env.kubectl_context.clone(),
                    selected_service.namespace.clone(),
                    selected_service.selector.clone(),
                    selected_service.container_name.clone(),
                );
                let res = dashboard.run().await;

                if let Err(e) = res {
                    println!("❌ Dashboard error or aborted: {}", e);
                    if Confirm::new("Revert local YAML changes?").with_default(true).prompt()? {
                        fs::write(&yaml_path, &original_content)?;
                        println!("YAML reverted.");
                    }
                    return Err(e);
                }

                // 6.1 Git Automation
                println!("\n🚀 Deployment successful. Preparing to commit changes...");
                let commit_msg = format!("deploy({}): update {} to {}", selected_env.name, selected_service.name, selected_tag);
                
                println!("\n--- Commit Recap ---");
                println!("File to commit:   {}", yaml_path.display());
                println!("Commit message:   {}", commit_msg);
                println!("--------------------\n");

                if Confirm::new("Do you want to commit and push these changes?")
                    .with_default(true)
                    .prompt()? 
                {
                    if let Err(e) = Git::commit_and_push(&selected_env.env_yaml_dir, &commit_msg, &yaml_path) {
                        println!("⚠️  Failed to commit/push changes: {}", e);
                    } else {
                        println!("✅ Changes committed and pushed to Git.");
                    }
                } else {
                    println!("Committing skipped by user.");
                }
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

fn resolve_service(env: &Environment, input: Option<String>) -> Result<ServiceSource> {
    let services = env.list_services()
        .context("Failed to list services in repo_root")?;
    
    if services.is_empty() {
        return Err(anyhow::anyhow!("No services found in {}", env.env_yaml_dir.display()));
    }

    let service_name = match input {
        Some(val) => {
            let names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
            resolve_from_list("Service", &names, val)?
        }
        None => {
            let names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
            Select::new("Select Service:", names)
                .prompt()
                .context("Service selection was cancelled")?
        }
    };

    services.into_iter()
        .find(|s| s.name == service_name)
        .context("Resolved service not found in list")
}

fn resolve_tag(env: &Environment, service: &ServiceSource) -> Result<String> {
    let project = env.gcp_project.as_deref().unwrap_or("MOCK_PROJECT");

    println!("Fetching images for {} using path {}...", service.name, service.image_path);
    
    let images = match Registry::fetch_images(&service.image_path) {
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
        return Err(anyhow::anyhow!("No images found for service {}", service.name));
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
