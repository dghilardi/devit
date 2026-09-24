mod blueprint;
mod config;
mod dashboard;
mod git;
mod helm;
mod info;
mod prompt;
mod registry;

use anyhow::{Context, Result};
use blueprint::Blueprint;
use chrono::Utc;
use clap::{Parser, Subcommand};
use config::{Config, DeploymentDriver, Environment, ServiceSource, YamlSource};
use crossterm::{
    cursor::MoveToColumn,
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size as terminal_size},
};
use dashboard::{Dashboard, DashboardExit};
use git::{Git, GitPullReport};
use inquire::{Confirm, Select, Text};
use prompt::{PromptPolicy, format_candidates};
use registry::{ImageMetadata, Registry};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_PARALLEL_PULLS: usize = 5;
const TAG_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const TAG_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const TAG_WAIT_CANCELLED_MESSAGE: &str = "__TAG_WAIT_CANCELLED__";
/// Stands for "declares no namespace of its own", in the NAMESPACE column and
/// in `--namespace` alike, so the listing round-trips. A lone `-` cannot
/// collide with a real namespace: those must start and end alphanumerically.
const NO_NAMESPACE: &str = "-";
/// Caps the SERVICE column. A name disambiguated all the way down to its
/// manifest path can run past 100 characters, and padding every other row to
/// match makes the listing unreadable.
const MAX_SERVICE_COLUMN: usize = 48;

#[derive(Debug, PartialEq, Eq)]
enum RemoteManifestDiffCheck {
    InSync,
    Drift(String),
    CheckFailed(String),
    SkippedDryRun,
}

#[derive(Parser)]
#[command(name = "davit")]
#[command(version)]
#[command(about = "A safe Kubernetes deployment wrapper & TUI", long_about = None)]
struct Cli {
    /// Never prompt; fail instead, naming the decision that would have been asked
    #[arg(long, global = true)]
    non_interactive: bool,

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

        /// Kubernetes namespace, or `-` for manifests that declare none, to disambiguate a service name
        #[arg(short, long, allow_hyphen_values = true)]
        namespace: Option<String>,

        /// Image tag to deploy
        #[arg(short, long)]
        tag: Option<String>,

        /// Wait until the provided image tag appears in the registry instead of prompting for similar tags
        #[arg(long, conflicts_with = "tag", value_name = "TAG")]
        wait_for_tag: Option<String>,

        /// Dry run: show commands without executing them
        #[arg(long)]
        dry_run: bool,

        /// Read the YAML sources as they are on disk, without running git pull first
        #[arg(long)]
        no_fetch: bool,

        /// Apply the selected version immediately and continue automatically through rollout and Git steps
        #[arg(long)]
        auto_apply: bool,

        /// After `kubectl apply`, continue automatically through rollout completion and Git push unless errors occur
        #[arg(long)]
        auto_continue: bool,

        /// Confirm a protected environment without prompting; must match the target environment name
        #[arg(long, value_name = "NAME")]
        confirm_env: Option<String>,
    },
    /// Show deployment information for a service
    Info {
        /// Target environment (e.g., staging, production)
        #[arg(short, long)]
        env: Option<String>,

        /// Kubernetes namespace filter, or `-` for manifests that declare none
        #[arg(short, long, allow_hyphen_values = true)]
        namespace: Option<String>,

        /// Service name to inspect
        #[arg(short, long)]
        service: Option<String>,

        /// Read the YAML sources as they are on disk, without running git pull first
        #[arg(long)]
        no_fetch: bool,
    },
    /// List the identifiers accepted by --env and --service
    List {
        #[command(subcommand)]
        command: ListCommands,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Subcommand)]
enum ListCommands {
    /// List the configured environments
    Envs,
    /// List the services discovered in an environment's YAML sources
    Services {
        /// Target environment (e.g., staging, production)
        #[arg(short, long)]
        env: Option<String>,

        /// Only list services in this namespace, or `-` for those that declare none
        #[arg(short, long, allow_hyphen_values = true)]
        namespace: Option<String>,

        /// Read the YAML sources as they are on disk, without running git pull first
        #[arg(long)]
        no_fetch: bool,
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
    let policy = PromptPolicy::resolve(cli.non_interactive);

    match cli.command {
        Commands::Deploy {
            env,
            service,
            namespace,
            tag,
            wait_for_tag,
            dry_run,
            no_fetch,
            auto_apply,
            auto_continue,
            confirm_env,
        } => {
            let auto_continue = auto_continue || auto_apply;
            let selected_env = resolve_environment(&config, env, policy)?;

            pull_yaml_sources(&selected_env, dry_run, no_fetch, "deployment", policy)?;

            let selected_service =
                resolve_service_with_ns_filter(&selected_env, service, namespace, policy)?;

            if selected_service.helm.is_none() {
                let remote_diff = check_remote_manifest_diff(
                    &selected_env.kubectl_context,
                    &selected_service.yaml_path,
                    dry_run,
                )?;
                if !confirm_remote_manifest_diff(&selected_service.yaml_path, remote_diff, policy)?
                {
                    println!("Deployment cancelled. No changes made.");
                    return Ok(());
                }
            }

            let selected_tag =
                match resolve_tag(&selected_env, &selected_service, tag, wait_for_tag, policy) {
                    Ok(tag) => tag,
                    Err(err) if err.to_string() == TAG_WAIT_CANCELLED_MESSAGE => {
                        println!("Tag wait cancelled. Deployment aborted.");
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                };

            // 6.3 Production Protection
            if selected_env.protected.unwrap_or(false) {
                confirm_protected_environment(
                    &selected_env.name,
                    confirm_env.as_deref(),
                    dry_run,
                    policy,
                )?;
            }

            if selected_service.helm.is_some() {
                deploy_helm_release(
                    &selected_env,
                    &selected_service,
                    &selected_tag,
                    dry_run,
                    auto_apply,
                    auto_continue,
                    policy,
                )
                .await?;
                return Ok(());
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

                if dry_run {
                    println!(
                        "Dry-run: would write updated YAML to {}",
                        yaml_path.display()
                    );
                    break;
                }

                if auto_apply {
                    fs::write(&yaml_path, &updated_content).with_context(|| {
                        format!("Failed to write updated YAML to {}", yaml_path.display())
                    })?;
                    println!("Auto-apply enabled. Local YAML updated. Executing kubectl apply...");
                    break;
                }

                let choices = if show_unified {
                    vec!["Apply", "Show full diff", "Dismiss"]
                } else {
                    vec!["Apply", "Show unified diff", "Dismiss"]
                };

                let selection = policy.select(
                    "approval of the manifest change",
                    "Action:",
                    choices.into_iter().map(str::to_string).collect(),
                    "Re-run with --auto-apply to apply it without asking, or --dry-run to preview only.",
                )?;

                let selection = selection.as_str();

                match selection {
                    "Apply" => {
                        fs::write(&yaml_path, &updated_content).with_context(|| {
                            format!("Failed to write updated YAML to {}", yaml_path.display())
                        })?;
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
                    offer_revert(&yaml_path, &original_content, auto_continue, policy)?;
                    return Err(anyhow::anyhow!("kubectl apply failed"));
                }
            }

            if dry_run {
                println!(
                    "Dry-run: would monitor the rollout of {} in the dashboard.",
                    selected_service.name
                );
            } else {
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
                        offer_revert(&yaml_path, &original_content, auto_continue, policy)?;
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

            if auto_continue || dry_run {
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
                if policy.confirm(
                    "approval to commit and push",
                    "Do you want to commit and push these changes?",
                    true,
                    "Re-run with --auto-continue to commit and push without asking.",
                )? {
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
            no_fetch,
        } => {
            let selected_env = resolve_environment(&config, env, policy)?;

            pull_yaml_sources(&selected_env, false, no_fetch, "info", policy)?;

            let selected_service =
                resolve_service_with_ns_filter(&selected_env, service, namespace, policy)?;
            let selected_service = materialize_helm_service(selected_service)?;
            info::show_info(&selected_env, &selected_service).await?;
        }
        Commands::List { command } => match command {
            ListCommands::Envs => {
                for environment in &config.environments {
                    let protected = if environment.protected.unwrap_or(false) {
                        "  (protected)"
                    } else {
                        ""
                    };
                    println!("{}{}", environment.name, protected);
                }
            }
            ListCommands::Services {
                env,
                namespace,
                no_fetch,
            } => {
                let selected_env = resolve_environment(&config, env, policy)?;

                pull_yaml_sources(&selected_env, false, no_fetch, "listing", policy)?;

                let services = list_services_in_namespace(&selected_env, namespace.as_deref())?;
                print_service_list(&services);
            }
        },
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

async fn deploy_helm_release(
    env: &Environment,
    service: &ServiceSource,
    selected_tag: &str,
    dry_run: bool,
    auto_apply: bool,
    auto_continue: bool,
    policy: PromptPolicy,
) -> Result<()> {
    let helm_source = service
        .helm
        .as_ref()
        .context("Helm deployment metadata is missing")?;
    let original_content = fs::read_to_string(&helm_source.values_path).with_context(|| {
        format!(
            "Failed to read Helm values at {}",
            helm_source.values_path.display()
        )
    })?;
    let updated_content =
        helm::update_tag_at_path(&original_content, &helm_source.image_tag_path, selected_tag)?;
    if updated_content == original_content {
        println!(
            "Helm values already select tag '{}'; no release is needed.",
            selected_tag
        );
        return Ok(());
    }

    println!("Validating chart with helm lint and helm template...");
    helm::lint(service, &updated_content)?;
    let old_rendered = helm::render(service, &original_content)?;
    let new_rendered = helm::render(service, &updated_content)?;
    let filename = helm_source
        .values_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("values.yaml");

    println!("\nValues change:");
    Blueprint::show_diff(&original_content, &updated_content, filename, true);
    println!("\nRendered Kubernetes change:");
    Blueprint::show_diff(&old_rendered, &new_rendered, "helm-template.yaml", true);
    if dry_run {
        println!("\nDry-run: would compare the rendered manifests with the cluster.");
    } else {
        match helm::cluster_diff(env, &new_rendered)? {
            Some(diff) => println!("\nRendered state versus cluster:\n{diff}"),
            None => println!("\nRendered state is already aligned with the cluster."),
        }
    }

    if dry_run {
        println!(
            "Dry-run: would commit {} and deploy Application '{}' using {:?}.",
            helm_source.values_path.display(),
            helm_source.application_name,
            env.deployment_driver
        );
        return Ok(());
    }

    if !auto_apply
        && !policy.confirm(
            "approval of the Helm release change",
            "Commit this values change and deploy the release?",
            true,
            "Re-run with --auto-apply to commit and deploy without asking.",
        )?
    {
        println!("Deployment cancelled. No changes made.");
        return Ok(());
    }

    fs::write(&helm_source.values_path, &updated_content).with_context(|| {
        format!(
            "Failed to write Helm values to {}",
            helm_source.values_path.display()
        )
    })?;
    let commit_message = format!(
        "deploy({}): update {} to {}",
        env.name, service.name, selected_tag
    );
    Git::commit_and_push(
        &service.source_root,
        &commit_message,
        &helm_source.values_path,
        false,
    )?;
    let revision = Git::head_sha(&service.source_root)?;
    println!("Committed desired state at {}.", revision);

    match env.deployment_driver {
        DeploymentDriver::Helm => helm::helm_upgrade(env, service)?,
        DeploymentDriver::ArgoCd => helm::argocd_sync(service, &revision)?,
        DeploymentDriver::Manifest => unreachable!("Helm service with manifest driver"),
    }

    let workload = helm::resolve_workload(&new_rendered, image_repository(&service.image_path))?;
    println!(
        "Release applied. Starting dashboard for {}...",
        workload.name
    );
    let mut dashboard = Dashboard::new(
        workload.name,
        workload.kind,
        env.name.clone(),
        selected_tag.to_string(),
        env.kubectl_context.clone(),
        workload.namespace.or_else(|| service.namespace.clone()),
        workload.selector,
        workload.container_name,
        auto_continue,
    );
    match dashboard.run().await? {
        DashboardExit::RolloutCompleted => println!("Rollout completed."),
        DashboardExit::UserQuit if auto_continue => {
            return Err(anyhow::anyhow!(
                "Dashboard closed before rollout completion in auto-continue mode"
            ));
        }
        DashboardExit::UserQuit => println!("Dashboard closed before rollout completion check."),
    }
    Ok(())
}

fn materialize_helm_service(mut service: ServiceSource) -> Result<ServiceSource> {
    let Some(helm_source) = service.helm.as_ref() else {
        return Ok(service);
    };
    let values = fs::read_to_string(&helm_source.values_path)?;
    let rendered = helm::render(&service, &values)?;
    let workload = helm::resolve_workload(&rendered, image_repository(&service.image_path))?;
    service.name = workload.name;
    service.kind = workload.kind;
    service.namespace = workload.namespace.or(service.namespace);
    service.selector = workload.selector;
    service.container_name = workload.container_name;
    Ok(service)
}

fn image_repository(image: &str) -> &str {
    if let Some((repository, _)) = image.split_once('@') {
        return repository;
    }
    if let Some((repository, tag)) = image.rsplit_once(':')
        && !tag.contains('/')
    {
        return repository;
    }
    image
}

fn resolve_environment(
    config: &Config,
    input: Option<String>,
    policy: PromptPolicy,
) -> Result<Environment> {
    let env_names: Vec<String> = config.environments.iter().map(|e| e.name.clone()).collect();

    let name = match input {
        Some(val) => resolve_from_list("Environment", &env_names, val, policy)?,
        None => policy.select(
            "an environment",
            "Select Environment:",
            env_names.clone(),
            &format!("Pass --env with one of:\n{}", format_candidates(&env_names)),
        )?,
    };

    config
        .environments
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .context("Environment not found in config")
}

fn get_service_display_name(s: &ServiceSource, all_services: &[ServiceSource]) -> String {
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

/// Lists services in a stable order, optionally narrowed to one namespace.
///
/// The order matters beyond presentation: it fixes the order of the candidate
/// lists reported when a selection cannot be made, and of the prompt itself.
fn list_services_in_namespace(
    env: &Environment,
    namespace: Option<&str>,
) -> Result<Vec<ServiceSource>> {
    let all_services = env.list_services().context("Failed to list services")?;

    let mut services = match namespace {
        Some(ns) => {
            let filtered: Vec<ServiceSource> = all_services
                .into_iter()
                .filter(|service| namespace_label(service) == ns)
                .collect();
            if filtered.is_empty() {
                return Err(if ns == NO_NAMESPACE {
                    anyhow::anyhow!("No services without a declared namespace found")
                } else {
                    anyhow::anyhow!("No services found in namespace '{}'", ns)
                });
            }
            filtered
        }
        None => all_services,
    };

    services.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.namespace.cmp(&b.namespace))
            .then_with(|| a.yaml_path.cmp(&b.yaml_path))
    });

    Ok(services)
}

fn resolve_service_with_ns_filter(
    env: &Environment,
    input: Option<String>,
    namespace: Option<String>,
    policy: PromptPolicy,
) -> Result<ServiceSource> {
    let services = list_services_in_namespace(env, namespace.as_deref())?;
    let input = input.map(|value| strip_namespace_suffix(value, namespace.as_deref()));
    resolve_service_from_list(services, env, input, policy)
}

/// Drops the `(namespace)` suffix from a service name already narrowed by `--namespace`.
///
/// `list services` disambiguates names across the whole environment, so it
/// prints `svc-api (tenant-b)`. Passing that back together with the NAMESPACE
/// column would otherwise fail, because within the narrowed set the name no
/// longer collides and is rendered plain.
fn strip_namespace_suffix(value: String, namespace: Option<&str>) -> String {
    let Some(namespace) = namespace else {
        return value;
    };

    let rendered = if namespace == NO_NAMESPACE {
        "no-namespace"
    } else {
        namespace
    };

    match value.strip_suffix(&format!(" ({})", rendered)) {
        Some(stripped) => stripped.to_string(),
        None => value,
    }
}

fn resolve_service_from_list(
    services: Vec<ServiceSource>,
    env: &Environment,
    input: Option<String>,
    policy: PromptPolicy,
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
        Some(val) => resolve_from_list("Service", &display_names, val, policy)?,
        None => policy.select(
            "a service",
            "Select Service:",
            display_names.clone(),
            &format!(
                "Pass --service with one of:\n{}",
                format_candidates(&display_names)
            ),
        )?,
    };

    service_map
        .into_iter()
        .find(|(n, _)| n == &selected_name)
        .map(|(_, s)| s)
        .context("Resolved service not found in list")
}

/// Prints the values `--service` accepts, alongside what each one resolves to.
fn print_service_list(services: &[ServiceSource]) {
    let rows: Vec<(String, &ServiceSource)> = services
        .iter()
        .map(|service| (get_service_display_name(service, services), service))
        .collect();

    let name_width = service_column_width(&rows);
    let namespace_width = rows
        .iter()
        .map(|(_, service)| namespace_label(service).chars().count())
        .chain(std::iter::once("NAMESPACE".len()))
        .max()
        .unwrap_or_default();

    println!(
        "{:<name_width$}  {:<namespace_width$}  {:<12}  MANIFEST",
        "SERVICE", "NAMESPACE", "KIND"
    );

    for (name, service) in &rows {
        println!(
            "{:<name_width$}  {:<namespace_width$}  {:<12}  {}",
            name,
            namespace_label(service),
            service.kind,
            get_service_source_display_path(service)
        );
    }
}

/// Indefinite article for a label, so an error reads "an image tag", not
/// "a image tag". A leading-vowel test is enough for the labels in use
/// (environment, service, image tag) and for anything similar.
fn article_for(noun: &str) -> &'static str {
    match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// Width of the SERVICE column: the widest name that still fits the cap.
///
/// Names past the cap overflow their own row instead of widening every other
/// one. They are never truncated: the value has to stay copy-pasteable into
/// `--service`, which is the whole point of the listing.
fn service_column_width<T>(rows: &[(String, T)]) -> usize {
    rows.iter()
        .map(|(name, _)| name.chars().count())
        .chain(std::iter::once("SERVICE".len()))
        .filter(|width| *width <= MAX_SERVICE_COLUMN)
        .max()
        .unwrap_or(MAX_SERVICE_COLUMN)
}

/// The namespace as the manifest declares it; `--namespace` matches this value.
fn namespace_label(service: &ServiceSource) -> &str {
    service.namespace.as_deref().unwrap_or(NO_NAMESPACE)
}

fn get_service_source_display_path(service: &ServiceSource) -> String {
    let relative_path = pathdiff::diff_paths(&service.yaml_path, &service.source_root)
        .unwrap_or_else(|| service.yaml_path.clone());
    format!("[{}]/{}", service.source_name, relative_path.display())
}

fn pull_yaml_sources(
    env: &Environment,
    dry_run: bool,
    no_fetch: bool,
    action: &str,
    policy: PromptPolicy,
) -> Result<()> {
    if no_fetch {
        println!("Skipping the YAML source refresh (--no-fetch); reading manifests from disk.");
        return Ok(());
    }

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
    for (source, result) in
        collect_parallel_pull_results(&sources, MAX_PARALLEL_PULLS, move |source| {
            pull_source(source, dry_run)
        })
    {
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

    if !policy.confirm(
        &format!(
            "approval to continue with {} after a failed git pull",
            action
        ),
        &format!("Do you want to continue with {} anyway?", action),
        false,
        "Fix the YAML sources listed above before retrying.",
    )? {
        return Err(anyhow::anyhow!(
            "{} aborted by user after git pull failure.",
            capitalize_action(action)
        ));
    }

    Ok(())
}

/// Pulls one YAML source, unless that would merge into uncommitted local work.
///
/// Leaving the source stale is reported and treated as a success: it is not a
/// failure the caller has to confirm, and refusing to touch edited manifests is
/// the point.
fn pull_source(source: &YamlSource, dry_run: bool) -> Result<GitPullReport> {
    if !dry_run && Git::is_dirty(&source.root)? {
        return Ok(GitPullReport {
            stdout: "Skipped: the working copy has uncommitted changes, so it was not pulled.\nManifests read from it may be stale.\n".to_string(),
            stderr: String::new(),
            success: true,
        });
    }

    Git::pull(&source.root, dry_run)
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

fn check_remote_manifest_diff(
    kubectl_context: &str,
    yaml_path: &Path,
    dry_run: bool,
) -> Result<RemoteManifestDiffCheck> {
    if dry_run {
        println!(
            "Dry-run: kubectl --context {} diff -f {}",
            kubectl_context,
            yaml_path.display()
        );
        return Ok(RemoteManifestDiffCheck::SkippedDryRun);
    }

    let output = Command::new("kubectl")
        .args([
            "--context",
            kubectl_context,
            "diff",
            "-f",
            yaml_path.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute kubectl diff")?;

    Ok(classify_kubectl_diff_result(
        output.status.code(),
        &output.stdout,
        &output.stderr,
    ))
}

fn classify_kubectl_diff_result(
    status_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> RemoteManifestDiffCheck {
    let output = combine_command_output(stdout, stderr);

    match status_code {
        Some(0) => RemoteManifestDiffCheck::InSync,
        Some(1) => RemoteManifestDiffCheck::Drift(output),
        Some(code) => RemoteManifestDiffCheck::CheckFailed(format!(
            "kubectl diff exited with status {}.\n{}",
            code,
            fallback_command_output(output)
        )),
        None => RemoteManifestDiffCheck::CheckFailed(format!(
            "kubectl diff terminated without an exit status.\n{}",
            fallback_command_output(output)
        )),
    }
}

fn combine_command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();

    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{}\n{}", stdout, stderr),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => String::new(),
    }
}

fn fallback_command_output(output: String) -> String {
    if output.is_empty() {
        "No output returned by kubectl diff.".to_string()
    } else {
        output
    }
}

fn confirm_remote_manifest_diff(
    yaml_path: &Path,
    diff_check: RemoteManifestDiffCheck,
    policy: PromptPolicy,
) -> Result<bool> {
    match diff_check {
        RemoteManifestDiffCheck::InSync | RemoteManifestDiffCheck::SkippedDryRun => Ok(true),
        RemoteManifestDiffCheck::Drift(diff) => {
            println!(
                "\n⚠️  Remote cluster state is not aligned with {}.",
                yaml_path.display()
            );
            if diff.is_empty() {
                println!("kubectl diff detected changes but did not return a diff body.");
            } else {
                println!("{}", diff);
            }

            policy.confirm(
                "approval to deploy despite remote drift",
                "Do you want to continue with deployment despite this drift?",
                false,
                "Align the cluster with the manifest before deploying, or inspect the drift with --dry-run.",
            )
        }
        RemoteManifestDiffCheck::CheckFailed(message) => {
            println!("\n⚠️  Could not verify remote manifest alignment.");
            println!("{}", message);

            policy.confirm(
                "approval to deploy without the remote alignment check",
                "Do you want to continue without the remote alignment check?",
                false,
                "Make `kubectl diff` succeed against the target context before deploying.",
            )
        }
    }
}

/// Guards a deployment to a protected environment.
///
/// Interactively the guard is typing the environment name back. Unattended it
/// is `--confirm-env`, an explicit opt-in the caller has to spell out, rather
/// than a prompt that a non-TTY run would simply skip past.
fn confirm_protected_environment(
    env_name: &str,
    confirm_env: Option<&str>,
    dry_run: bool,
    policy: PromptPolicy,
) -> Result<()> {
    println!("⚠️  WARNING: Deployment to {} is PROTECTED!", env_name);

    if dry_run {
        println!("Dry-run: skipping the protected environment confirmation.");
        return Ok(());
    }

    let confirmation = match confirm_env {
        Some(value) => value.to_string(),
        None if policy.is_interactive() => Text::new(&format!(
            "Type the environment name '{}' to confirm:",
            env_name
        ))
        .prompt()
        .context("Production confirmation was cancelled")?,
        None => {
            return Err(policy.cannot_ask(
                "confirmation of a protected environment",
                &format!(
                    "Pass --confirm-env {} to confirm the deployment explicitly.",
                    env_name
                ),
            ));
        }
    };

    if confirmation != env_name {
        return Err(anyhow::anyhow!(
            "Confirmation '{}' does not match the target environment '{}'. Deployment aborted.",
            confirmation,
            env_name
        ));
    }

    Ok(())
}

/// Offers to undo the local manifest edit after a failed deploy.
///
/// This runs on an error path, so a missing terminal must not replace the
/// original failure with a prompt failure: the edit is reported and left in
/// place for the caller to deal with.
fn offer_revert(
    yaml_path: &Path,
    original_content: &str,
    auto_continue: bool,
    policy: PromptPolicy,
) -> Result<()> {
    if auto_continue {
        return Ok(());
    }

    if !policy.is_interactive() {
        println!(
            "Local YAML at {} still holds the updated tag; revert it manually if needed.",
            yaml_path.display()
        );
        return Ok(());
    }

    if Confirm::new("Revert local YAML changes?")
        .with_default(true)
        .prompt()?
    {
        fs::write(yaml_path, original_content)?;
        println!("YAML reverted.");
    }

    Ok(())
}

fn resolve_tag(
    env: &Environment,
    service: &ServiceSource,
    input: Option<String>,
    wait_for_tag: Option<String>,
    policy: PromptPolicy,
) -> Result<String> {
    if let Some(tag) = wait_for_tag {
        return wait_for_exact_tag(env, service, tag, policy);
    }

    if let Some(tag) = input {
        let images = fetch_service_images(env, service, true)?;
        let available_tags = collect_available_tags(&images);

        if available_tags.is_empty() {
            return Err(anyhow::anyhow!(
                "No images found for service {}",
                service.name
            ));
        }

        return resolve_from_list("Image tag", &available_tags, tag, policy);
    }

    let images = fetch_service_images(env, service, true)?;
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

    let selection = policy.select(
        "an image tag",
        "Select Image Tag:",
        options,
        &format!(
            "Pass --tag with one of:\n{}",
            format_candidates(&available_tags)
        ),
    )?;

    let tag = selection
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(',')
        .to_string();
    Ok(tag)
}

fn fetch_service_images(
    env: &Environment,
    service: &ServiceSource,
    announce: bool,
) -> Result<Vec<ImageMetadata>> {
    let project = env.gcp_project.as_deref().unwrap_or("MOCK_PROJECT");

    if announce {
        println!(
            "Fetching images for {} using path {}...",
            service.name, service.image_path
        );
    }

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

fn wait_for_exact_tag(
    env: &Environment,
    service: &ServiceSource,
    tag: String,
    policy: PromptPolicy,
) -> Result<String> {
    let mut attempt = 1;

    println!(
        "Waiting for image tag '{}' for service '{}'.",
        tag, service.name
    );
    println!("Registry image: {}", service.image_path);
    println!(
        "Polling every {} seconds.{}",
        TAG_RETRY_INTERVAL.as_secs(),
        if policy.is_interactive() {
            " Press 'q' to cancel."
        } else {
            ""
        }
    );

    loop {
        if policy.is_interactive() {
            render_tag_wait_status(&tag, &service.name, attempt, "Checking registry", None, '.')?;
        }

        let images = fetch_service_images(env, service, false)?;
        let available_tags = collect_available_tags(&images);

        if available_tags.iter().any(|available| available == &tag) {
            if policy.is_interactive() {
                clear_tag_wait_status_line()?;
            }
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

        wait_for_next_tag_check(service, &tag, attempt, policy)?;
        attempt += 1;
    }
}

fn wait_for_next_tag_check(
    service: &ServiceSource,
    tag: &str,
    attempt: usize,
    policy: PromptPolicy,
) -> Result<()> {
    // Raw mode and the 'q' shortcut both need a terminal; without one, just wait.
    if !policy.is_interactive() {
        println!(
            "Tag '{}' not available yet (check {}). Retrying in {} seconds.",
            tag,
            attempt,
            TAG_RETRY_INTERVAL.as_secs()
        );
        thread::sleep(TAG_RETRY_INTERVAL);
        return Ok(());
    }

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

        render_tag_wait_status(
            tag,
            &service.name,
            attempt,
            "Tag not available yet",
            Some((minutes, seconds)),
            spinner[frame % spinner.len()],
        )?;

        if event::poll(TAG_WAIT_POLL_INTERVAL)? {
            loop {
                if let Event::Key(key) = event::read()?
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            clear_tag_wait_status_line()?;
                            return Err(anyhow::anyhow!(TAG_WAIT_CANCELLED_MESSAGE));
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

    clear_tag_wait_status_line()?;
    Ok(())
}

fn render_tag_wait_status(
    _tag: &str,
    _service_name: &str,
    attempt: usize,
    phase: &str,
    remaining: Option<(u64, u64)>,
    marker: char,
) -> Result<()> {
    clear_tag_wait_status_line()?;

    let status = match remaining {
        Some((minutes, seconds)) => {
            format!(
                "{} {} | check {} | next {:02}:{:02} | q cancel",
                marker, phase, attempt, minutes, seconds
            )
        }
        None => {
            format!(
                "{} {} | completed {} | q cancel",
                marker,
                phase,
                attempt.saturating_sub(1)
            )
        }
    };

    let terminal_width = terminal_size()
        .map(|(width, _)| width as usize)
        .unwrap_or(120);
    print!("{}", truncate_for_terminal_width(&status, terminal_width));

    io::stdout().flush()?;
    Ok(())
}

fn clear_tag_wait_status_line() -> Result<()> {
    crossterm::execute!(
        io::stdout(),
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )
    .context("Failed to refresh tag wait status line")?;
    Ok(())
}

fn truncate_for_terminal_width(input: &str, width: usize) -> String {
    let safe_width = width.saturating_sub(1);
    let char_count = input.chars().count();
    if char_count <= safe_width {
        return input.to_string();
    }

    if safe_width == 0 {
        return String::new();
    }

    if safe_width <= 3 {
        return ".".repeat(safe_width);
    }

    let mut truncated = String::new();
    for ch in input.chars().take(safe_width - 3) {
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()
            .context("Failed to enable terminal raw mode while waiting for image tag")?;
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
fn resolve_from_list(
    label: &str,
    items: &[String],
    input: String,
    policy: PromptPolicy,
) -> Result<String> {
    // 1. Exact match
    if items.contains(&input) {
        return Ok(input);
    }

    // 2. Partial matches
    let matches: Vec<&String> = items.iter().filter(|&i| i.contains(&input)).collect();
    let noun = label.to_lowercase();
    let decision = format!("{} {}", article_for(&noun), noun);

    match matches.len() {
        0 => {
            if policy.is_interactive() {
                println!("No {} matches '{}'.", noun, input);
            }
            policy.select(
                &decision,
                &format!("Select {}:", label),
                items.to_vec(),
                &format!(
                    "'{}' matches no {}. Valid values:\n{}",
                    input,
                    noun,
                    format_candidates(items)
                ),
            )
        }
        // A single fuzzy match is a guess, so it is offered rather than applied.
        // Without a human to confirm it, resolving it silently could target the
        // wrong service after a rename, so the exact value is demanded instead.
        1 => {
            let suggest = matches[0].clone();
            if !policy.is_interactive() {
                return Err(policy.cannot_ask(
                    &decision,
                    &format!(
                        "'{}' is not an exact {}; the closest match is '{}'. Pass it verbatim.",
                        input, noun, suggest
                    ),
                ));
            }

            if Confirm::new(&format!("Did you mean '{}'?", suggest))
                .with_default(true)
                .prompt()?
            {
                Ok(suggest)
            } else {
                Select::new(&format!("Select {}:", label), items.to_vec())
                    .prompt()
                    .context(format!("{} selection was cancelled", label))
            }
        }
        _ => policy.select(
            &decision,
            &format!("Multiple matches for '{}'. Select {}:", input, noun),
            matches.iter().map(|m| (*m).clone()).collect(),
            &format!(
                "'{}' matches {} entries:\n{}\nPass one of them verbatim.",
                input,
                matches.len(),
                format_candidates(&matches)
            ),
        ),
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
            helm: None,
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
            helm: None,
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
            helm: None,
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
            helm: None,
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
            helm: None,
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
    fn test_deploy_auto_apply_accepts_flag() {
        let parse = Cli::try_parse_from(["davit", "deploy", "--auto-apply"]);
        assert!(parse.is_ok());
    }

    /// Non-interactive resolution never falls through to a prompt, and says why.
    fn blocked() -> PromptPolicy {
        PromptPolicy::new(true, true)
    }

    #[test]
    fn test_decision_labels_read_grammatically() {
        assert_eq!(article_for("environment"), "an");
        assert_eq!(article_for("image tag"), "an");
        assert_eq!(article_for("service"), "a");

        // The label reaches the user inside the error, so check it end to end.
        let items = vec!["v1.2.3".to_string(), "v1.2.4".to_string()];
        let err = resolve_from_list("Image tag", &items, "v1.2".to_string(), blocked())
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Cannot ask for an image tag:"), "{err}");
    }

    #[test]
    fn test_resolve_from_list_accepts_an_exact_match_without_prompting() {
        let items = vec!["svc-api (no-namespace)".to_string(), "billing".to_string()];
        let resolved =
            resolve_from_list("Service", &items, "billing".to_string(), blocked()).unwrap();
        assert_eq!(resolved, "billing");
    }

    #[test]
    fn test_resolve_from_list_lists_candidates_when_ambiguous() {
        let items = vec![
            "svc-api (no-namespace)".to_string(),
            "svc-api (tenant-b)".to_string(),
            "billing".to_string(),
        ];
        let err = resolve_from_list("Service", &items, "svc-api".to_string(), blocked())
            .unwrap_err()
            .to_string();

        assert!(err.contains("Cannot ask for a service"), "{err}");
        assert!(err.contains("'svc-api' matches 2 entries"), "{err}");
        assert!(err.contains("  - svc-api (no-namespace)"), "{err}");
        assert!(err.contains("  - svc-api (tenant-b)"), "{err}");
        assert!(!err.contains("billing"), "{err}");
    }

    #[test]
    fn test_resolve_from_list_refuses_to_guess_a_single_fuzzy_match() {
        let items = vec!["billing".to_string()];
        let err = resolve_from_list("Service", &items, "billin".to_string(), blocked())
            .unwrap_err()
            .to_string();

        assert!(err.contains("the closest match is 'billing'"), "{err}");
    }

    #[test]
    fn test_resolve_from_list_lists_valid_values_when_nothing_matches() {
        let items = vec!["preprod".to_string(), "production".to_string()];
        let err = resolve_from_list("Environment", &items, "staging".to_string(), blocked())
            .unwrap_err()
            .to_string();

        assert!(err.contains("matches no environment"), "{err}");
        assert!(err.contains("  - preprod"), "{err}");
        assert!(err.contains("  - production"), "{err}");
    }

    #[test]
    fn test_protected_environment_is_not_confirmed_in_dry_run() {
        assert!(confirm_protected_environment("production", None, true, blocked()).is_ok());
    }

    #[test]
    fn test_protected_environment_accepts_a_matching_confirm_env() {
        assert!(
            confirm_protected_environment("production", Some("production"), false, blocked())
                .is_ok()
        );
    }

    #[test]
    fn test_protected_environment_rejects_a_mismatched_confirm_env() {
        let err = confirm_protected_environment("production", Some("preprod"), false, blocked())
            .unwrap_err()
            .to_string();

        assert!(err.contains("'preprod' does not match"), "{err}");
        assert!(err.contains("'production'"), "{err}");
    }

    #[test]
    fn test_protected_environment_demands_confirm_env_when_unattended() {
        let err = confirm_protected_environment("production", None, false, blocked())
            .unwrap_err()
            .to_string();

        assert!(err.contains("Pass --confirm-env production"), "{err}");
    }

    /// A manifest per entry, so the environment sees colliding service names.
    fn env_with_manifests(dir: &std::path::Path, manifests: &[(&str, &str)]) -> Environment {
        for (file, body) in manifests {
            std::fs::write(dir.join(file), body).unwrap();
        }

        Environment {
            name: "preprod".to_string(),
            env_yaml_dir: dir.to_path_buf(),
            env_yaml_dir_extra: Default::default(),
            kubectl_context: "ctx".to_string(),
            gcp_project: None,
            protected: None,
            deployment_driver: config::DeploymentDriver::Manifest,
            helm_cluster: None,
        }
    }

    fn manifest(name: &str, namespace: Option<&str>) -> String {
        let namespace = namespace
            .map(|ns| format!("\n  namespace: {ns}"))
            .unwrap_or_default();
        format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {name}{namespace}\nspec:\n  template:\n    spec:\n      containers:\n      - name: {name}\n        image: gcr.io/p/{name}:v1\n"
        )
    }

    #[test]
    fn test_list_services_in_namespace_is_ordered_by_name_then_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(
            dir.path(),
            &[
                ("c.yml", &manifest("svc-api", Some("tenant-b"))),
                ("a.yml", &manifest("billing", Some("core"))),
                ("b.yml", &manifest("svc-api", None)),
            ],
        );

        let services = list_services_in_namespace(&env, None).unwrap();
        let ordered: Vec<(&str, Option<&str>)> = services
            .iter()
            .map(|s| (s.name.as_str(), s.namespace.as_deref()))
            .collect();

        assert_eq!(
            ordered,
            vec![
                ("billing", Some("core")),
                ("svc-api", None),
                ("svc-api", Some("tenant-b")),
            ]
        );
    }

    #[test]
    fn test_list_services_in_namespace_filters_and_disambiguates() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(
            dir.path(),
            &[
                ("a.yml", &manifest("svc-api", Some("tenant-b"))),
                ("b.yml", &manifest("svc-api", None)),
            ],
        );

        let services = list_services_in_namespace(&env, Some("tenant-b")).unwrap();
        assert_eq!(services.len(), 1);
        // Once narrowed the name no longer collides, so --service takes it plain.
        assert_eq!(get_service_display_name(&services[0], &services), "svc-api");
    }

    #[test]
    fn test_list_services_in_namespace_selects_manifests_declaring_none() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(
            dir.path(),
            &[
                ("a.yml", &manifest("svc-api", Some("tenant-b"))),
                ("b.yml", &manifest("svc-api", None)),
                ("c.yml", &manifest("reports", None)),
            ],
        );

        let services = list_services_in_namespace(&env, Some(NO_NAMESPACE)).unwrap();
        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(names, vec!["reports", "svc-api"]);
        assert!(services.iter().all(|s| s.namespace.is_none()));
    }

    /// The NAMESPACE column and the --namespace filter must not drift apart.
    #[test]
    fn test_every_listed_namespace_is_accepted_by_the_filter() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(
            dir.path(),
            &[
                ("a.yml", &manifest("svc-api", Some("tenant-b"))),
                ("b.yml", &manifest("svc-api", None)),
                ("c.yml", &manifest("billing", Some("core"))),
            ],
        );

        for service in list_services_in_namespace(&env, None).unwrap() {
            let printed = namespace_label(&service).to_string();
            assert!(
                list_services_in_namespace(&env, Some(&printed)).is_ok(),
                "listing printed '{printed}', which --namespace rejects"
            );
        }
    }

    #[test]
    fn test_list_services_in_namespace_reports_no_undeclared_namespace_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(dir.path(), &[("a.yml", &manifest("billing", Some("core")))]);

        let err = list_services_in_namespace(&env, Some(NO_NAMESPACE))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "No services without a declared namespace found");
    }

    #[test]
    fn test_strip_namespace_suffix_accepts_both_spellings() {
        let cases = [
            // (printed by `list services`, --namespace, expected)
            ("svc-api (tenant-b)", Some("tenant-b"), "svc-api"),
            ("svc-api (no-namespace)", Some(NO_NAMESPACE), "svc-api"),
            // Already plain, or no --namespace to narrow by: left alone.
            ("svc-api", Some("tenant-b"), "svc-api"),
            ("svc-api (tenant-b)", None, "svc-api (tenant-b)"),
            // A suffix naming a different namespace is not the one we narrowed to.
            ("svc-api (tenant-b)", Some("core"), "svc-api (tenant-b)"),
            // The three-part form stays ambiguous inside the narrowed set too.
            (
                "svc-api (core) [main]/a.yml",
                Some("core"),
                "svc-api (core) [main]/a.yml",
            ),
        ];

        for (value, namespace, expected) in cases {
            assert_eq!(
                strip_namespace_suffix(value.to_string(), namespace),
                expected,
                "{value} with --namespace {namespace:?}"
            );
        }
    }

    /// Rows are (display name, anything); the payload is irrelevant to the width.
    fn named(names: &[&str]) -> Vec<(String, ())> {
        names.iter().map(|n| ((*n).to_string(), ())).collect()
    }

    #[test]
    fn test_service_column_fits_the_widest_name() {
        assert_eq!(
            service_column_width(&named(&["billing", "svc-api (tenant-b)"])),
            18
        );
    }

    #[test]
    fn test_service_column_never_shrinks_below_its_header() {
        assert_eq!(service_column_width(&named(&["a", "bc"])), "SERVICE".len());
        assert_eq!(service_column_width::<()>(&[]), "SERVICE".len());
    }

    #[test]
    fn test_service_column_ignores_names_past_the_cap() {
        let outlier = "x".repeat(MAX_SERVICE_COLUMN + 60);
        let width = service_column_width(&named(&["svc-api (tenant-b)", &outlier]));

        // The outlier overflows its own row rather than padding every other one.
        assert_eq!(width, 18);
    }

    #[test]
    fn test_service_column_falls_back_to_the_cap_when_all_names_overflow() {
        let long = "x".repeat(MAX_SERVICE_COLUMN + 1);
        assert_eq!(
            service_column_width(&named(&[&long, &long])),
            "SERVICE".len()
        );
    }

    #[test]
    fn test_version_flag_is_available() {
        // `Cli` is not Debug, so unwrap_err() is not available here.
        let Err(err) = Cli::try_parse_from(["davit", "--version"]) else {
            panic!("--version should short-circuit parsing");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn test_namespace_accepts_the_bare_sentinel() {
        let parsed =
            Cli::try_parse_from(["davit", "list", "services", "--namespace", NO_NAMESPACE])
                .unwrap();
        match parsed.command {
            Commands::List {
                command: ListCommands::Services { namespace, .. },
            } => assert_eq!(namespace.as_deref(), Some(NO_NAMESPACE)),
            _ => panic!("expected list services"),
        }
    }

    #[test]
    fn test_list_services_in_namespace_rejects_an_empty_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(dir.path(), &[("a.yml", &manifest("billing", Some("core")))]);

        let err = list_services_in_namespace(&env, Some("absent"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("No services found in namespace 'absent'"),
            "{err}"
        );
    }

    #[test]
    fn test_no_fetch_skips_the_refresh_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_manifests(dir.path(), &[("a.yml", &manifest("billing", Some("core")))]);

        // The directory is not a git repository, so a refresh fails and, with
        // nobody to confirm the failure, aborts. --no-fetch never gets there.
        assert!(pull_yaml_sources(&env, false, false, "listing", blocked()).is_err());
        assert!(pull_yaml_sources(&env, false, true, "listing", blocked()).is_ok());
    }

    #[test]
    fn test_pull_source_skips_a_dirty_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        // Untracked content is enough to make the working copy dirty.
        std::fs::write(root.join("manifest.yml"), "kind: Deployment\n").unwrap();

        let source = YamlSource {
            name: "main".to_string(),
            root: root.to_path_buf(),
        };

        let report = pull_source(&source, false).unwrap();
        assert!(report.success);
        assert!(
            report.stdout.contains("uncommitted changes"),
            "{}",
            report.stdout
        );
    }

    #[test]
    fn test_pull_source_does_not_inspect_the_working_copy_in_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let source = YamlSource {
            name: "main".to_string(),
            root: dir.path().to_path_buf(),
        };

        // Not a repository at all: reaching git status would fail.
        let report = pull_source(&source, true).unwrap();
        assert!(report.stdout.contains("Dry-run"), "{}", report.stdout);
    }

    #[test]
    fn test_no_fetch_is_accepted_by_every_reading_command() {
        assert!(Cli::try_parse_from(["davit", "deploy", "--no-fetch"]).is_ok());
        assert!(Cli::try_parse_from(["davit", "info", "--no-fetch"]).is_ok());
        assert!(Cli::try_parse_from(["davit", "list", "services", "--no-fetch"]).is_ok());
    }

    #[test]
    fn test_deploy_accepts_a_namespace() {
        let parse = Cli::try_parse_from([
            "davit",
            "deploy",
            "--service",
            "svc-api",
            "--namespace",
            "tenant-b",
        ]);
        assert!(parse.is_ok());
    }

    #[test]
    fn test_list_subcommands_parse() {
        assert!(Cli::try_parse_from(["davit", "list", "envs"]).is_ok());
        assert!(Cli::try_parse_from(["davit", "list", "services", "--env", "preprod"]).is_ok());
    }

    #[test]
    fn test_non_interactive_is_a_global_flag() {
        assert!(
            Cli::try_parse_from(["davit", "deploy", "--non-interactive"])
                .unwrap()
                .non_interactive
        );
        assert!(
            Cli::try_parse_from(["davit", "--non-interactive", "info"])
                .unwrap()
                .non_interactive
        );
    }

    #[test]
    fn test_classify_kubectl_diff_result_in_sync() {
        assert_eq!(
            classify_kubectl_diff_result(Some(0), b"", b""),
            RemoteManifestDiffCheck::InSync
        );
    }

    #[test]
    fn test_classify_kubectl_diff_result_detects_drift() {
        assert_eq!(
            classify_kubectl_diff_result(Some(1), b"diff body\n", b""),
            RemoteManifestDiffCheck::Drift("diff body".to_string())
        );
    }

    #[test]
    fn test_classify_kubectl_diff_result_combines_stdout_and_stderr_on_failure() {
        assert_eq!(
            classify_kubectl_diff_result(Some(2), b"stdout\n", b"stderr\n"),
            RemoteManifestDiffCheck::CheckFailed(
                "kubectl diff exited with status 2.\nstdout\nstderr".to_string()
            )
        );
    }

    #[test]
    fn test_classify_kubectl_diff_result_handles_missing_output() {
        assert_eq!(
            classify_kubectl_diff_result(Some(3), b"", b""),
            RemoteManifestDiffCheck::CheckFailed(
                "kubectl diff exited with status 3.\nNo output returned by kubectl diff."
                    .to_string()
            )
        );
    }

    #[test]
    fn test_truncate_for_terminal_width_truncates_with_ellipsis() {
        assert_eq!(
            truncate_for_terminal_width("1234567890", 8),
            "1234...".to_string()
        );
    }

    #[test]
    fn test_truncate_for_terminal_width_keeps_short_message() {
        assert_eq!(
            truncate_for_terminal_width("short", 10),
            "short".to_string()
        );
    }

    #[test]
    fn test_truncate_for_terminal_width_reserves_last_terminal_column() {
        let rendered = truncate_for_terminal_width("1234567890", 10);
        assert_eq!(rendered.chars().count(), 9);
        assert_eq!(rendered, "123456...".to_string());
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
