use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::apply::{Manifest, Resource, documents, resources};
use crate::config::YamlSource;
use crate::git::Git;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Release {
    pub name: String,
    pub namespace: String,
    pub repo: PathBuf,
    pub application: PathBuf,
    pub chart: PathBuf,
    pub values: Vec<PathBuf>,
}

impl Release {
    pub fn includes(&self, path: &Path) -> bool {
        path.starts_with(&self.chart) || self.values.iter().any(|v| v == path)
    }

    pub fn label(&self) -> String {
        format!("Helm {} ({})", self.name, self.namespace)
    }
}

/// Read release metadata independently of the primary image/registry discovery.
pub fn discover(source: &YamlSource) -> Result<Vec<Release>> {
    let root = source.root.canonicalize()?;
    let repo = Git::repo_root(&root).context("Helm source must belong to a Git repository")?;
    let apps = root
        .join("clusters")
        .join(
            source
                .helm_cluster
                .as_deref()
                .context("Missing helm_cluster")?,
        )
        .join("apps");
    let mut releases = Vec::new();
    for entry in fs::read_dir(&apps).with_context(|| format!("Cannot read {}", apps.display()))? {
        let path = entry?.path();
        if !matches!(
            path.extension().and_then(|v| v.to_str()),
            Some("yaml" | "yml")
        ) {
            continue;
        }
        let applications = match fs::read_to_string(&path)
            .with_context(|| format!("Cannot read {}", path.display()))
            .and_then(|content| documents(&content))
        {
            Ok(applications) => applications,
            Err(error) => {
                eprintln!("Skipping {}: {error:#}", path.display());
                continue;
            }
        };
        for application in applications {
            match discover_application(&application, &path, &root, &repo) {
                Ok(Some(release)) => releases.push(release),
                Ok(None) => {}
                Err(error) => eprintln!("Skipping {}: {error:#}", path.display()),
            }
        }
    }
    releases.sort();
    Ok(releases)
}

fn discover_application(
    application: &serde_yaml::Value,
    path: &Path,
    root: &Path,
    repo: &Path,
) -> Result<Option<Release>> {
    if application["kind"].as_str() != Some("Application") {
        return Ok(None);
    }
    let sources = application["spec"]["sources"]
        .as_sequence()
        .context("Helm apply requires Application spec.sources")?;
    let charts: Vec<_> = sources
        .iter()
        .filter(|s| s["path"].as_str().is_some())
        .collect();
    if charts.is_empty()
        && let Some(chart) = sources.iter().find_map(|source| source["chart"].as_str())
    {
        anyhow::bail!(
            "Remote Helm chart '{chart}' is not supported by davit apply; only local charts are selectable"
        );
    }
    anyhow::ensure!(
        charts.len() == 1,
        "Application {} must reference exactly one local chart",
        path.display()
    );
    let chart_source = charts[0];
    anyhow::ensure!(
        chart_source["helm"]["parameters"].is_null()
            && chart_source["helm"]["values"].is_null()
            && chart_source["helm"]["valuesObject"].is_null(),
        "Application {} uses inline Helm overrides; apply requires file-based values",
        path.display()
    );
    let chart = safe_path(
        root,
        chart_source["path"]
            .as_str()
            .context("Missing chart path")?,
    )?;
    let files = chart_source["helm"]["valueFiles"]
        .as_sequence()
        .context("Missing Helm valueFiles")?;
    let mut values = Vec::new();
    for file in files {
        let file = file.as_str().context("Invalid Helm valueFiles entry")?;
        let value = if let Some(relative) = file.strip_prefix("$values/") {
            safe_path(root, relative)?
        } else {
            anyhow::ensure!(
                !file.starts_with('$'),
                "Unsupported Helm values reference {file}"
            );
            safe_path(&chart, file)?
        };
        anyhow::ensure!(
            value.starts_with(repo),
            "Helm values escape their repository"
        );
        values.push(value);
    }
    let name = chart_source["helm"]["releaseName"]
        .as_str()
        .or_else(|| application["metadata"]["name"].as_str())
        .context("Missing Helm release name")?;
    Ok(Some(Release {
        name: name.into(),
        namespace: application["spec"]["destination"]["namespace"]
            .as_str()
            .unwrap_or("default")
            .into(),
        repo: repo.to_path_buf(),
        application: path.canonicalize()?,
        chart,
        values,
    }))
}

fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = root
        .join(relative)
        .canonicalize()
        .with_context(|| format!("Cannot resolve Helm path {relative}"))?;
    anyhow::ensure!(
        path.starts_with(root),
        "Helm path escapes its source: {relative}"
    );
    Ok(path)
}

pub struct Prepared {
    pub operations: Vec<Manifest>,
    // Keep snapshots alive throughout validation, upgrades, monitoring, and commits.
    _snapshots: Vec<tempfile::TempDir>,
}

fn archive(repo: &Path) -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "HEAD"])
        .output()?;
    anyhow::ensure!(output.status.success(), "Cannot snapshot Helm repository");
    let mut tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(directory.path())
        .stdin(Stdio::piped())
        .spawn()?;
    tar.stdin
        .take()
        .context("Cannot write Helm snapshot")?
        .write_all(&output.stdout)?;
    anyhow::ensure!(tar.wait()?.success(), "Cannot extract Helm snapshot");
    Ok(directory)
}

fn in_snapshot(path: &Path, repo: &Path, snapshot: &Path) -> Result<PathBuf> {
    Ok(snapshot.join(
        path.strip_prefix(repo)
            .context("Helm input is outside its Git repository")?,
    ))
}

fn relocated(release: &Release, snapshot: &Path) -> Result<Release> {
    let mut result = release.clone();
    result.chart = in_snapshot(&release.chart, &release.repo, snapshot)?;
    result.values = release
        .values
        .iter()
        .map(|p| in_snapshot(p, &release.repo, snapshot))
        .collect::<Result<_>>()?;
    for entry in walkdir::WalkDir::new(&result.chart) {
        let entry = entry.context("Cannot inspect Helm chart snapshot")?;
        anyhow::ensure!(
            !entry.file_type().is_symlink(),
            "Symlinks in Helm charts are unsupported"
        );
    }
    for path in &result.values {
        for ancestor in path.ancestors().take_while(|p| *p != snapshot) {
            anyhow::ensure!(
                !ancestor.is_symlink(),
                "Symlinks in Helm values are unsupported"
            );
        }
    }
    Ok(result)
}

fn helm_command(release: &Release, operation: &str) -> Command {
    let mut command = Command::new("helm");
    command.arg(operation);
    if operation == "template" {
        command.arg(&release.name);
    }
    command
        .arg(&release.chart)
        .args(["--namespace", &release.namespace]);
    for values in &release.values {
        command.arg("-f").arg(values);
    }
    command
}

fn render(release: &Release, lint: bool) -> Result<String> {
    if lint {
        let output = helm_command(release, "lint")
            .output()
            .context("Failed to execute helm lint")?;
        anyhow::ensure!(
            output.status.success(),
            "Helm lint failed for {} (output hidden to protect values)",
            release.label()
        );
    }
    let output = helm_command(release, "template")
        .output()
        .context("Failed to execute helm template")?;
    anyhow::ensure!(
        output.status.success(),
        "Helm template failed for {} (output hidden to protect values)",
        release.label()
    );
    Ok(String::from_utf8(output.stdout)?)
}

pub fn prepare(files: &[Manifest]) -> Result<Prepared> {
    let mut prepared = Prepared {
        operations: Vec::new(),
        _snapshots: Vec::new(),
    };
    let mut repositories = BTreeMap::new();
    for file in files.iter().filter(|f| !f.helm.is_empty()) {
        if repositories.contains_key(&file.repo) {
            continue;
        }
        let baseline = archive(&file.repo)?;
        let updated = archive(&file.repo)?;
        for selected in files.iter().filter(|s| s.repo == file.repo) {
            let destination = updated.path().join(&selected.relative);
            // Refuse tracked symlinks so overlay writes cannot escape the snapshot.
            for ancestor in destination.ancestors().take_while(|p| *p != updated.path()) {
                anyhow::ensure!(
                    !ancestor.is_symlink(),
                    "Symlinks in Helm snapshots are unsupported"
                );
            }
            fs::create_dir_all(destination.parent().context("Invalid selected path")?)?;
            fs::write(destination, &selected.content)?;
        }
        repositories.insert(
            file.repo.clone(),
            (baseline.path().to_path_buf(), updated.path().to_path_buf()),
        );
        prepared._snapshots.extend([baseline, updated]);
    }
    let mut seen = BTreeMap::new();
    for file in files {
        if file.helm.is_empty() {
            prepared.operations.push(file.clone());
            continue;
        }
        for release in &file.helm {
            let key = (release.name.clone(), release.namespace.clone());
            if let Some(previous) = seen.insert(key, release.clone()) {
                anyhow::ensure!(
                    previous == *release,
                    "Conflicting definitions for {}",
                    release.label()
                );
                continue;
            }
            let (baseline, updated) = &repositories[&release.repo];
            // Release routing comes from committed Application metadata, never unselected edits.
            let committed_application = in_snapshot(&release.application, &release.repo, baseline)?;
            anyhow::ensure!(
                fs::read_to_string(committed_application).ok()
                    == fs::read_to_string(&release.application).ok(),
                "Application {} has local changes; commit routing changes separately before Helm apply",
                release.application.display()
            );
            let before = relocated(release, baseline)?;
            let after = relocated(release, updated)?;
            let original = render(&before, false)?;
            let content = render(&after, true)?;
            let mut rendered_resources = resources(&content)?;
            for resource in &mut rendered_resources {
                if resource.namespace.is_empty() {
                    resource.namespace = release.namespace.clone();
                }
            }
            prepared.operations.push(Manifest {
                path: file.path.clone(),
                repo: file.repo.clone(),
                relative: file.relative.clone(),
                original,
                content,
                resources: rendered_resources,
                helm: vec![after],
                rendered: true,
            });
        }
    }
    Ok(prepared)
}

pub fn preview(file: &Manifest) -> Result<String> {
    let prepared = prepare(std::slice::from_ref(file))?;
    let mut text = String::from(
        "Rendered Helm diff for this file only. The final plan combines all selected inputs.\n",
    );
    for operation in &prepared.operations {
        text.push_str(&format!("\n{}\n", operation.helm[0].label()));
        text.push_str(&crate::apply::diff(operation)?);
    }
    Ok(text)
}

pub fn upgrade(release: &Release, context: &str) -> Result<()> {
    let mut command = Command::new("helm");
    command
        .args(["upgrade", "--install", &release.name])
        .arg(&release.chart)
        .args([
            "--kube-context",
            context,
            "--namespace",
            &release.namespace,
            "--atomic",
            "--wait",
            "--timeout",
            "300s",
        ]);
    for values in &release.values {
        command.arg("-f").arg(values);
    }
    let output = command.output().context("Failed to execute helm upgrade")?;
    anyhow::ensure!(
        output.status.success(),
        "Helm upgrade failed for {} (output hidden to protect values); automatic rollback was requested",
        release.label()
    );
    Ok(())
}

pub fn placeholders(releases: &[Release]) -> Vec<Resource> {
    releases
        .iter()
        .map(|r| Resource {
            kind: "HelmRelease".into(),
            name: r.name.clone(),
            namespace: r.namespace.clone(),
        })
        .collect()
}
