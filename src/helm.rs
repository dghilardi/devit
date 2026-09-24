use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::config::{Environment, HelmServiceSource, ServiceSource};

const TAG_PATH_ANNOTATION: &str = "davit.io/image-tag-path";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedWorkload {
    pub name: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub selector: Option<String>,
    pub container_name: String,
}

#[derive(Debug)]
struct ImageCandidate {
    repository: String,
    tag: String,
    tag_path: Vec<String>,
    score: usize,
}

pub fn discover_helm_services(env: &Environment) -> Result<Vec<ServiceSource>> {
    let cluster = env
        .helm_cluster
        .as_deref()
        .context("helm_cluster is required for Helm environments")?;
    let repo_root = &env.env_yaml_dir;
    let apps_dir = repo_root.join("clusters").join(cluster).join("apps");
    if !apps_dir.exists() {
        return Err(anyhow::anyhow!(
            "Helm Application directory does not exist: {}",
            apps_dir.display()
        ));
    }

    let mut services = Vec::new();
    for entry in WalkDir::new(&apps_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            matches!(
                entry.path().extension().and_then(|v| v.to_str()),
                Some("yaml" | "yml")
            )
        })
    {
        match discover_application(repo_root, entry.path()) {
            Ok(Some(service)) => services.push(service),
            Ok(None) => {}
            Err(error) => eprintln!("Skipping {}: {error:#}", entry.path().display()),
        }
    }

    services.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(services)
}

fn discover_application(
    repo_root: &Path,
    application_path: &Path,
) -> Result<Option<ServiceSource>> {
    let application: Value = serde_yaml::from_str(
        &fs::read_to_string(application_path)
            .with_context(|| format!("Failed to read {}", application_path.display()))?,
    )?;
    if application.get("kind").and_then(Value::as_str) != Some("Application") {
        return Ok(None);
    }

    let name = string_at(&application, &["metadata", "name"])
        .context("Application has no metadata.name")?
        .to_string();
    let namespace =
        string_at(&application, &["spec", "destination", "namespace"]).map(ToString::to_string);
    let Some(sources) = application
        .get("spec")
        .and_then(|value| value.get("sources"))
        .and_then(Value::as_sequence)
    else {
        return Ok(None);
    };

    let Some(chart_source) = sources
        .iter()
        .find(|source| source.get("path").and_then(Value::as_str).is_some())
    else {
        return Ok(None);
    };
    let chart_relative = chart_source
        .get("path")
        .and_then(Value::as_str)
        .context("Chart source has no path")?;
    let value_file = chart_source
        .get("helm")
        .and_then(|value| value.get("valueFiles"))
        .and_then(Value::as_sequence)
        .and_then(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .find(|file| file.starts_with("$values/"))
        })
        .context("Chart source has no local $values value file")?;

    let chart_path = repo_root.join(chart_relative);
    let values_path = repo_root.join(value_file.trim_start_matches("$values/"));
    let defaults_path = chart_path.join("values.yaml");
    let mut merged = read_yaml_or_empty(&defaults_path)?;
    let overrides = read_yaml_or_empty(&values_path)?;
    merge_yaml(&mut merged, &overrides);

    let explicit_tag_path = string_at(
        &application,
        &["metadata", "annotations", TAG_PATH_ANNOTATION],
    )
    .map(|path| path.split('.').map(ToString::to_string).collect::<Vec<_>>());
    let candidate = match select_image_candidate(&merged, explicit_tag_path.as_deref(), &name) {
        Ok(candidate) => candidate,
        Err(error)
            if explicit_tag_path.is_none()
                && error.to_string().starts_with("No gcr.io or pkg.dev image") =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    Ok(Some(ServiceSource {
        name: name.clone(),
        kind: "HelmRelease".to_string(),
        image_path: format!("{}:{}", candidate.repository, candidate.tag),
        container_name: "default".to_string(),
        source_name: "main".to_string(),
        source_root: repo_root.to_path_buf(),
        yaml_path: values_path.clone(),
        namespace,
        selector: None,
        helm: Some(HelmServiceSource {
            application_name: name,
            chart_path,
            values_path,
            image_tag_path: candidate.tag_path,
        }),
    }))
}

fn read_yaml_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Mapping(Mapping::new()));
    }
    serde_yaml::from_str(&fs::read_to_string(path)?)
        .with_context(|| format!("Invalid YAML in {}", path.display()))
}

fn merge_yaml(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Mapping(base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(current) => merge_yaml(current, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_str)
}

fn value_at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn select_image_candidate(
    values: &Value,
    explicit_tag_path: Option<&[String]>,
    application_name: &str,
) -> Result<ImageCandidate> {
    let mut candidates = Vec::new();
    collect_image_candidates(values, &mut Vec::new(), application_name, &mut candidates);

    if let Some(path) = explicit_tag_path {
        return candidates
            .into_iter()
            .find(|candidate| candidate.tag_path == path)
            .with_context(|| {
                format!(
                    "Annotation {TAG_PATH_ANNOTATION} points to '{}', which is not an image tag",
                    path.join(".")
                )
            });
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    let selected = candidates
        .into_iter()
        .next()
        .context("No gcr.io or pkg.dev image with a tag was found in merged chart values")?;
    Ok(selected)
}

fn collect_image_candidates(
    value: &Value,
    path: &mut Vec<String>,
    application_name: &str,
    candidates: &mut Vec<ImageCandidate>,
) {
    let Some(mapping) = value.as_mapping() else {
        return;
    };

    let repository = mapping
        .get(Value::String("repository".to_string()))
        .and_then(Value::as_str);
    let tag = mapping
        .get(Value::String("tag".to_string()))
        .and_then(scalar_to_string);
    if let (Some(repository), Some(tag)) = (repository, tag)
        && (repository.contains("gcr.io") || repository.contains("pkg.dev"))
    {
        let normalized_app = application_name.replace(['-', '_'], "");
        let normalized_repo = repository.replace(['-', '_'], "");
        let mut score = if path
            .iter()
            .any(|part| part.eq_ignore_ascii_case("microserviceContainer"))
        {
            100
        } else if path.last().is_some_and(|part| part == "image") {
            50
        } else {
            0
        };
        if normalized_repo.contains(&normalized_app) {
            score += 20;
        }
        let mut tag_path = path.clone();
        tag_path.push("tag".to_string());
        candidates.push(ImageCandidate {
            repository: repository.to_string(),
            tag,
            tag_path,
            score,
        });
    }

    for (key, child) in mapping {
        let Some(key) = key.as_str() else {
            continue;
        };
        path.push(key.to_string());
        collect_image_candidates(child, path, application_name, candidates);
        path.pop();
    }
}

fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub fn update_tag_at_path(content: &str, path: &[String], new_tag: &str) -> Result<String> {
    let parsed: Value =
        serde_yaml::from_str(content).context("Failed to parse Helm values YAML")?;
    value_at(&parsed, path)
        .with_context(|| format!("Image tag path '{}' does not exist", path.join(".")))?;

    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut result = String::with_capacity(content.len() + new_tag.len());
    let mut replaced = false;

    for line in content.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let indent = body
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        let trimmed = body.trim_start();
        let key = trimmed
            .split_once(':')
            .map(|(key, _)| key.trim().trim_matches(['\'', '"']).to_string());

        if let Some(key) = key {
            while stack.last().is_some_and(|(level, _)| *level >= indent) {
                stack.pop();
            }
            let mut current_path = stack.iter().map(|(_, key)| key.clone()).collect::<Vec<_>>();
            current_path.push(key.clone());
            if current_path == path {
                let prefix_len = body.find(':').context("Malformed YAML mapping")? + 1;
                let suffix = body[prefix_len..]
                    .find(" #")
                    .map(|index| &body[prefix_len + index..])
                    .unwrap_or("");
                let quote = body[prefix_len..]
                    .trim_start()
                    .chars()
                    .next()
                    .filter(|character| matches!(character, '\'' | '"'));
                result.push_str(&body[..prefix_len]);
                result.push(' ');
                if let Some(quote) = quote {
                    result.push(quote);
                    result.push_str(new_tag);
                    result.push(quote);
                } else {
                    result.push_str(new_tag);
                }
                result.push_str(suffix);
                if line.ends_with('\n') {
                    result.push('\n');
                }
                replaced = true;
                continue;
            }
            let value_part = trimmed
                .split_once(':')
                .map(|(_, value)| value.trim())
                .unwrap_or("");
            if value_part.is_empty() || value_part.starts_with('#') {
                stack.push((indent, key));
            }
        }
        result.push_str(line);
    }

    if !replaced {
        return Err(anyhow::anyhow!(
            "Could not locate scalar '{}' while preserving values formatting",
            path.join(".")
        ));
    }
    Ok(result)
}

pub fn render(service: &ServiceSource, values_content: &str) -> Result<String> {
    let helm = service
        .helm
        .as_ref()
        .context("Service is not Helm-backed")?;
    let mut values_file = NamedTempFile::new().context("Failed to create temporary values file")?;
    values_file.write_all(values_content.as_bytes())?;

    let output = Command::new("helm")
        .args(["template", &helm.application_name])
        .arg(&helm.chart_path)
        .arg("-f")
        .arg(values_file.path())
        .output()
        .context("Failed to execute helm template")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "helm template failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn lint(service: &ServiceSource, values_content: &str) -> Result<()> {
    let helm = service
        .helm
        .as_ref()
        .context("Service is not Helm-backed")?;
    let mut values_file = NamedTempFile::new().context("Failed to create temporary values file")?;
    values_file.write_all(values_content.as_bytes())?;
    let output = Command::new("helm")
        .arg("lint")
        .arg(&helm.chart_path)
        .arg("-f")
        .arg(values_file.path())
        .output()
        .context("Failed to execute helm lint")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "helm lint failed:\n{}",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }
    Ok(())
}

pub fn cluster_diff(env: &Environment, rendered: &str) -> Result<Option<String>> {
    let mut rendered_file =
        NamedTempFile::new().context("Failed to create rendered manifest file")?;
    rendered_file.write_all(rendered.as_bytes())?;
    let output = Command::new("kubectl")
        .args(["--context", &env.kubectl_context, "diff", "-f"])
        .arg(rendered_file.path())
        .output()
        .context("Failed to execute kubectl diff for rendered Helm manifests")?;
    match output.status.code() {
        Some(0) => Ok(None),
        Some(1) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
        Some(code) => Err(anyhow::anyhow!(
            "kubectl diff exited with status {code}:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        None => Err(anyhow::anyhow!(
            "kubectl diff terminated without an exit status"
        )),
    }
}

pub fn resolve_workload(rendered: &str, image_repository: &str) -> Result<RenderedWorkload> {
    for document in serde_yaml::Deserializer::from_str(rendered) {
        let resource = Value::deserialize(document)?;
        let Some(kind) = resource.get("kind").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(
            kind,
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob"
        ) {
            continue;
        }
        if let Some(container_name) = find_matching_container(&resource, image_repository) {
            let metadata = resource
                .get("metadata")
                .context("Rendered workload has no metadata")?;
            let name = metadata
                .get("name")
                .and_then(Value::as_str)
                .context("Rendered workload has no name")?;
            let namespace = metadata
                .get("namespace")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let selector = resource
                .get("spec")
                .and_then(|value| value.get("selector"))
                .and_then(|value| value.get("matchLabels"))
                .and_then(Value::as_mapping)
                .and_then(|labels| {
                    labels.iter().find_map(|(key, value)| {
                        Some(format!("{}={}", key.as_str()?, value.as_str()?))
                    })
                });
            return Ok(RenderedWorkload {
                name: name.to_string(),
                kind: kind.to_string(),
                namespace,
                selector,
                container_name,
            });
        }
    }
    Err(anyhow::anyhow!(
        "Rendered chart contains no workload using image repository '{}'",
        image_repository
    ))
}

fn find_matching_container(value: &Value, image_repository: &str) -> Option<String> {
    if let Some(mapping) = value.as_mapping() {
        if let Some(image) = mapping
            .get(Value::String("image".to_string()))
            .and_then(Value::as_str)
            && image.split([':', '@']).next() == Some(image_repository)
        {
            return mapping
                .get(Value::String("name".to_string()))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        return mapping
            .values()
            .find_map(|child| find_matching_container(child, image_repository));
    }
    value
        .as_sequence()?
        .iter()
        .find_map(|child| find_matching_container(child, image_repository))
}

pub fn helm_upgrade(env: &Environment, service: &ServiceSource) -> Result<()> {
    let helm = service
        .helm
        .as_ref()
        .context("Service is not Helm-backed")?;
    let namespace = service.namespace.as_deref().unwrap_or("default");
    let status = Command::new("helm")
        .args(["upgrade", "--install", &helm.application_name])
        .arg(&helm.chart_path)
        .arg("-f")
        .arg(&helm.values_path)
        .args([
            "--kube-context",
            &env.kubectl_context,
            "--namespace",
            namespace,
            "--create-namespace",
            "--atomic",
            "--wait",
        ])
        .status()
        .context("Failed to execute helm upgrade")?;
    if !status.success() {
        return Err(anyhow::anyhow!("helm upgrade failed"));
    }
    Ok(())
}

pub fn argocd_sync(service: &ServiceSource, revision: &str) -> Result<()> {
    let helm = service
        .helm
        .as_ref()
        .context("Service is not Helm-backed")?;
    let status = Command::new("argocd")
        .args([
            "app",
            "sync",
            &helm.application_name,
            "--revisions",
            revision,
            "--source-positions",
            "1",
            "--revisions",
            revision,
            "--source-positions",
            "2",
        ])
        .status()
        .context("Failed to execute argocd app sync; install/login to the ArgoCD CLI")?;
    if !status.success() {
        return Err(anyhow::anyhow!("argocd app sync failed"));
    }
    let status = Command::new("argocd")
        .args([
            "app",
            "wait",
            &helm.application_name,
            "--sync",
            "--health",
            "--timeout",
            "600",
        ])
        .status()
        .context("Failed to execute argocd app wait")?;
    if !status.success() {
        return Err(anyhow::anyhow!("argocd app wait failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeploymentDriver;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    #[test]
    fn updates_nested_tag_without_reformatting_values() {
        let original = "template-master:\n  replicas: 2\n  microserviceContainer:\n    image:\n      tag: \"1.0.0\" # release\n";
        let path =
            ["template-master", "microserviceContainer", "image", "tag"].map(ToString::to_string);
        let updated = update_tag_at_path(original, &path, "2.0.0").unwrap();
        assert_eq!(
            updated,
            "template-master:\n  replicas: 2\n  microserviceContainer:\n    image:\n      tag: \"2.0.0\" # release\n"
        );
    }

    #[test]
    fn merge_selects_primary_microservice_image() {
        let values: Value = serde_yaml::from_str(
            r#"
template-master:
  microserviceContainer:
    image:
      repository: gcr.io/project/authentication
      tag: 2.0.0
  cronJobs:
    backup:
      image:
        repository: gcr.io/project/backup
        tag: 1.0.0
"#,
        )
        .unwrap();
        let candidate = select_image_candidate(&values, None, "authentication").unwrap();
        assert_eq!(candidate.repository, "gcr.io/project/authentication");
        assert_eq!(
            candidate.tag_path.join("."),
            "template-master.microserviceContainer.image.tag"
        );
    }

    #[test]
    fn discovers_application_chart_values_and_environment_tag() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path();
        fs::create_dir_all(root.join("clusters/test/apps"))?;
        fs::create_dir_all(root.join("clusters/test/values"))?;
        fs::create_dir_all(root.join("charts/usvc/auth"))?;
        fs::write(
            root.join("clusters/test/apps/auth.yaml"),
            r#"
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: auth
spec:
  sources:
    - path: charts/usvc/auth
      helm:
        valueFiles:
          - $values/clusters/test/values/auth.yaml
    - ref: values
  destination:
    namespace: services
"#,
        )?;
        fs::write(
            root.join("charts/usvc/auth/values.yaml"),
            "service:\n  image:\n    repository: gcr.io/project/auth\n    tag: default\n",
        )?;
        fs::write(
            root.join("clusters/test/values/auth.yaml"),
            "service:\n  image:\n    tag: 2.3.4\n",
        )?;
        let environment = Environment {
            name: "test".to_string(),
            env_yaml_dir: root.to_path_buf(),
            env_yaml_dir_extra: BTreeMap::new(),
            kubectl_context: "test".to_string(),
            gcp_project: None,
            protected: None,
            deployment_driver: DeploymentDriver::ArgoCd,
            helm_cluster: Some("test".to_string()),
        };

        let services = discover_helm_services(&environment)?;
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "auth");
        assert_eq!(services[0].namespace.as_deref(), Some("services"));
        assert_eq!(services[0].image_path, "gcr.io/project/auth:2.3.4");
        assert_eq!(
            services[0].helm.as_ref().unwrap().image_tag_path.join("."),
            "service.image.tag"
        );
        Ok(())
    }

    #[test]
    fn resolves_workload_and_selector_from_rendered_chart() -> Result<()> {
        let workload = resolve_workload(
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: auth
  namespace: default
spec:
  selector:
    matchLabels:
      app: auth-app
  template:
    spec:
      containers:
        - name: auth
          image: gcr.io/project/auth:2.0.0
"#,
            "gcr.io/project/auth",
        )?;
        assert_eq!(workload.name, "auth");
        assert_eq!(workload.kind, "Deployment");
        assert_eq!(workload.selector.as_deref(), Some("app=auth-app"));
        assert_eq!(workload.container_name, "auth");
        Ok(())
    }
}
