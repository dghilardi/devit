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
-   **Manual Configuration Changes:** Apply selected edited manifests or Helm values/chart YAML with `davit apply`, inspect diffs with `d`, choose their order, optionally restart dependent workloads, and commit only the selected files.
-   **Helm & GitOps:** Discover ArgoCD Applications, update environment values, validate/render charts, and release through Helm or ArgoCD.
-   **Live Helm Rollout:** Shows pod logs during the upgrade while preserving automatic rollback; closing the dashboard waits for Helm and leaves values uncommitted.
-   **Automated Auditing:** Keeps the release state in Git; Direct Helm and manifest changes are committed after successful rollout; ArgoCD changes are committed before syncing the exact Git revision.
-   **Deployment Info:** Inspect deployed services with `davit info` - refreshes the YAML sources (unless `--no-fetch` is passed), reads live workload state from cluster, and shows YAML vs cluster image drift together with workload status, current image version, last release commit, labels, pod details, resource usage, and recent events.
-   **Scriptable:** `--non-interactive` never prompts and fails naming the decision it could not ask, so Davit can be driven from CI or an agent.

## 🚀 Getting Started

### Prerequisites

-   Rust (Latest Stable)
-   `kubectl`
-   `gcloud`
-   `git`
-   `helm` for Helm-backed environments
-   `tar` for Git snapshots when applying Helm changes
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

[environments.release_tag]
enabled = true
format = "prod_%Y%m%d"

# A transitional environment can aggregate repositories with different drivers.
[[environments]]
name = "preprod"
kubectl_context = "gke_context_preprod"

[[environments.sources]]
name = "legacy"
type = "manifest"
repo_root = "/path/to/legacy-infra/k8s/preprod"

[[environments.sources]]
name = "helm"
type = "argo-cd"
repo_root = "/path/to/helm-repo"
helm_cluster = "ccs-preprod"
```

The legacy `env_yaml_dir`, `env_yaml_dir_extra`, `deployment_driver`, and `helm_cluster` fields
remain supported and are converted to implicit sources. New configurations should prefer
`environments.sources`: `type` accepts `manifest`, `helm`, or `argo-cd`; `repo_root` is the Git
working copy, and Helm sources use `helm_cluster` to select `clusters/<helm_cluster>/apps` and its
referenced values files.

Before every deploy, inspection, or service listing, Davit runs `git pull --ff-only` for every
distinct source repository. Untracked or modified files that do not overlap incoming changes are
preserved and do not prevent the pull. A conflict, a divergent branch, or any other pull failure is
printed and aborts the operation. Use `--no-fetch` only when deliberately working from local state.

An environment can publish an annotated Git tag after a confirmed successful rollout by defining
`environments.release_tag`. The format uses `strftime` placeholders and the machine's local time.
If the formatted tag already exists locally or on `origin`, Davit appends the first available
zero-padded increment (`_01`, `_02`, and so on). The tag is created in the selected service's source
repository and pushed to `origin`; `--dry-run` only prints the tag and Git commands. No tag is
created when Git commit/push is skipped or rollout completion is not confirmed.

Davit normally identifies the primary image from the merged chart defaults and environment
values, supporting both separate `image.repository`/`image.tag` fields and full `image: repository:tag`
strings. It prefers `microserviceContainer` or `app` images over sidecars. If a chart has multiple
equally relevant application images, declare the tag explicitly on its ArgoCD Application:

```yaml
metadata:
  annotations:
    davit.io/image-tag-path: template-master.microserviceContainer.image.tag
```

For a full image string, point the annotation to the image itself (for example, `app.image`).

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

### Applying manually edited YAML

Use `davit apply --env staging` after editing YAML files by hand. Both `manifest` and direct `helm`
sources are supported; ArgoCD sources are explicitly skipped. Modified files (staged or
unstaged) and new, non-ignored `.yaml`/`.yml` files inside the environment's configured sources
are discovered. Deleted files are reported and skipped; standalone manifests never trigger cluster
deletion. Helm upgrades retain Helm's normal resource lifecycle.

In the selection screen, use **Space** to select files, **d** to preview the highlighted file's
diff against Git HEAD, **Esc** to return from the diff, and **J/K** to move a file down/up in the
application order. **Enter** confirms the selection; **Esc** in the list cancels. Arrow keys move
through the list or scroll the diff. Secret payloads, including annotations, are hidden; changes
to their hidden values may therefore not appear in the diff. For Helm YAML, **d** previews the
rendered Kubernetes changes caused by that file, with Secret payloads hidden. The final plan
combines all selected files and shows the rendered diff for each affected release.

Davit asks for a commit message, validates all selected snapshots with a server dry run, and
shows the cluster diff before final approval. Cluster diffs for files containing Secrets are
hidden. It applies manifests or upgrades Helm releases in selection order and waits for Deployment,
StatefulSet, and DaemonSet rollouts. For changed ConfigMaps and Secrets, it offers to discover referencing workloads in the cluster
and lets you select which to restart. References in environment variables, volumes, and projected
volumes are supported. Workloads whose pod templates already changed and completed rollout after
the configuration changes are excluded from the restart list.

After success, Davit creates one commit per repository and pushes it. Only selected files enter
the commits, preserving unrelated staged files. No environment release tag is created. Files are
applied from temporary snapshots; subsequent local edits abort the sequence before committing.
At the first failure, processing stops with a summary of completed and unfinished operations. Local edits
are retained. Failed Helm upgrades request automatic rollback; successful earlier upgrades and
manifest applications remain applied. Git operations
across repositories are independent, so a later failure can leave earlier repositories pushed.

```bash
# Interactive selection, diff previews, commit message, and optional restarts
davit apply --env staging

# Preview without accessing the cluster, changing files, or creating commits
davit apply --env staging --file k8s/staging/config.yaml --dry-run

# Paths relative to the Git repository root, repeated in application order
davit apply --env staging \
  --file k8s/staging/config.yaml --file k8s/staging/deployment.yaml \
  --message "fix(config): adjust API timeouts" --yes --restart skip \
  --non-interactive

# Explicitly restart all discovered dependent workloads
davit apply --env staging --file k8s/staging/config.yaml \
  --message "fix(config): update settings" --yes --restart all

# Apply edited Helm values without choosing a new image tag
davit apply --env staging --file clusters/staging/values/api.yaml \
  --message "fix(config): adjust API settings" --yes --restart skip
```

Direct Helm sources use the configured Application metadata to identify the release, namespace,
local chart, and ordered values files. All files selected for a release are combined into one
`helm upgrade --install --atomic --wait`. A shared values file or chart change affects every
referencing release in this environment, and the plan lists them all. Releases run in the order
of their first selected input. Registry image discovery is not required and image tags are not rewritten.

Charts and values are rendered from Git HEAD plus only selected changes, then validated with
`helm lint` and `helm template`. Other local changes remain excluded from the upgrade. Both rendering
and upgrade use the release namespace and all values files in declaration order. Helm dry-run uses
local rendering and does not access the cluster. Chart dependencies must be available in Git HEAD.

Helm apply currently supports local charts and file-based Application `spec.sources` configuration.
Application routing changes must be committed separately, and inline Helm overrides are rejected.
The chart and its existing values must render successfully at Git HEAD. Only YAML inputs are
selectable; changes to `.tpl` helpers or other chart files must be committed separately before applying.

Absolute paths and paths relative to the current directory are also accepted. If the same
repository-relative path matches multiple source repositories, pass an absolute path.
Non-interactive execution requires explicit files, a message, `--yes`, and, for ConfigMaps/Secrets,
`--restart skip` or `--restart all`. Protected environments still require `--confirm-env NAME`.
`--no-fetch` skips the usual source refresh. With `--dry-run`, select files explicitly when running
without a terminal; no commit message or approval is required.

Server validation requires referenced namespaces and resource types to exist already; create a
new Namespace or CRD in a separate invocation before applying resources that depend on it.

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
