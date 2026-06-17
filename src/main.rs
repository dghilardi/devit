mod blueprint;
mod config;
mod dashboard;
mod git;
mod info;
mod registry;

use anyhow::{Context, Result};
use blueprint::Blueprint;
use chrono::Utc;
use clap::{Parser, Subcommand};
use config::{Config, Environment, ServiceSource, YamlSource};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use dashboard::{Dashboard, DashboardExit};
use git::Git;
use inquire::{Confirm, Select, Text};
use registry::{ImageMetadata, Registry};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_PARALLEL_PULLS: usize = 5;
const TAG_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const TAG_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

        /// Wait until the provided image tag appears in the registry instead of prompting for similar tags
        #[arg(long, conflicts_with = "tag", value_name = "TAG")]
        wait_for_tag: Option<String>,

        /// Dry run: show commands without executing them
        #[arg(long)]
        dry_run: bool,

        /// After `kubectl apply`, continue automatically through rollout completion and Git push unless errors occur
        #[arg(long)]
        auto_continue: bool,
    },
    /// Show deployment information for a service
    Info {
        /// Target environment (e.g., staging, production)
        #[arg(short, long)]
        env: Option<String>,

        /// Kubernetes namespace filter
        #[arg(short, long)]
        namespace: Option<String>,

        /// Service name to inspect
        #[arg(short, long)]
        service: Option<String>,
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
        Commands::Deploy {
            env,
            service,
            tag,
            wait_for_tag,
            dry_run,
            auto_continue,
        } => {
            let selected_env = resolve_environment(&config, env)?;

            pull_yaml_sources(&selected_env, dry_run, "deployment")?;

            let selected_service = resolve_service(&selected_env, service)?;

            let selected_tag =
                resolve_tag(&selected_env, &selected_service, tag, wait_for_tag)?;

            // 6.3 Production Protection
            if selected_env.protected.unwrap_or(false) {
                println!(
                    "⚠️  WARNING: Deployment to {} is PROTECTED!",
                    selected_env.name
                );
                let confirmation = Text::new(&format!(
                    "Type the environment name '{}' to confirm:",
                    selected_env.name
                ))
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

            let base_image = selected_service
                .image_path
                .split([':', '@'])
                .next()
                .unwrap_or(&selected_service.image_path);

            let updated_content =
                Blueprint::update_image_tag(&original_content, base_image, &selected_tag)
                    .context("Failed to update image tag in YAML")?;

            let mut show_unified = true;
            let filename = yaml_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("deployment.yaml");

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
                        if dry_run {
                            println!(
                                "Dry-run: would write updated YAML to {}",
                                yaml_path.display()
                            );
                        } else {
                            fs::write(&yaml_path, &updated_content).with_context(|| {
                                format!("Failed to write updated YAML to {}", yaml_path.display())
                            })?;
                        }
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

            if dry_run {
                println!(
                    "Dry-run: kubectl --context {} apply -f {}",
                    selected_env.kubectl_context,
                    yaml_path.display()
                );
            } else {
                let output = Command::new("kubectl")
                    .args([
                        "--context",
                        &selected_env.kubectl_context,
                        "apply",
                        "-f",
                        yaml_path.to_str().unwrap(),
                    ])
                    .output()
                    .context("Failed to execute kubectl apply")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    println!("❌ kubectl apply failed: {}", stderr);
                    if !auto_continue {
                        if Confirm::new("Revert local YAML changes?")
                            .with_default(true)
                            .prompt()?
                        {
                            fs::write(&yaml_path, &original_content)?;
                            println!("YAML reverted.");
                        }
                    }
                    return Err(anyhow::anyhow!("kubectl apply failed"));
                }
            }

            println!("Deployment applied. Starting dashboard...");

            let mut dashboard = Dashboard::new(
                selected_service.name.clone(),
                selected_service.kind.clone(),
                selected_env.name.clone(),
                selected_tag.clone(),
                selected_env.kubectl_context.clone(),
                selected_service.namespace.clone(),
                selected_service.selector.clone(),
                selected_service.container_name.clone(),
                auto_continue,
            );
            let res = dashboard.run().await;

            match res {
                Err(e) => {
                    println!("❌ Dashboard error or aborted: {}", e);
                    if !auto_continue {
                        if Confirm::new("Revert local YAML changes?")
                            .with_default(true)
                            .prompt()?
                        {
                            fs::write(&yaml_path, &original_content)?;
                            println!("YAML reverted.");
                        }
                    }
                    return Err(e);
                }
                Ok(DashboardExit::UserQuit) => {
                    if auto_continue {
                        return Err(anyhow::anyhow!(
                            "Dashboard closed before rollout completion in auto-continue mode"
                        ));
                    }
                    println!("Dashboard closed before rollout completion check.");
                }
                Ok(DashboardExit::RolloutCompleted) => {
                    println!("Rollout completed. Continuing to the Git step...");
                }
            }

            // 6.1 Git Automation
            println!("\n🚀 Deployment successful. Preparing to commit changes...");
            let commit_msg = format!(
                "deploy({}): update {} to {}",
                selected_env.name, selected_service.name, selected_tag
            );

            println!("\n--- Commit Recap ---");
            println!("File to commit:   {}", yaml_path.display());
            println!("Commit message:   {}", commit_msg);
            Blueprint::show_diff(&original_content, &updated_content, filename, true);
            println!("--------------------\n");

            if auto_continue {
                Git::commit_and_push(
                    &selected_service.source_root,
                    &commit_msg,
                    &yaml_path,
                    dry_run,
                )?;
                if !dry_run {
                    println!("✅ Changes committed and pushed to Git.");
                }
            } else {
                if Confirm::new("Do you want to commit and push these changes?")
                    .with_default(true)
                    .prompt()?
                {
                    if let Err(e) = Git::commit_and_push(
                        &selected_service.source_root,
                        &commit_msg,
                        &yaml_path,
                        dry_run,
                    ) {
                        println!("⚠️  Failed to commit/push changes: {}", e);
                    } else if !dry_run {
                        println!("✅ Changes committed and pushed to Git.");
                    }
                } else {
                    println!("Committing skipped by user.");
                }
            }
        }
        Commands::Info {
            env,
            namespace,
            service,
        } => {
            let selected_env = resolve_environment(&config, env)?;

            pull_yaml_sources(&selected_env, false, "info")?;

            let selected_service =
                resolve_service_with_ns_filter(&selected_env, service, namespace)?;
            info::show_info(&selected_env, &selected_service).await?;
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
        None => Select::new("Select Environment:", env_names.clone())
            .prompt()
            .context("Environment selection was cancelled")?,
    };

    config
        .environments
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .context("Environment not found in config")
}

fn get_service_display_name(
    s: &ServiceSource,
    all_services: &[ServiceSource],
) -> String {
    let duplicates: Vec<&ServiceSource> = all_services
        .iter()
        .filter(|&other| other.name == s.name)
        .collect();

    if duplicates.len() <= 1 {
        return s.name.clone();
    }

    // Multiple services with same name, check namespace
    let same_namespace: Vec<&&ServiceSource> = duplicates
        .iter()
        .filter(|&other| other.namespace == s.namespace)
        .collect();

    if same_namespace.len() <= 1 {
        return format!(
            "{} ({})",
            s.name,
            s.namespace.as_deref().unwrap_or("no-namespace")
        );
    }

    // Multiple services with same name and same namespace, use relative path
    let relative_path = get_service_source_display_path(s);

    format!(
        "{} ({}) {}",
        s.name,
        s.namespace.as_deref().unwrap_or("no-namespace"),
        relative_path
    )
}

fn resolve_service(env: &Environment, input: Option<String>) -> Result<ServiceSource> {
    let services = env
        .list_services()
        .context("Failed to list services in repo_root")?;
    resolve_service_from_list(services, env, input)
}

fn resolve_service_with_ns_filter(
    env: &Environment,
    input: Option<String>,
    namespace: Option<String>,
) -> Result<ServiceSource> {
    let all_services = env.list_services().context("Failed to list services")?;

    let services = match namespace {
        Some(ref ns) => {
            let filtered: Vec<ServiceSource> = all_services
                .into_iter()
                .filter(|s| s.namespace.as_deref() == Some(ns.as_str()))
                .collect();
            if filtered.is_empty() {
                return Err(anyhow::anyhow!("No services found in namespace '{}'", ns));
            }
            filtered
        }
        None => all_services,
    };

    resolve_service_from_list(services, env, input)
}

fn resolve_service_from_list(
    services: Vec<ServiceSource>,
    env: &Environment,
    input: Option<String>,
) -> Result<ServiceSource> {
    if services.is_empty() {
        return Err(anyhow::anyhow!(
            "No services found in configured YAML sources for {}",
            env.name
        ));
    }

    let service_map: Vec<(String, ServiceSource)> = services
        .iter()
        .cloned()
        .map(|s| (get_service_display_name(&s, &services), s))
        .collect();

    let display_names: Vec<String> = service_map.iter().map(|(n, _)| n.clone()).collect();

    let selected_name = match input {
        Some(val) => resolve_from_list("Service", &display_names, val)?,
        None => Select::new("Select Service:", display_names.clone())
            .prompt()
            .context("Service selection was cancelled")?,
    };

    service_map
        .into_iter()
        .find(|(n, _)| n == &selected_name)
        .map(|(_, s)| s)
        .context("Resolved service not found in list")
}

fn get_service_source_display_path(service: &ServiceSource) -> String {
    let relative_path = pathdiff::diff_paths(&service.yaml_path, &service.source_root)
        .unwrap_or_else(|| service.yaml_path.clone());
    format!("[{}]/{}", service.source_name, relative_path.display())
}

fn pull_yaml_sources(env: &Environment, dry_run: bool, action: &str) -> Result<()> {
    let sources = unique_yaml_sources(env);

    if sources.is_empty() {
        return Ok(());
    }

    println!("🔄 Checking for updates in configured YAML sources...");
    if sources.len() > 1 {
        println!(
            "Running up to {} git pulls in parallel and showing each repository output sequentially.",
            MAX_PARALLEL_PULLS
        );
    }

    let mut failures = Vec::new();
    for (source, result) in collect_parallel_pull_results(&sources, MAX_PARALLEL_PULLS, move |source| {
        Git::pull(&source.root, dry_run)
    }) {
        println!("  - [{}] {}", source.name, source.root.display());
        match result {
            Ok(report) => {
                print_git_pull_report(&report.stdout, false);
                print_git_pull_report(&report.stderr, true);

                if !report.success {
                    failures.push((source, "git pull failed".to_string()));
                }
            }
            Err(error) => failures.push((source, error.to_string())),
        }
    }

    if failures.is_empty() {
        return Ok(());
    }

    println!("⚠️  Some YAML sources could not be updated:");
    for (source, error) in &failures {
        println!("  - [{}] {}: {}", source.name, source.root.display(), error);
    }

    if !Confirm::new(&format!("Do you want to continue with {} anyway?", action))
        .with_default(false)
        .prompt()?
    {
        return Err(anyhow::anyhow!(
            "{} aborted by user after git pull failure.",
            capitalize_action(action)
        ));
    }

    Ok(())
}

fn collect_parallel_pull_results<T, F>(
    sources: &[YamlSource],
    max_parallel: usize,
    pull: F,
) -> Vec<(YamlSource, Result<T>)>
where
    T: Send + 'static,
    F: Fn(&YamlSource) -> Result<T> + Send + Sync + 'static,
{
    let max_parallel = max_parallel.max(1);
    let pull = Arc::new(pull);
    let mut results = Vec::with_capacity(sources.len());

    for batch in sources.chunks(max_parallel) {
        let mut handles = Vec::with_capacity(batch.len());

        for source in batch {
            let source = source.clone();
            let pull = Arc::clone(&pull);
            handles.push(thread::spawn(move || {
                let result = pull(&source);
                (source, result)
            }));
        }

        for handle in handles {
            results.push(handle.join().unwrap_or_else(|_| {
                (
                    YamlSource {
                        name: "unknown".to_string(),
                        root: std::path::PathBuf::new(),
                    },
                    Err(anyhow::anyhow!("git pull worker thread panicked")),
                )
            }));
        }
    }

    results
}

fn print_git_pull_report(output: &str, is_stderr: bool) {
    for line in output.lines() {
        if is_stderr {
            eprintln!("    {}", line);
        } else {
            println!("    {}", line);
        }
    }
}

fn unique_yaml_sources(env: &Environment) -> Vec<YamlSource> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();

    for source in env.yaml_sources() {
        let key = source
            .root
            .canonicalize()
            .unwrap_or_else(|_| source.root.clone());
        if seen.insert(key) {
            unique.push(source);
        }
    }

    unique
}

fn capitalize_action(action: &str) -> String {
    let mut chars = action.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn resolve_tag(
    env: &Environment,
    service: &ServiceSource,
    input: Option<String>,
    wait_for_tag: Option<String>,
) -> Result<String> {
    if let Some(tag) = wait_for_tag {
        return wait_for_exact_tag(env, service, tag);
    }

    if let Some(tag) = input {
        let images = fetch_service_images(env, service)?;
        let available_tags = collect_available_tags(&images);

        if available_tags.is_empty() {
            return Err(anyhow::anyhow!(
                "No images found for service {}",
                service.name
            ));
        }

        return resolve_from_list("Image tag", &available_tags, tag);
    }

    let images = fetch_service_images(env, service)?;
    let available_tags = collect_available_tags(&images);

    if available_tags.is_empty() {
        return Err(anyhow::anyhow!(
            "No images found for service {}",
            service.name
        ));
    }

    let options: Vec<String> = images
        .iter()
        .map(|img| {
            format!(
                "{:<15} ({}) [{}]",
                img.display_tag(),
                img.age_string(),
                img.short_hash()
            )
        })
        .collect();

    let selection = Select::new("Select Image Tag:", options)
        .prompt()
        .context("Image selection was cancelled")?;

    let tag = selection
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(',')
        .to_string();
    Ok(tag)
}

fn fetch_service_images(env: &Environment, service: &ServiceSource) -> Result<Vec<ImageMetadata>> {
    let project = env.gcp_project.as_deref().unwrap_or("MOCK_PROJECT");

    println!(
        "Fetching images for {} using path {}...",
        service.name, service.image_path
    );

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

    Ok(images)
}

fn collect_available_tags(images: &[ImageMetadata]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut tags = Vec::new();

    for image in images {
        for tag in &image.tags {
            if seen.insert(tag.clone()) {
                tags.push(tag.clone());
            }
        }
    }

    tags
}

fn wait_for_exact_tag(env: &Environment, service: &ServiceSource, tag: String) -> Result<String> {
    let mut attempt = 1;

    loop {
        let images = fetch_service_images(env, service)?;
        let available_tags = collect_available_tags(&images);

        if available_tags.iter().any(|available| available == &tag) {
            if attempt == 1 {
                println!("Found image tag '{}'.", tag);
            } else {
                println!(
                    "Found image tag '{}' after {} checks. Continuing deployment.",
                    tag, attempt
                );
            }
            return Ok(tag);
        }

        wait_for_next_tag_check(service, &tag, attempt)?;
        attempt += 1;
    }
}

fn wait_for_next_tag_check(service: &ServiceSource, tag: &str, attempt: usize) -> Result<()> {
    let _raw_mode = RawModeGuard::new()?;
    let spinner = ['|', '/', '-', '\\'];
    let start = Instant::now();
    let mut frame = 0usize;

    loop {
        let elapsed = start.elapsed();
        if elapsed >= TAG_RETRY_INTERVAL {
            break;
        }

        let remaining = TAG_RETRY_INTERVAL.saturating_sub(elapsed).as_secs();
        let minutes = remaining / 60;
        let seconds = remaining % 60;

        print!(
            "\r{} Tag '{}' not found for {}. Check #{}. Retrying in {:02}:{:02}. Press 'q' to cancel.",
            spinner[frame % spinner.len()],
            tag,
            service.name,
            attempt + 1,
            minutes,
            seconds
        );
        io::stdout().flush()?;

        if event::poll(TAG_WAIT_POLL_INTERVAL)? {
            loop {
                if let Event::Key(key) = event::read()?
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            println!();
                            return Err(anyhow::anyhow!(
                                "Deployment aborted while waiting for image tag '{}'",
                                tag
                            ));
                        }
                        _ => {}
                    }
                }

                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        frame += 1;
    }

    println!();
    println!(
        "Rechecking registry for tag '{}' on service {}...",
        tag, service.name
    );
    Ok(())
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("Failed to enable terminal raw mode while waiting for image tag")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
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
    let matches: Vec<&String> = items.iter().filter(|&i| i.contains(&input)).collect();

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
        _ => Select::new(
            &format!(
                "Multiple matches for '{}'. Select {}:",
                input,
                label.to_lowercase()
            ),
            matches.into_iter().cloned().collect(),
        )
        .prompt()
        .context(format!("{} selection was cancelled", label)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn test_service_display_name_unique() {
        let s = ServiceSource {
            name: "service1".to_string(),
            kind: "Deployment".to_string(),
            image_path: "img1".to_string(),
            container_name: "c1".to_string(),
            source_name: "main".to_string(),
            source_root: PathBuf::from("/root"),
            yaml_path: PathBuf::from("/root/dir1/deploy.yaml"),
            namespace: Some("ns1".to_string()),
            selector: None,
        };
        let all = vec![s.clone()];
        assert_eq!(get_service_display_name(&s, &all), "service1");
    }

    #[test]
    fn test_service_display_name_duplicate_name() {
        let s1 = ServiceSource {
            name: "service1".to_string(),
            kind: "Deployment".to_string(),
            image_path: "img1".to_string(),
            container_name: "c1".to_string(),
            source_name: "main".to_string(),
            source_root: PathBuf::from("/root"),
            yaml_path: PathBuf::from("/root/dir1/deploy.yaml"),
            namespace: Some("ns1".to_string()),
            selector: None,
        };
        let s2 = ServiceSource {
            name: "service1".to_string(),
            kind: "Deployment".to_string(),
            image_path: "img2".to_string(),
            container_name: "c2".to_string(),
            source_name: "main".to_string(),
            source_root: PathBuf::from("/root"),
            yaml_path: PathBuf::from("/root/dir2/deploy.yaml"),
            namespace: Some("ns2".to_string()),
            selector: None,
        };
        let all = vec![s1.clone(), s2.clone()];
        assert_eq!(get_service_display_name(&s1, &all), "service1 (ns1)");
        assert_eq!(get_service_display_name(&s2, &all), "service1 (ns2)");
    }

    #[test]
    fn test_service_display_name_duplicate_name_and_ns() {
        let s1 = ServiceSource {
            name: "service1".to_string(),
            kind: "Deployment".to_string(),
            image_path: "img1".to_string(),
            container_name: "c1".to_string(),
            source_name: "main".to_string(),
            source_root: PathBuf::from("/root"),
            yaml_path: PathBuf::from("/root/dir1/deploy.yaml"),
            namespace: Some("ns1".to_string()),
            selector: None,
        };
        let s2 = ServiceSource {
            name: "service1".to_string(),
            kind: "Deployment".to_string(),
            image_path: "img2".to_string(),
            container_name: "c2".to_string(),
            source_name: "demo".to_string(),
            source_root: PathBuf::from("/root"),
            yaml_path: PathBuf::from("/root/dir2/deploy.yaml"),
            namespace: Some("ns1".to_string()),
            selector: None,
        };
        let all = vec![s1.clone(), s2.clone()];
        assert_eq!(
            get_service_display_name(&s1, &all),
            "service1 (ns1) [main]/dir1/deploy.yaml"
        );
        assert_eq!(
            get_service_display_name(&s2, &all),
            "service1 (ns1) [demo]/dir2/deploy.yaml"
        );
    }

    #[test]
    fn test_collect_available_tags_preserves_order_and_deduplicates() {
        let now = Utc::now();
        let images = vec![
            ImageMetadata {
                tags: vec!["v1.2.3".to_string(), "latest".to_string()],
                update_time: now,
                name: "service@sha256:abcdef1".to_string(),
            },
            ImageMetadata {
                tags: vec!["latest".to_string(), "v1.2.2".to_string()],
                update_time: now,
                name: "service@sha256:abcdef2".to_string(),
            },
        ];

        assert_eq!(
            collect_available_tags(&images),
            vec![
                "v1.2.3".to_string(),
                "latest".to_string(),
                "v1.2.2".to_string()
            ]
        );
    }

    #[test]
    fn test_deploy_wait_for_tag_conflicts_with_tag() {
        let parse = Cli::try_parse_from([
            "davit",
            "deploy",
            "--tag",
            "v1.2.3",
            "--wait-for-tag",
            "v1.2.4",
        ]);
        assert!(parse.is_err());
    }

    #[test]
    fn test_deploy_wait_for_tag_accepts_value() {
        let parse = Cli::try_parse_from(["davit", "deploy", "--wait-for-tag", "v1.2.3"]);
        assert!(parse.is_ok());
    }

    #[test]
    fn test_collect_parallel_pull_results_preserves_source_order() {
        let sources = vec![
            YamlSource {
                name: "one".to_string(),
                root: PathBuf::from("/tmp/one"),
            },
            YamlSource {
                name: "two".to_string(),
                root: PathBuf::from("/tmp/two"),
            },
            YamlSource {
                name: "three".to_string(),
                root: PathBuf::from("/tmp/three"),
            },
        ];

        let results = collect_parallel_pull_results(&sources, 2, |source| Ok(source.name.clone()));
        let ordered_names: Vec<String> = results
            .into_iter()
            .map(|(source, result)| {
                assert_eq!(result.as_ref().unwrap(), &source.name);
                source.name
            })
            .collect();

        assert_eq!(ordered_names, vec!["one", "two", "three"]);
    }

    #[test]
    fn test_collect_parallel_pull_results_respects_parallel_limit() {
        let sources: Vec<YamlSource> = (0..12)
            .map(|idx| YamlSource {
                name: format!("repo-{idx}"),
                root: PathBuf::from(format!("/tmp/repo-{idx}")),
            })
            .collect();

        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let active_for_pull = Arc::clone(&active);
        let max_seen_for_pull = Arc::clone(&max_seen);

        let results = collect_parallel_pull_results(&sources, 5, move |_source| {
            let current = active_for_pull.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen_for_pull.fetch_max(current, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
            active_for_pull.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(results.len(), 12);
        assert_eq!(max_seen.load(Ordering::SeqCst), 5);
    }
}
