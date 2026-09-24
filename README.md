# Davit

A safe Kubernetes release orchestrator and TUI built with Rust. Davit supports both legacy
Kubernetes manifests and Helm releases managed directly or through ArgoCD.

## 🚢 What is a Davit?

In maritime terms, a **davit** is a crane-like device used to safely lower lifeboats or anchors into the water. In our context, **Davit** is designed to safely "lower" new container versions into our Kubernetes clusters.

## ✨ Features

-   **Safety First:** Standardizes the path from Google Artifact Registry to running Pod.
-   **Terminal User Interface (TUI):**
    -   **Wizard Mode:** Interactive selection of environments, services, and image tags (`inquire`).
    -   **Dashboard Mode:** Real-time rollout monitoring with split-screen logs (`ratatui`).
-   **Visual Diffs:** Preview infrastructure YAML changes before applying them.
-   **Helm & GitOps:** Discover ArgoCD Applications, update environment values, validate/render charts, and release through Helm or ArgoCD.
-   **Automated Auditing:** Keeps the release state in Git; Helm changes are committed before deployment, while legacy manifests retain their post-rollout commit flow.
-   **Deployment Info:** Inspect deployed services with `davit info` - refreshes the YAML sources (unless `--no-fetch` is passed), reads live workload state from cluster, and shows YAML vs cluster image drift together with workload status, current image version, last release commit, labels, pod details, resource usage, and recent events.
-   **Scriptable:** `--non-interactive` never prompts and fails naming the decision it could not ask, so Davit can be driven from CI or an agent.

## 🚀 Getting Started

### Prerequisites

-   Rust (Latest Stable)
-   `kubectl`
-   `gcloud`
-   `git`
-   `helm` for Helm-backed environments
-   `argocd` for environments using the `argo-cd` deployment driver

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
name = "legacy-production"
env_yaml_dir = "/path/to/infra-repo/k8s/prod"
kubectl_context = "gke_context_prod"
protected = true

# Helm repository deployed directly with `helm upgrade --install`
[[environments]]
name = "preprod"
env_yaml_dir = "/path/to/helm-repo"
kubectl_context = "gke_context_preprod"
deployment_driver = "helm"
helm_cluster = "ccs-preprod"

# The same repository can be released through an ArgoCD Application.
# Davit commits and pushes the values change before syncing the exact Git SHA.
[[environments]]
name = "production"
env_yaml_dir = "/path/to/helm-repo"
kubectl_context = "gke_context_prod"
deployment_driver = "argo-cd"
helm_cluster = "ccs-prod"
protected = true
```

`deployment_driver` defaults to `manifest`, preserving existing configurations. For Helm
drivers, `env_yaml_dir` is the repository root and `helm_cluster` selects
`clusters/<helm_cluster>/apps` and its referenced values files.

Davit normally identifies the primary image from the merged chart defaults and environment
values. If a chart has multiple equally relevant application images, declare the tag explicitly
on its ArgoCD Application:

```yaml
metadata:
  annotations:
    davit.io/image-tag-path: template-master.microserviceContainer.image.tag
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

# Discover the values accepted by --env and --service
davit list envs
davit list services --env staging
```

### Running without a terminal

`--non-interactive` (also assumed when stdin is not a terminal) never prompts. Instead of
`The input device is not a TTY`, it reports the decision it could not ask and how to supply it:

```bash
$ davit deploy --env preprod --service svc-api --tag v1.2.3 --non-interactive
Error: Cannot ask for a service: --non-interactive was passed.
'svc-api' matches 2 entries:
  - svc-api (no-namespace)
  - svc-api (tenant-b)
Pass one of them verbatim.
```

Use `davit list services` to discover those values, or `--namespace` to narrow the choice:

```bash
# Unambiguous without quoting a rendered display name
davit deploy --env preprod --service svc-api --namespace tenant-b --tag v1.2.3

# `-` selects the manifests that declare no namespace, as the listing shows them
davit deploy --env preprod --service svc-api --namespace - --tag v1.2.3

# Protected environments take an explicit opt-in instead of the typed confirmation
davit deploy --env production --service auth-api --tag v1.2.3 \
  --non-interactive --confirm-env production

# Validate an invocation without touching anything, and without a terminal
davit deploy --env production --service auth-api --tag v1.2.3 --dry-run

# Read the manifests as they are on disk, without running git pull first
davit info --env staging --service auth-api --no-fetch
```

## 🛠 For Developers

Please refer to [AGENT.md](./AGENT.md) for coding standards, branching strategies, and contribution guidelines.

## 📄 License

[Insert License Information Here]
