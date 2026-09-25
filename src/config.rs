use anyhow::{Context, Result};
use chrono::format::{Item, StrftimeItems};
use directories::ProjectDirs;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::helm::discover_helm_services;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub environments: Vec<Environment>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Environment {
    pub name: String,
    #[serde(default)]
    pub env_yaml_dir: PathBuf,
    #[serde(default)]
    pub env_yaml_dir_extra: BTreeMap<String, PathBuf>,
    pub kubectl_context: String,
    pub gcp_project: Option<String>,
    pub protected: Option<bool>,
    #[serde(default)]
    pub deployment_driver: DeploymentDriver,
    pub helm_cluster: Option<String>,
    #[serde(default)]
    pub sources: Vec<YamlSource>,
    pub release_tag: Option<ReleaseTagConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ReleaseTagConfig {
    pub enabled: bool,
    pub format: String,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentDriver {
    #[default]
    Manifest,
    Helm,
    ArgoCd,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct YamlSource {
    pub name: String,
    #[serde(rename = "repo_root")]
    pub root: PathBuf,
    #[serde(rename = "type", default)]
    pub driver: DeploymentDriver,
    pub helm_cluster: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceSource {
    pub name: String,
    pub kind: String,
    pub image_path: String,
    pub container_name: String,
    pub source_name: String,
    pub source_root: std::path::PathBuf,
    pub yaml_path: std::path::PathBuf,
    pub namespace: Option<String>,
    pub selector: Option<String>,
    pub deployment_driver: DeploymentDriver,
    pub helm: Option<HelmServiceSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HelmServiceSource {
    pub application_name: String,
    pub chart_path: PathBuf,
    pub values_path: PathBuf,
    pub image_tag_path: Vec<String>,
}

impl Environment {
    pub fn yaml_sources(&self) -> Vec<YamlSource> {
        let mut sources = Vec::new();

        if !self.env_yaml_dir.as_os_str().is_empty() {
            sources.push(YamlSource {
                name: "main".to_string(),
                root: self.env_yaml_dir.clone(),
                driver: self.deployment_driver,
                helm_cluster: self.helm_cluster.clone(),
            });
        }

        sources.extend(
            self.env_yaml_dir_extra
                .iter()
                .map(|(name, root)| YamlSource {
                    name: name.clone(),
                    root: root.clone(),
                    driver: DeploymentDriver::Manifest,
                    helm_cluster: None,
                }),
        );

        sources.extend(self.sources.iter().cloned());

        sources
    }

    pub fn list_services(&self) -> Result<Vec<ServiceSource>> {
        let mut services = HashSet::new();

        for source in self.yaml_sources() {
            if !source.root.exists() {
                continue;
            }

            if source.driver != DeploymentDriver::Manifest {
                services.extend(discover_helm_services(&source)?);
                continue;
            }

            for entry in WalkDir::new(&source.root)
                .into_iter()
                .filter_entry(|e| {
                    if e.depth() == 0 {
                        return true;
                    }
                    !e.file_name()
                        .to_str()
                        .map(|s| s.starts_with('.'))
                        .unwrap_or(false)
                })
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                        if ext == "yaml" || ext == "yml" {
                            if let Ok(content) = fs::read_to_string(path) {
                                let deserializer = serde_yaml::Deserializer::from_str(&content);
                                for document in deserializer {
                                    match serde_yaml::Value::deserialize(document) {
                                        Ok(resource) => {
                                            if let Some(source) =
                                                self.extract_gcr_service(&source, &resource, path)
                                            {
                                                services.insert(source);
                                            }
                                        }
                                        Err(e) => {
                                            let err_msg = e.to_string();
                                            if !err_msg.contains("more than one document") {
                                                eprintln!(
                                                    "Failed to parse YAML doc in {:?}: {}",
                                                    path, e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut sorted_services = services.into_iter().collect::<Vec<_>>();
        sorted_services.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sorted_services)
    }

    fn extract_gcr_service(
        &self,
        source: &YamlSource,
        resource: &serde_yaml::Value,
        yaml_path: &std::path::Path,
    ) -> Option<ServiceSource> {
        let kind = resource.get("kind")?.as_str()?;
        let metadata = resource.get("metadata")?;
        let name = metadata.get("name")?.as_str()?;

        let microservice_kinds = ["Deployment", "StatefulSet", "DaemonSet", "Job", "CronJob"];
        if !microservice_kinds.contains(&kind) {
            return None;
        }

        // Search for images in the spec
        if let Some(spec) = resource.get("spec") {
            if let Some((image_path, container_name)) = self.find_gcr_image(spec) {
                let namespace = metadata
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let mut selector = None;

                // Extract app label selector
                if let Some(sel) = spec.get("selector") {
                    if let Some(match_labels) = sel.get("matchLabels") {
                        if let Some(app) = match_labels.get("app") {
                            if let Some(app_str) = app.as_str() {
                                selector = Some(format!("app={}", app_str));
                            }
                        }
                    }
                }

                return Some(ServiceSource {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    image_path,
                    container_name,
                    source_name: source.name.clone(),
                    source_root: source.root.clone(),
                    yaml_path: yaml_path.to_path_buf(),
                    namespace,
                    selector,
                    deployment_driver: source.driver,
                    helm: None,
                });
            }
        }

        None
    }

    fn find_gcr_image(&self, value: &serde_yaml::Value) -> Option<(String, String)> {
        if let Some(map) = value.as_mapping() {
            // Check if this mapping is a container definition
            if let Some(image_val) = map.get(&serde_yaml::Value::String("image".to_string())) {
                if let Some(img_str) = image_val.as_str() {
                    if img_str.contains("gcr.io") || img_str.contains("pkg.dev") {
                        let container_name = map
                            .get(&serde_yaml::Value::String("name".to_string()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("default")
                            .to_string();
                        return Some((img_str.to_string(), container_name));
                    }
                }
            }

            for (_k, v) in map {
                if let Some(found) = self.find_gcr_image(v) {
                    return Some(found);
                }
            }
        }

        if let Some(seq) = value.as_sequence() {
            for v in seq {
                if let Some(found) = self.find_gcr_image(v) {
                    return Some(found);
                }
            }
        }

        None
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

        config.validate()?;

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

    fn validate(&self) -> Result<()> {
        for env in &self.environments {
            env.validate()?;
        }

        Ok(())
    }
}

impl Environment {
    fn validate(&self) -> Result<()> {
        if let Some(release_tag) = &self.release_tag {
            if release_tag.format.is_empty() {
                return Err(anyhow::anyhow!(
                    "Environment '{}' defines an empty release tag format",
                    self.name
                ));
            }
            if StrftimeItems::new(&release_tag.format).any(|item| matches!(item, Item::Error)) {
                return Err(anyhow::anyhow!(
                    "Environment '{}' defines an invalid release tag format '{}'",
                    self.name,
                    release_tag.format
                ));
            }
        }

        if !self.env_yaml_dir.as_os_str().is_empty()
            && self.deployment_driver != DeploymentDriver::Manifest
            && self.helm_cluster.as_deref().unwrap_or_default().is_empty()
        {
            return Err(anyhow::anyhow!(
                "Environment '{}' uses deployment_driver '{:?}' but does not define helm_cluster",
                self.name,
                self.deployment_driver
            ));
        }

        if self.env_yaml_dir_extra.contains_key("main") {
            return Err(anyhow::anyhow!(
                "Environment '{}' uses reserved extra source name 'main'",
                self.name
            ));
        }

        if self.yaml_sources().is_empty() {
            return Err(anyhow::anyhow!(
                "Environment '{}' does not define any deployment sources",
                self.name
            ));
        }

        let mut seen_paths = HashSet::new();
        let mut seen_names = HashSet::new();
        for source in self.yaml_sources() {
            if !seen_names.insert(source.name.clone()) {
                return Err(anyhow::anyhow!(
                    "Environment '{}' defines duplicate source name '{}'",
                    self.name,
                    source.name
                ));
            }
            if source.driver != DeploymentDriver::Manifest
                && source
                    .helm_cluster
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
            {
                return Err(anyhow::anyhow!(
                    "Source '{}' in environment '{}' uses driver '{:?}' but does not define helm_cluster",
                    source.name,
                    self.name,
                    source.driver
                ));
            }
            let normalized = normalize_source_path(&source.root);
            if !seen_paths.insert(normalized.clone()) {
                return Err(anyhow::anyhow!(
                    "Environment '{}' defines duplicate YAML source path '{}' (source '{}')",
                    self.name,
                    normalized.display(),
                    source.name
                ));
            }
        }

        Ok(())
    }
}

fn normalize_source_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_list_services_gcr_filter() -> Result<()> {
        let dir = tempdir()?;
        let env_yaml_dir = dir.path().to_path_buf();

        // 1. Valid Deployment with GCR image
        let service1_dir = env_yaml_dir.join("service1");
        fs::create_dir(&service1_dir)?;
        fs::write(
            service1_dir.join("deploy.yaml"),
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: gcr-service
  namespace: test-ns
spec:
  selector:
    matchLabels:
      app: gcr-service-app
  template:
    metadata:
      labels:
        app: gcr-service-app
    spec:
      containers:
      - name: gcr-container
        image: gcr.io/my-project/my-image:latest
"#,
        )?;

        // 2. Valid StatefulSet with Artifact Registry image
        let service2_dir = env_yaml_dir.join("service2");
        fs::create_dir(&service2_dir)?;
        fs::write(
            service2_dir.join("statefulset.yaml"),
            r#"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: pkg-service
spec:
  template:
    spec:
      containers:
      - name: main
        image: europe-west1-docker.pkg.dev/my-project/my-repo/my-image:v1
"#,
        )?;

        // 3. Invalid Kind (Service)
        fs::write(
            env_yaml_dir.join("service.yaml"),
            r#"
apiVersion: v1
kind: Service
metadata:
  name: not-a-microservice
spec:
  ports:
  - port: 80
"#,
        )?;

        // 4. Invalid Image (Docker Hub)
        let service3_dir = env_yaml_dir.join("service3");
        fs::create_dir(&service3_dir)?;
        fs::write(
            service3_dir.join("deploy.yaml"),
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: dockerhub-service
spec:
  template:
    spec:
      containers:
      - name: main
        image: nginx:latest
"#,
        )?;

        let env = Environment {
            name: "test".to_string(),
            env_yaml_dir,
            env_yaml_dir_extra: BTreeMap::new(),
            kubectl_context: "test".to_string(),
            gcp_project: None,
            protected: None,
            deployment_driver: DeploymentDriver::Manifest,
            helm_cluster: None,
            sources: Vec::new(),
            release_tag: None,
        };

        let services = env.list_services()?;
        assert_eq!(services.len(), 2);

        let gcr_service = services.iter().find(|s| s.name == "gcr-service").unwrap();
        assert_eq!(gcr_service.kind, "Deployment");
        assert_eq!(gcr_service.image_path, "gcr.io/my-project/my-image:latest");
        assert_eq!(gcr_service.container_name, "gcr-container");
        assert!(
            gcr_service
                .yaml_path
                .to_str()
                .unwrap()
                .contains("deploy.yaml")
        );
        assert_eq!(
            gcr_service.selector,
            Some("app=gcr-service-app".to_string())
        );
        assert_eq!(gcr_service.namespace, Some("test-ns".to_string()));

        let pkg_service = services.iter().find(|s| s.name == "pkg-service").unwrap();
        assert_eq!(pkg_service.kind, "StatefulSet");
        assert_eq!(
            pkg_service.image_path,
            "europe-west1-docker.pkg.dev/my-project/my-repo/my-image:v1"
        );
        assert!(
            pkg_service
                .yaml_path
                .to_str()
                .unwrap()
                .contains("statefulset.yaml")
        );

        assert!(!services.iter().any(|s| s.name == "not-a-microservice"));
        assert!(!services.iter().any(|s| s.name == "dockerhub-service"));

        Ok(())
    }

    #[test]
    fn test_list_services_includes_named_extra_sources() -> Result<()> {
        let dir = tempdir()?;
        let env_yaml_dir = dir.path().join("main");
        let extra_yaml_dir = dir.path().join("demo");
        fs::create_dir_all(&env_yaml_dir)?;
        fs::create_dir_all(&extra_yaml_dir)?;

        fs::write(
            env_yaml_dir.join("main.yaml"),
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shared-service
  namespace: default
spec:
  template:
    spec:
      containers:
      - name: main
        image: gcr.io/my-project/shared:main
"#,
        )?;

        fs::write(
            extra_yaml_dir.join("demo.yaml"),
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shared-service
  namespace: default
spec:
  template:
    spec:
      containers:
      - name: demo
        image: gcr.io/my-project/shared:demo
"#,
        )?;

        let env = Environment {
            name: "test".to_string(),
            env_yaml_dir: env_yaml_dir.clone(),
            env_yaml_dir_extra: BTreeMap::from([("demo".to_string(), extra_yaml_dir.clone())]),
            kubectl_context: "test".to_string(),
            gcp_project: None,
            protected: None,
            deployment_driver: DeploymentDriver::Manifest,
            helm_cluster: None,
            sources: Vec::new(),
            release_tag: None,
        };

        let services = env.list_services()?;
        assert_eq!(services.len(), 2);

        let main_service = services
            .iter()
            .find(|s| s.source_name == "main")
            .expect("main source service");
        assert_eq!(main_service.source_root, env_yaml_dir);

        let demo_service = services
            .iter()
            .find(|s| s.source_name == "demo")
            .expect("demo source service");
        assert_eq!(demo_service.source_root, extra_yaml_dir);

        Ok(())
    }

    #[test]
    fn test_environment_accepts_mixed_deployment_sources() -> Result<()> {
        let config: Config = toml::from_str(
            r#"
[[environments]]
name = "preprod"
kubectl_context = "cluster-preprod"

[[environments.sources]]
name = "legacy"
type = "manifest"
repo_root = "/repos/legacy/preprod"

[[environments.sources]]
name = "helm"
type = "argo-cd"
repo_root = "/repos/helm"
helm_cluster = "ccs-preprod"
"#,
        )?;
        config.validate()?;

        let sources = config.environments[0].yaml_sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].name, "legacy");
        assert_eq!(sources[0].driver, DeploymentDriver::Manifest);
        assert_eq!(sources[1].name, "helm");
        assert_eq!(sources[1].driver, DeploymentDriver::ArgoCd);
        assert_eq!(sources[1].helm_cluster.as_deref(), Some("ccs-preprod"));
        Ok(())
    }

    #[test]
    fn test_legacy_environment_becomes_an_implicit_source() -> Result<()> {
        let config: Config = toml::from_str(
            r#"
[[environments]]
name = "staging"
env_yaml_dir = "/repos/legacy/staging"
kubectl_context = "cluster-staging"
"#,
        )?;
        config.validate()?;

        let sources = config.environments[0].yaml_sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "main");
        assert_eq!(sources[0].driver, DeploymentDriver::Manifest);
        assert_eq!(sources[0].root, PathBuf::from("/repos/legacy/staging"));
        Ok(())
    }

    #[test]
    fn test_environment_accepts_release_tag_configuration() -> Result<()> {
        let config: Config = toml::from_str(
            r#"
[[environments]]
name = "production"
env_yaml_dir = "/repos/production"
kubectl_context = "cluster-production"

[environments.release_tag]
enabled = true
format = "prod_%Y%m%d"
"#,
        )?;
        config.validate()?;

        assert_eq!(
            config.environments[0].release_tag,
            Some(ReleaseTagConfig {
                enabled: true,
                format: "prod_%Y%m%d".to_string(),
            })
        );
        Ok(())
    }

    #[test]
    fn test_environment_rejects_invalid_release_tag_format() -> Result<()> {
        let config: Config = toml::from_str(
            r#"
[[environments]]
name = "production"
env_yaml_dir = "/repos/production"
kubectl_context = "cluster-production"

[environments.release_tag]
enabled = true
format = "prod_%Q"
"#,
        )?;

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("invalid release tag format"), "{error}");
        Ok(())
    }

    #[test]
    fn test_list_services_aggregates_manifest_and_helm_sources() -> Result<()> {
        let directory = tempdir()?;
        let legacy = directory.path().join("legacy");
        let helm = directory.path().join("helm");
        fs::create_dir_all(&legacy)?;
        fs::create_dir_all(helm.join("clusters/test/apps"))?;
        fs::create_dir_all(helm.join("clusters/test/values"))?;
        fs::create_dir_all(helm.join("charts/auth"))?;
        fs::write(
            legacy.join("billing.yaml"),
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: billing\nspec:\n  template:\n    spec:\n      containers:\n        - name: billing\n          image: gcr.io/project/billing:1.0.0\n",
        )?;
        fs::write(
            helm.join("clusters/test/apps/auth.yaml"),
            "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: auth\nspec:\n  sources:\n    - path: charts/auth\n      helm:\n        valueFiles:\n          - $values/clusters/test/values/auth.yaml\n    - ref: values\n  destination:\n    namespace: default\n",
        )?;
        fs::write(
            helm.join("charts/auth/values.yaml"),
            "image:\n  repository: gcr.io/project/auth\n  tag: default\n",
        )?;
        fs::write(
            helm.join("clusters/test/values/auth.yaml"),
            "image:\n  tag: 2.0.0\n",
        )?;

        let environment = Environment {
            name: "test".to_string(),
            env_yaml_dir: PathBuf::new(),
            env_yaml_dir_extra: BTreeMap::new(),
            kubectl_context: "test".to_string(),
            gcp_project: None,
            protected: None,
            deployment_driver: DeploymentDriver::Manifest,
            helm_cluster: None,
            sources: vec![
                YamlSource {
                    name: "legacy".to_string(),
                    root: legacy,
                    driver: DeploymentDriver::Manifest,
                    helm_cluster: None,
                },
                YamlSource {
                    name: "helm".to_string(),
                    root: helm,
                    driver: DeploymentDriver::ArgoCd,
                    helm_cluster: Some("test".to_string()),
                },
            ],
            release_tag: None,
        };

        let services = environment.list_services()?;
        assert_eq!(services.len(), 2);
        assert!(services.iter().any(|service| {
            service.name == "billing"
                && service.source_name == "legacy"
                && service.deployment_driver == DeploymentDriver::Manifest
        }));
        assert!(services.iter().any(|service| {
            service.name == "auth"
                && service.source_name == "helm"
                && service.deployment_driver == DeploymentDriver::ArgoCd
        }));
        Ok(())
    }
}
