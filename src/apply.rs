use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::{DeploymentDriver, Environment};
use crate::git::Git;
use crate::prompt::PromptPolicy;

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    #[arg(short, long)]
    pub env: Option<String>,
    /// Select a modified manifest (repeat for multiple files, in application order)
    #[arg(short, long)]
    pub file: Vec<PathBuf>,
    /// Commit message; requested interactively when omitted
    #[arg(short, long)]
    pub message: Option<String>,
    /// Preview selected local changes and commands without changing Git or the cluster
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub no_fetch: bool,
    /// Approve the selected application plan and Git commit/push
    #[arg(short, long)]
    pub yes: bool,
    #[arg(long)]
    pub confirm_env: Option<String>,
    /// Restart dependent workloads after applying ConfigMaps/Secrets
    #[arg(long, value_enum, default_value = "ask")]
    pub restart: Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Restart {
    Ask,
    Skip,
    All,
}

fn kubectl(env: &Environment) -> Command {
    let mut command = Command::new("kubectl");
    command.args(["--context", &env.kubectl_context]);
    command
}

fn default_namespace(env: &Environment) -> Result<String> {
    let output = kubectl(env)
        .args(["config", "view", "--minify", "-o", "jsonpath={..namespace}"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "Cannot resolve the namespace for the configured context"
    );
    let namespace = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(if namespace.is_empty() {
        "default".into()
    } else {
        namespace
    })
}

fn select_files(
    manifests: &[Manifest],
    files: &[PathBuf],
    policy: PromptPolicy,
) -> Result<Vec<Manifest>> {
    if files.is_empty() {
        if !policy.is_interactive() {
            return Err(policy.cannot_ask(
                "modified YAML files",
                "Pass --file PATH for each manifest, in application order.",
            ));
        }
        return select_tui(manifests);
    }
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for file in files {
        let canonical = file.canonicalize().ok();
        let matches: Vec<_> = manifests
            .iter()
            .filter(|m| canonical.as_ref() == Some(&m.path) || file == &m.relative)
            .collect();
        anyhow::ensure!(
            matches.len() == 1,
            "File {} is not an unambiguous modified manifest in this environment",
            file.display()
        );
        if seen.insert(matches[0].path.clone()) {
            selected.push(matches[0].clone());
        }
    }
    Ok(selected)
}

pub fn run(env: &Environment, args: ApplyArgs, policy: PromptPolicy) -> Result<()> {
    let manifests = discover(env)?;
    if manifests.is_empty() {
        println!("No modified YAML manifests in {}.", env.name);
        return Ok(());
    }
    let mut selected = select_files(&manifests, &args.file, policy)?;
    if selected.is_empty() {
        println!("Application cancelled.");
        return Ok(());
    }
    println!(
        "Application plan for {} (context {}):",
        env.name, env.kubectl_context
    );
    for (index, manifest) in selected.iter().enumerate() {
        println!("{}. {}", index + 1, manifest.path.display());
        println!("{}", diff(manifest)?);
    }
    if args.dry_run {
        for manifest in &selected {
            println!(
                "Dry-run: kubectl --context {} apply --dry-run=server -f {}",
                env.kubectl_context,
                manifest.path.display()
            );
            println!(
                "Dry-run: kubectl --context {} apply -f {}",
                env.kubectl_context,
                manifest.path.display()
            );
        }
        println!(
            "Dry-run: would monitor workload rollouts, handle dependent restarts ({:?}), and commit/push selected files once per repository. No release tag.",
            args.restart
        );
        return Ok(());
    }
    let message = match args.message {
        Some(message) => message,
        None if policy.is_interactive() => inquire::Text::new("Commit message:").prompt()?,
        None => return Err(policy.cannot_ask("a commit message", "Pass --message MESSAGE.")),
    };
    anyhow::ensure!(!message.trim().is_empty(), "Commit message cannot be empty");
    let has_configuration = selected
        .iter()
        .any(|m| m.resources.iter().any(Resource::configuration));
    if has_configuration && args.restart == Restart::Ask && !policy.is_interactive() {
        return Err(policy.cannot_ask(
            "dependent workload restarts",
            "Pass --restart skip or --restart all.",
        ));
    }
    if !args.yes && !policy.is_interactive() {
        return Err(policy.cannot_ask(
            "approval of the application plan",
            "Pass --yes to approve the plan.",
        ));
    }
    let namespace = default_namespace(env)?;
    for manifest in &mut selected {
        for resource in &mut manifest.resources {
            if resource.namespace.is_empty() {
                resource.namespace = namespace.clone();
            }
        }
    }
    // Apply immutable snapshots; a later editor change must never be committed as deployed.
    let snapshots = tempfile::tempdir()?;
    let mut paths = Vec::new();
    for (index, manifest) in selected.iter().enumerate() {
        let path = snapshots.path().join(format!("{index}.yaml"));
        fs::write(&path, &manifest.content)?;
        validate_unchanged(manifest)?;
        let output = kubectl(env)
            .args(["apply", "--dry-run=server", "-f"])
            .arg(&path)
            .output()?;
        // kubectl may echo complete resources in errors: never expose Secret payloads.
        if !output.status.success() {
            let details = if manifest.resources.iter().any(|r| r.kind == "Secret") {
                "Secret validation output hidden to protect values".to_string()
            } else {
                String::from_utf8_lossy(&output.stderr).into_owned()
            };
            anyhow::bail!(
                "Validation failed for {}: {details}. Nothing applied.",
                manifest.path.display()
            );
        }
        paths.push(path);
    }
    for (manifest, path) in selected.iter().zip(&paths) {
        let output = kubectl(env).args(["diff", "-f"]).arg(path).output()?;
        anyhow::ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "Cluster diff failed for {} (output hidden because it may contain sensitive values). Nothing applied.",
            manifest.path.display()
        );
        if output.status.code() == Some(1) {
            println!("Cluster changes for {}:", manifest.path.display());
            if manifest.resources.iter().any(|r| r.kind == "Secret") {
                println!("Cluster diff hidden because this file contains Secrets.");
            } else {
                println!("{}", String::from_utf8_lossy(&output.stdout));
            }
        }
    }
    if !args.yes
        && !policy.confirm(
            "approval of the application plan",
            "Apply this plan, then commit and push the selected files?",
            false,
            "Pass --yes to approve the plan.",
        )?
    {
        println!("Application cancelled.");
        return Ok(());
    }
    let mut completed = Vec::new();
    let execution = (|| -> Result<()> {
        for (manifest, path) in selected.iter().zip(&paths) {
            validate_unchanged(manifest)?;
            println!("Applying {}", manifest.path.display());
            let output = kubectl(env).args(["apply", "-f"]).arg(path).output()?;
            anyhow::ensure!(
                output.status.success(),
                "Application failed for {} (command output hidden because it may contain sensitive values)",
                manifest.path.display()
            );
            completed.push(manifest.path.clone());
            for resource in manifest.resources.iter().filter(|r| r.workload()) {
                rollout(env, resource, false)?;
            }
        }
        if has_configuration && args.restart != Restart::Skip {
            restart_dependents(env, &selected, args.restart, policy)?;
        }
        for manifest in &selected {
            validate_unchanged(manifest)?;
        }
        let mut repos: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for manifest in &selected {
            repos
                .entry(manifest.repo.clone())
                .or_default()
                .push(manifest.relative.clone());
        }
        for (repo, files) in repos {
            Git::commit_files(&repo, &message, &files)?;
            Git::push(&repo)?;
            println!(
                "Committed and pushed {} selected file(s) in {}.",
                files.len(),
                repo.display()
            );
        }
        Ok(())
    })();
    if let Err(error) = execution {
        eprintln!("Sequence stopped. Successfully applied files:");
        for path in &completed {
            eprintln!("  {}", path.display());
        }
        eprintln!("Unapplied files:");
        for manifest in &selected {
            if !completed.contains(&manifest.path) {
                eprintln!(
                    "  {} (a failed apply may have changed some resources)",
                    manifest.path.display()
                );
            }
        }
        eprintln!(
            "Local edits are retained. Check cluster and Git state before retrying; no automatic rollback was performed."
        );
        return Err(error);
    }
    println!(
        "Applied {} file(s) successfully. Configuration saved without a release tag.",
        selected.len()
    );
    Ok(())
}

fn validate_unchanged(manifest: &Manifest) -> Result<()> {
    anyhow::ensure!(
        fs::read_to_string(&manifest.path)? == manifest.content,
        "{} changed after selection; stopping to avoid committing unapplied edits",
        manifest.path.display()
    );
    Ok(())
}

fn rollout(env: &Environment, resource: &Resource, restart: bool) -> Result<()> {
    if restart {
        let status = kubectl(env)
            .args([
                "-n",
                &resource.namespace,
                "rollout",
                "restart",
                &resource.target(),
            ])
            .status()?;
        anyhow::ensure!(status.success(), "Restart failed for {}", resource.target());
    }
    let status = kubectl(env)
        .args([
            "-n",
            &resource.namespace,
            "rollout",
            "status",
            &resource.target(),
            "--timeout=300s",
        ])
        .status()?;
    anyhow::ensure!(status.success(), "Rollout failed for {}", resource.target());
    Ok(())
}

fn restart_dependents(
    env: &Environment,
    manifests: &[Manifest],
    restart: Restart,
    policy: PromptPolicy,
) -> Result<()> {
    let configs: Vec<_> = manifests
        .iter()
        .flat_map(|m| &m.resources)
        .filter(|r| r.configuration())
        .cloned()
        .collect();
    let mut applied = BTreeSet::new();
    let last_config = manifests
        .iter()
        .rposition(|m| m.resources.iter().any(Resource::configuration))
        .unwrap_or(0);
    for (index, manifest) in manifests.iter().enumerate() {
        let old = documents(&manifest.original)?;
        let new = documents(&manifest.content)?;
        for resource in manifest.resources.iter().filter(|r| r.workload()) {
            let matches = |doc: &&Value| {
                doc["kind"].as_str() == Some(&resource.kind)
                    && doc["metadata"]["name"].as_str() == Some(&resource.name)
                    && doc["metadata"]["namespace"]
                        .as_str()
                        .unwrap_or(&resource.namespace)
                        == resource.namespace
            };
            let before = old.iter().find(matches);
            let after = new.iter().find(matches);
            if index >= last_config
                && before.map(|v| &v["spec"]["template"]) != after.map(|v| &v["spec"]["template"])
            {
                applied.insert(resource.clone());
            }
        }
    }
    if restart == Restart::Ask
        && !policy.confirm(
            "dependent workload restarts",
            "ConfigMaps/Secrets applied. Look for dependent workloads to restart?",
            false,
            "Pass --restart skip or --restart all.",
        )?
    {
        return Ok(());
    }
    let namespaces: BTreeSet<_> = configs.iter().map(|r| &r.namespace).collect();
    let mut candidates = Vec::new();
    for namespace in namespaces {
        let output = kubectl(env)
            .args([
                "get",
                "deployments,statefulsets,daemonsets",
                "-n",
                namespace,
                "-o",
                "json",
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Cannot discover dependent workloads in {namespace}; applied files remain uncommitted"
        );
        candidates.extend(dependent_workloads(
            &serde_json::from_slice(&output.stdout)?,
            &configs,
            &applied,
        ));
    }
    if candidates.is_empty() {
        println!(
            "No dependent workloads found to restart (workloads applied in this sequence are excluded)."
        );
        return Ok(());
    }
    let selected = if restart == Restart::All {
        candidates
    } else {
        inquire::MultiSelect::new(
            "Select dependent workloads to restart:",
            candidates
                .iter()
                .map(|r| format!("{} ({})", r.target(), r.namespace))
                .collect(),
        )
        .prompt()?
        .into_iter()
        .filter_map(|label| {
            candidates
                .iter()
                .find(|r| label == format!("{} ({})", r.target(), r.namespace))
                .cloned()
        })
        .collect()
    };
    for resource in selected {
        rollout(env, &resource, true)?;
    }
    Ok(())
}

/// A scoped terminal guard also restores the terminal on cancellation or errors.
struct SelectionTerminal;

impl Drop for SelectionTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn select_tui(manifests: &[Manifest]) -> Result<Vec<Manifest>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::CrosstermBackend,
        layout::{Constraint, Layout},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    };
    crossterm::terminal::enable_raw_mode()?;
    let _guard = SelectionTerminal;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut order: Vec<usize> = (0..manifests.len()).collect();
    let mut chosen = BTreeSet::new();
    let mut cursor = 0usize;
    let mut preview: Option<String> = None;
    let mut scroll = 0u16;
    loop {
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(frame.area());
            if let Some(diff) = &preview {
                frame.render_widget(Paragraph::new(diff.as_str()).scroll((scroll, 0)).block(Block::default().borders(Borders::ALL).title("Diff against Git HEAD (Secret values hidden)")), areas[0]);
                frame.render_widget(Paragraph::new("↑/↓ scroll · Esc/d back to selection"), areas[1]);
            } else {
                let items: Vec<_> = order.iter().map(|index| {
                    let manifest = &manifests[*index];
                    ListItem::new(format!("[{}] {} — {}", if chosen.contains(index) { "x" } else { " " }, manifest.path.display(), manifest.resources.iter().map(|r| format!("{}/{}", r.kind, r.name)).collect::<Vec<_>>().join(", ")))
                }).collect();
                let mut state = ListState::default().with_selected(Some(cursor));
                frame.render_stateful_widget(List::new(items).highlight_symbol("> ").block(Block::default().borders(Borders::ALL).title("Modified YAML — application order")), areas[0], &mut state);
                frame.render_widget(Paragraph::new("↑/↓ move · Space select · d diff · K/J reorder · Enter confirm · Esc cancel"), areas[1]);
            }
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Vec::new());
        }
        if preview.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('d') => {
                    preview = None;
                    scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => scroll = scroll.saturating_add(20),
                KeyCode::PageUp => scroll = scroll.saturating_sub(20),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(Vec::new()),
            KeyCode::Down | KeyCode::Char('j') => cursor = (cursor + 1).min(order.len() - 1),
            KeyCode::Up | KeyCode::Char('k') => cursor = cursor.saturating_sub(1),
            KeyCode::Char(' ') if !chosen.insert(order[cursor]) => {
                chosen.remove(&order[cursor]);
            }
            KeyCode::Char('d') => preview = Some(diff(&manifests[order[cursor]])?),
            KeyCode::Char('K') if cursor > 0 => {
                order.swap(cursor, cursor - 1);
                cursor -= 1;
            }
            KeyCode::Char('J') if cursor + 1 < order.len() => {
                order.swap(cursor, cursor + 1);
                cursor += 1;
            }
            KeyCode::Enter => {
                return Ok(order
                    .into_iter()
                    .filter(|index| chosen.contains(index))
                    .map(|index| manifests[index].clone())
                    .collect());
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub path: PathBuf,
    pub repo: PathBuf,
    pub relative: PathBuf,
    pub content: String,
    pub original: String,
    pub resources: Vec<Resource>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Resource {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

impl Resource {
    fn workload(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "Deployment" | "StatefulSet" | "DaemonSet"
        )
    }

    fn configuration(&self) -> bool {
        matches!(self.kind.as_str(), "ConfigMap" | "Secret")
    }

    fn target(&self) -> String {
        format!("{}/{}", self.kind.to_lowercase(), self.name)
    }
}

pub fn documents(content: &str) -> Result<Vec<Value>> {
    serde_yaml::Deserializer::from_str(content)
        .map(Value::deserialize)
        .filter(|r| !matches!(r, Ok(Value::Null)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Invalid YAML")
}

fn resources(content: &str) -> Result<Vec<Resource>> {
    let mut result = Vec::new();
    for doc in documents(content)? {
        anyhow::ensure!(doc["apiVersion"].as_str().is_some(), "Missing apiVersion");
        let kind = doc["kind"].as_str().context("Missing kind")?;
        anyhow::ensure!(
            kind != "List",
            "List manifests are unsupported; use separate YAML documents"
        );
        result.push(Resource {
            kind: kind.to_string(),
            name: doc["metadata"]["name"]
                .as_str()
                .context("Missing metadata.name")?
                .to_string(),
            namespace: doc["metadata"]["namespace"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        });
    }
    anyhow::ensure!(!result.is_empty(), "Empty manifest");
    Ok(result)
}

/// Hide the entire Secret payload, including annotations and custom fields.
pub fn redacted(content: &str) -> Result<String> {
    let mut output = Vec::new();
    for doc in documents(content)? {
        let safe = if doc["kind"].as_str() == Some("Secret") {
            serde_yaml::to_value(serde_json::json!({
                "apiVersion": doc["apiVersion"], "kind": "Secret",
                "metadata": {"name": doc["metadata"]["name"], "namespace": doc["metadata"]["namespace"]},
                "content": "[Secret content hidden]"
            }))?
        } else {
            doc
        };
        output.push(serde_yaml::to_string(&safe)?);
    }
    Ok(output.join("---\n"))
}

pub fn diff(manifest: &Manifest) -> Result<String> {
    let secret = manifest.resources.iter().any(|r| r.kind == "Secret")
        || documents(&manifest.original)?
            .iter()
            .any(|d| d["kind"].as_str() == Some("Secret"));
    let (old, new) = if secret {
        (redacted(&manifest.original)?, redacted(&manifest.content)?)
    } else {
        (manifest.original.clone(), manifest.content.clone())
    };
    let mut result = similar::TextDiff::from_lines(&old, &new)
        .unified_diff()
        .header("HEAD", &manifest.relative.to_string_lossy())
        .to_string();
    if secret {
        result
            .push_str("\nSecret content is hidden; payload changes may not appear in this diff.\n");
    }
    Ok(result)
}

pub fn discover(env: &Environment) -> Result<Vec<Manifest>> {
    let sources = env.yaml_sources();
    let excluded: Vec<PathBuf> = sources
        .iter()
        .filter(|s| s.driver != DeploymentDriver::Manifest)
        .filter_map(|s| s.root.canonicalize().ok())
        .collect();
    let mut paths = BTreeSet::new();
    let mut result = Vec::new();
    for source in sources {
        if source.driver != DeploymentDriver::Manifest {
            println!(
                "Skipping {}: apply currently supports manifest sources only.",
                source.name
            );
            continue;
        }
        let root = source
            .root
            .canonicalize()
            .with_context(|| format!("Cannot read {}", source.root.display()))?;
        let repo =
            Git::repo_root(&root).context("Manifest sources must belong to a Git repository")?;
        for args in [
            vec!["diff", "--name-only", "-z", "HEAD", "--"],
            vec!["ls-files", "--others", "--exclude-standard", "-z", "--"],
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .arg(&root)
                .output()?;
            anyhow::ensure!(
                output.status.success(),
                "Cannot discover modified manifests"
            );
            for name in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
                let relative =
                    PathBuf::from(std::str::from_utf8(name).context("Non-UTF8 manifest path")?);
                if !matches!(
                    relative.extension().and_then(|s| s.to_str()),
                    Some("yaml" | "yml")
                ) {
                    continue;
                }
                let path = repo.join(&relative);
                if !path.exists() {
                    println!(
                        "Skipping deleted YAML {} (apply does not delete resources).",
                        relative.display()
                    );
                    continue;
                }
                let canonical = path.canonicalize()?;
                anyhow::ensure!(
                    canonical.starts_with(&root),
                    "Manifest escapes its configured source: {}",
                    path.display()
                );
                anyhow::ensure!(
                    !path.is_symlink(),
                    "Symlink manifests are unsupported: {}",
                    path.display()
                );
                anyhow::ensure!(
                    canonical == path,
                    "Manifest paths through symlink directories are unsupported: {}",
                    path.display()
                );
                if excluded.iter().any(|p| canonical.starts_with(p))
                    || !paths.insert(canonical.clone())
                {
                    continue;
                }
                let content = fs::read_to_string(&canonical)?;
                let parsed = resources(&content)
                    .with_context(|| format!("Invalid manifest {}", path.display()))?;
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["show", &format!("HEAD:{}", relative.to_string_lossy())])
                    .output()?;
                let original = if output.status.success() {
                    String::from_utf8(output.stdout)?
                } else {
                    String::new()
                };
                result.push(Manifest {
                    path: canonical,
                    repo: repo.clone(),
                    relative,
                    content,
                    original,
                    resources: parsed,
                });
            }
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn references(value: &serde_json::Value, kind: &str, name: &str) -> bool {
    if let Some(map) = value.as_object() {
        for (key, child) in map {
            let field = match (kind, key.as_str()) {
                ("ConfigMap", "configMapRef" | "configMapKeyRef" | "configMap") => Some("name"),
                ("Secret", "secretRef" | "secretKeyRef" | "secret") => Some("name"),
                _ => None,
            };
            if field.is_some_and(|f| child[f].as_str() == Some(name))
                || (kind == "Secret"
                    && key == "secret"
                    && child["secretName"].as_str() == Some(name))
                || references(child, kind, name)
            {
                return true;
            }
        }
    }
    value
        .as_array()
        .is_some_and(|items| items.iter().any(|v| references(v, kind, name)))
}

fn dependent_workloads(
    items: &serde_json::Value,
    configs: &[Resource],
    applied: &BTreeSet<Resource>,
) -> Vec<Resource> {
    let mut result = BTreeSet::new();
    for item in items["items"].as_array().into_iter().flatten() {
        let Some(kind) = item["kind"].as_str() else {
            continue;
        };
        let Some(name) = item["metadata"]["name"].as_str() else {
            continue;
        };
        let namespace = item["metadata"]["namespace"].as_str().unwrap_or("default");
        let resource = Resource {
            kind: kind.into(),
            name: name.into(),
            namespace: namespace.into(),
        };
        if resource.workload()
            && !applied.contains(&resource)
            && configs.iter().any(|c| {
                c.namespace == namespace
                    && references(&item["spec"]["template"]["spec"], &c.kind, &c.name)
            })
        {
            result.insert(resource);
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.name", "Davit Test"]);
        git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repo.path().join("tracked.yaml"), configmap("old")).unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        repo
    }

    fn configmap(value: &str) -> String {
        format!(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: settings\ndata:\n  key: {value}\n"
        )
    }

    fn environment(root: &Path) -> Environment {
        toml::from_str(&format!(
            "name = 'test'\nkubectl_context = 'test'\nenv_yaml_dir = '{}'",
            root.display()
        ))
        .unwrap()
    }

    #[test]
    fn discovers_staged_unstaged_and_new_yaml_with_spaces_only_in_source() {
        let repo = repository();
        let root = repo.path().join("env");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("staged.yaml"), configmap("old")).unwrap();
        fs::write(root.join("unstaged.yml"), configmap("old")).unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "environment"]);
        fs::write(root.join("staged.yaml"), configmap("new")).unwrap();
        git(repo.path(), &["add", "env/staged.yaml"]);
        fs::write(root.join("unstaged.yml"), configmap("new")).unwrap();
        fs::write(root.join("new file.yaml"), configmap("new")).unwrap();
        fs::write(repo.path().join("tracked.yaml"), configmap("outside")).unwrap();
        fs::write(root.join("notes.txt"), "not a manifest").unwrap();
        let found = discover(&environment(&root)).unwrap();
        assert_eq!(found.len(), 3);
        assert!(
            found
                .iter()
                .any(|m| m.relative == Path::new("env/new file.yaml") && m.original.is_empty())
        );
        let chosen = select_files(
            &found,
            &[
                PathBuf::from("env/unstaged.yml"),
                PathBuf::from("env/staged.yaml"),
            ],
            PromptPolicy::new(true, false),
        )
        .unwrap();
        assert_eq!(chosen[0].relative, Path::new("env/unstaged.yml"));
    }

    #[test]
    fn secret_diff_hides_old_new_values_and_annotations_in_mixed_documents() {
        let secret = |value| {
            format!(
                "apiVersion: v1\nkind: Secret\nmetadata:\n  name: password\n  annotations:\n    private: {value}\nstringData:\n  password: {value}\n---\n{}",
                configmap("visible")
            )
        };
        let current = secret("new-secret");
        let manifest = Manifest {
            path: "secret.yaml".into(),
            repo: ".".into(),
            relative: "secret.yaml".into(),
            resources: resources(&current).unwrap(),
            content: current,
            original: secret("old-secret"),
        };
        let shown = diff(&manifest).unwrap();
        assert!(!shown.contains("new-secret"));
        assert!(!shown.contains("old-secret"));
        assert!(shown.contains("Secret content is hidden"));
        assert!(redacted(&manifest.content).unwrap().contains("visible"));
    }

    #[test]
    fn discovers_namespace_scoped_volume_env_and_projected_references() {
        let items = serde_json::json!({"items": [
            {"kind": "Deployment", "metadata": {"name": "env", "namespace": "test"}, "spec": {"template": {"spec": {"containers": [{"envFrom": [{"configMapRef": {"name": "settings"}}]}]}}}},
            {"kind": "StatefulSet", "metadata": {"name": "volume", "namespace": "test"}, "spec": {"template": {"spec": {"volumes": [{"secret": {"secretName": "password"}}]}}}},
            {"kind": "DaemonSet", "metadata": {"name": "projected", "namespace": "test"}, "spec": {"template": {"spec": {"volumes": [{"projected": {"sources": [{"secret": {"name": "password"}}]}}]}}}},
            {"kind": "Deployment", "metadata": {"name": "other-namespace", "namespace": "other"}, "spec": {"template": {"spec": {"containers": [{"envFrom": [{"configMapRef": {"name": "settings"}}]}]}}}}
        ]});
        let configs = vec![
            Resource {
                kind: "ConfigMap".into(),
                name: "settings".into(),
                namespace: "test".into(),
            },
            Resource {
                kind: "Secret".into(),
                name: "password".into(),
                namespace: "test".into(),
            },
        ];
        let found = dependent_workloads(&items, &configs, &BTreeSet::new());
        assert_eq!(found.len(), 3);
        let already_applied = BTreeSet::from([found[0].clone()]);
        assert_eq!(
            dependent_workloads(&items, &configs, &already_applied).len(),
            2
        );
    }

    #[test]
    fn commit_selected_files_preserves_unrelated_staging_and_supports_new_paths() {
        let repo = repository();
        fs::write(repo.path().join("tracked.yaml"), configmap("new")).unwrap();
        fs::write(repo.path().join("new file.yaml"), configmap("new")).unwrap();
        fs::write(repo.path().join("unrelated.txt"), "keep staged").unwrap();
        git(repo.path(), &["add", "unrelated.txt"]);
        Git::commit_files(
            repo.path(),
            "feat: apply config",
            &["tracked.yaml".into(), "new file.yaml".into()],
        )
        .unwrap();
        let committed = git(repo.path(), &["show", "--pretty=", "--name-only", "HEAD"]);
        let committed = String::from_utf8(committed.stdout).unwrap();
        assert!(committed.contains("tracked.yaml"));
        assert!(committed.contains("new file.yaml"));
        assert!(!committed.contains("unrelated.txt"));
        assert_eq!(
            String::from_utf8(git(repo.path(), &["diff", "--cached", "--name-only"]).stdout)
                .unwrap()
                .trim(),
            "unrelated.txt"
        );
    }

    #[test]
    fn refuses_files_edited_after_selection_and_symlinks_outside_source() {
        let repo = repository();
        fs::write(repo.path().join("tracked.yaml"), configmap("selected")).unwrap();
        let found = discover(&environment(repo.path())).unwrap();
        fs::write(repo.path().join("tracked.yaml"), configmap("later")).unwrap();
        assert!(validate_unchanged(&found[0]).is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("outside.yaml"), configmap("outside")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("outside.yaml"),
                repo.path().join("link.yaml"),
            )
            .unwrap();
            assert!(discover(&environment(repo.path())).is_err());
        }
    }
}
