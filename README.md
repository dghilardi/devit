# Davit

A safe Kubernetes deployment wrapper and TUI built with Rust.

## 🚢 What is a Davit?

In maritime terms, a **davit** is a crane-like device used to safely lower lifeboats or anchors into the water. In our context, **Davit** is designed to safely "lower" new container versions into our Kubernetes clusters.

## ✨ Features

-   **Safety First:** Standardizes the path from Google Artifact Registry to running Pod.
-   **Terminal User Interface (TUI):**
    -   **Wizard Mode:** Interactive selection of environments, services, and image tags (`inquire`).
    -   **Dashboard Mode:** Real-time rollout monitoring with split-screen logs (`ratatui`).
-   **Visual Diffs:** Preview infrastructure YAML changes before applying them.
-   **Automated Auditing:** Automatically commits and pushes changes to Git upon successful deployment.
-   **Deployment Info:** Inspect deployed services with `davit info` - runs `git pull`, reads live workload state from cluster, and shows YAML vs cluster image drift together with workload status, current image version, last release commit, labels, pod details, resource usage, and recent events.

## 🚀 Getting Started

### Prerequisites

-   Rust (Latest Stable)
-   `kubectl`
-   `gcloud`
-   `git`

### Configuration

Davit respects the XDG Base Directory specification. Create your configuration at:
`$XDG_CONFIG_HOME/davit/config.toml`

Example configuration:
```toml
[[environments]]
name = "staging"
env_yaml_dir = "/path/to/infra-repo/k8s/staging"
env_yaml_dir_extra.demo = "/path/to/infra-demo-repo/k8s/staging"
kubectl_context = "gke_context_staging"

[[environments]]
name = "production"
env_yaml_dir = "/path/to/infra-repo/k8s/prod"
kubectl_context = "gke_context_prod"
protected = true
```

### Installation

```bash
cargo install --path .
```

### Usage

```bash
# Start the full wizard
davit deploy

# Direct deploy
davit deploy --env staging --service auth-api --tag v1.2.3

# Inspect a deployed service
davit info --env staging --service auth-api

# Filter by namespace
davit info --env staging --namespace default --service auth-api
```

## 🛠 For Developers

Please refer to [AGENT.md](./AGENT.md) for coding standards, branching strategies, and contribution guidelines.

## 📄 License

[Insert License Information Here]
