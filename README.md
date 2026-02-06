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
repo_root = "/path/to/infra-repo/k8s/staging"
kubectl_context = "gke_context_staging"

[[environments]]
name = "production"
repo_root = "/path/to/infra-repo/k8s/prod"
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
```

## 🛠 For Developers

Please refer to [AGENT.md](./AGENT.md) for coding standards, branching strategies, and contribution guidelines.

## 📄 License

[Insert License Information Here]
