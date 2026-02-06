use serde::Deserialize;
use std::path::PathBuf;
use anyhow::{Result, Context};
use directories::ProjectDirs;
use std::fs;
use std::collections::HashSet;
use walkdir::WalkDir;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceSource {
    pub name: String,
    pub image_path: String,
    pub container_name: String,
    pub yaml_path: std::path::PathBuf,
    pub namespace: Option<String>,
    pub selector: Option<String>,
}

impl Environment {
    pub fn list_services(&self) -> Result<Vec<ServiceSource>> {
        let mut services = HashSet::new();
        if !self.repo_root.exists() {
            return Ok(Vec::new());
        }

        for entry in WalkDir::new(&self.repo_root)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                !e.file_name().to_str().map(|s| s.starts_with('.')).unwrap_or(false)
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
                                        if let Some(source) = self.extract_gcr_service(&resource, path) {
                                            services.insert(source);
                                        }
                                    }
                                    Err(e) => {
                                        let err_msg = e.to_string();
                                        // Ignore "deserializing from YAML containing more than one document" if we are already using Deserializer
                                        // But if it's another error, log it.
                                        if !err_msg.contains("more than one document") {
                                            eprintln!("Failed to parse YAML doc in {:?}: {}", path, e);
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

    fn extract_gcr_service(&self, resource: &serde_yaml::Value, yaml_path: &std::path::Path) -> Option<ServiceSource> {
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
                let namespace = metadata.get("namespace").and_then(|v| v.as_str()).map(|s| s.to_string());
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
                    image_path,
                    container_name,
                    yaml_path: yaml_path.to_path_buf(),
                    namespace,
                    selector,
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
                        let container_name = map.get(&serde_yaml::Value::String("name".to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_list_services_gcr_filter() -> Result<()> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();

        // 1. Valid Deployment with GCR image
        let service1_dir = repo_root.join("service1");
        fs::create_dir(&service1_dir)?;
        fs::write(service1_dir.join("deploy.yaml"), r#"
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
"#)?;

        // 2. Valid StatefulSet with Artifact Registry image
        let service2_dir = repo_root.join("service2");
        fs::create_dir(&service2_dir)?;
        fs::write(service2_dir.join("statefulset.yaml"), r#"
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
"#)?;

        // 3. Invalid Kind (Service)
        fs::write(repo_root.join("service.yaml"), r#"
apiVersion: v1
kind: Service
metadata:
  name: not-a-microservice
spec:
  ports:
  - port: 80
"#)?;

        // 4. Invalid Image (Docker Hub)
        let service3_dir = repo_root.join("service3");
        fs::create_dir(&service3_dir)?;
        fs::write(service3_dir.join("deploy.yaml"), r#"
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
"#)?;

        let env = Environment {
            name: "test".to_string(),
            repo_root,
            kubectl_context: "test".to_string(),
            gcp_project: None,
            gcp_location: None,
            gcp_repository: None,
            protected: None,
        };

        let services = env.list_services()?;
        assert_eq!(services.len(), 2);
        
        let gcr_service = services.iter().find(|s| s.name == "gcr-service").unwrap();
        assert_eq!(gcr_service.image_path, "gcr.io/my-project/my-image:latest");
        assert_eq!(gcr_service.container_name, "gcr-container");
        assert!(gcr_service.yaml_path.to_str().unwrap().contains("deploy.yaml"));
        assert_eq!(gcr_service.selector, Some("app=gcr-service-app".to_string()));
        assert_eq!(gcr_service.namespace, Some("test-ns".to_string()));

        let pkg_service = services.iter().find(|s| s.name == "pkg-service").unwrap();
        assert_eq!(pkg_service.image_path, "europe-west1-docker.pkg.dev/my-project/my-repo/my-image:v1");
        assert!(pkg_service.yaml_path.to_str().unwrap().contains("statefulset.yaml"));

        assert!(!services.iter().any(|s| s.name == "not-a-microservice"));
        assert!(!services.iter().any(|s| s.name == "dockerhub-service"));

        Ok(())
    }
}
