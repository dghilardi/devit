# Product Requirement Document: Davit

| Metadata | Details |
| --- | --- |
| **Project Name** | **Davit** |
| **Version** | 1.0 |
| **Status** | Approved Draft |
| **Target Stack** | Rust (Linux/macOS) |
| **Core Function** | Safe Kubernetes Deployment Wrapper & TUI |

## 1. Executive Summary

**Concept:** In maritime terms, a *davit* is a crane-like device used to safely lower lifeboats or anchors into the water. In our context, **Davit** is a Rust-based CLI tool designed to safely "lower" new container versions into our Kubernetes clusters.

**Problem:** Our current deployment workflow relies on disparate manual steps: checking Google Artifact Registry (GAR), editing YAMLs by hand, switching `kubectl` contexts, and monitoring rollouts via raw terminal output. This "human glue" is prone to context errors (e.g., deploying to Prod instead of Staging) and lacks a standardized audit trail.

**Solution:** Davit acts as a mechanical safety layer. It standardizes the path from Artifact Registry to running Pod. It validates inputs, visualizes changes via a TUI (Text User Interface), creates a unified diff of the infrastructure code, and provides a rich dashboard to monitor the specific moment of "handoff" between old and new pods.

---

## 2. User Personas

* **The Backend Developer:** "I built a feature and want to test it in Staging. I don't remember the exact `kubectl` context or the long image hash. I just want to select 'MyService' and the latest version."
* **The DevOps Engineer:** "I need to release a hotfix to Production. I need absolute certainty that the YAML diff is correct and that the previous pod shuts down gracefully while the new one starts."

---

## 3. Functional Requirements

### 3.1 Global Configuration

Davit must respect the XDG Base Directory specification.

* **Location:** `$XDG_CONFIG_HOME/davit/config.toml` (defaulting to `~/.config/davit/config.toml`).
* **Structure:** Defines available environments, root paths for local IaC repositories, and strict Kubernetes contexts.

**Example Schema:**

```toml
[defaults]
interactive = true

[[environments]]
name = "staging"
repo_root = "/home/user/git/infra-repo/k8s/staging"
kubectl_context = "gke_europe-west1_staging"

[[environments]]
name = "production"
repo_root = "/home/user/git/infra-repo/k8s/prod"
kubectl_context = "gke_europe-west1_prod"
protected = true  # Forces an extra "type the environment name to confirm" step

```

### 3.2 Progressive Disclosure & Input Resolution

The tool supports a hybrid workflow: "CLI for speed, TUI for discovery."

**Input Logic:**

**Smart Disambiguation Strategy:**

* **YAML-based Discovery:** Instead of assuming a fixed directory structure, Davit recursively scans the `repo_root` for YAML files and identifies microservices by searching for GCR/Artifact Registry images within container specs.
* **Exact Match:** User inputs `user-api`. Found 1 folder/service. -> **Select.**
* **Partial Match (Unique):** User inputs `pay`. Only `payment-service` exists. -> **Prompt:** *"Did you mean 'payment-service'? (Y/n)"*
* **Ambiguous Match:** User inputs `data`. Matches `data-ingest` and `database-proxy`. -> **Menu:** Show list of these two for selection.
* **Namespace Conflict:** User inputs `redis`. Exists in both `cache` and `session` namespaces. -> **Menu:** *"Which 'redis'?"*

### 3.3 Artifact Registry Integration

* **Source:** Google Artifact Registry (`pkg.dev`) and legacy Google Container Registry (`gcr.io`) via `gcloud` wrapper.
* **Sorting:** Reverse chronological (Newest `updateTime` at top). Supports robust timestamp parsing for various `gcloud` output formats.
* **Visuals:** The list must display:
* **Tag** (e.g., `v1.0.4-fix`)
* **Age** (e.g., `2 hours ago` - colored Green for recent, Grey for old)
* **Hash** (Short SHA)



### 3.4 The "Dry Run" (Preview Phase)

Before touching the cluster, Davit modifies the YAML in memory or a temp file.

* **Unified Diff:** A context-aware diff view showing specifically what is changing in the discovered `yaml_path`.
* **Interactive Selection:** After showing the diff, provide a menu with:
    * **Apply:** Save change and proceed to deployment.
    * **Show Full Diff:** Toggle between unified and complete file view.
    * **Dismiss:** Abort the deployment.
* **Context Check:** Prominently display: *"WARNING: You are targeting PRODUCTION"*.

### 3.5 The "Lowering" (Execution & Watch Phase)

Once confirmed, Davit applies the changes and enters **Dashboard Mode**.

* **Technology:** Uses `kubectl apply` for the change, but switches to `kube-rs` (or parsed `kubectl get -w`) for monitoring.
* **TUI Layout (Split Screen):**
* **Top Pane (Rollout Status):** Real-time table of ReplicaSets.
* *New Pods:* `ContainerCreating` -> `Running` -> `Ready`.
* *Old Pods:* `Running` -> `Terminating`.


* **Bottom Pane (Live Log Stream):**
* **Left Column:** Tailing logs of the **terminating** pod (to catch graceful shutdown errors).
* **Right Column:** Tailing logs of the **new** pod (to catch startup crashes/boot loops).





### 3.6 The "Anchor" (Git Operations)

Upon successful rollout (New Pod is Ready, Old Pod is Gone):

1. `git add <modified_file>`
2. `git commit -m "feat(deploy): update <service> to <tag> in <env>"`
3. `git push`
4. Display: *"Deployment Successful & Config Saved."*

---

## 4. Non-Functional Requirements

### 4.1 System

* **Language:** Rust.
* **Distribution:** Single static binary.
* **OS Support:** Linux (primary), macOS (secondary).

### 4.2 Reliability & Safety

* **Atomic Revert:** If `kubectl apply` fails or the user aborts during the watch phase (Ctrl+C), Davit should offer to revert the local YAML file changes to the previous state.
* **Targeted Tag Replacement:** Deployment logic must escape image names to ensure only the intended microservice container is updated, leaving sidecars (e.g. `haproxy`) untouched.
* **Dependency Minimalist:** Should rely only on `kubectl`, `gcloud`, and `git` being present in `$PATH`.

---

## 5. Technical Architecture

### 5.1 Tech Stack (Rust Crates)

| Component | Crate | Usage |
| --- | --- | --- |
| **CLI Parser** | `clap` | Parsing arguments (`--env`, `--tag`). |
| **Config** | `config`, `serde`, `directories` | Loading TOML from XDG paths. |
| **Wizard TUI** | `inquire` | Selection lists, confirmation prompts, fuzzy search. |
| **Dashboard TUI** | `ratatui` | The complex split-screen view during rollout. |
| **K8s Interaction** | `kube` (kube-rs) | Monitoring Pod events and streaming logs programmatically. |
| **Process** | `std::process::Command` | Invoking `kubectl apply` and `git` commands. |

### 5.2 Application Flow

```mermaid
graph TD
    A[Start Davit] --> B{Config Loaded?}
    B -->|Yes| C[Parse CLI Args]
    C --> D{Missing Args?}
    D -->|Yes| E[Inquire Wizard: Env -> Service]
    E --> F[Inquire Wizard: Fetch & Select Image]
    D -->|No| F
    F --> G[Modify YAML (InMemory)]
    G --> H[Show Visual Diff]
    H --> I{User Confirm?}
    I -->|No| J[Exit / Revert]
    I -->|Yes| K[kubectl apply]
    K --> L[Ratatui Dashboard]
    L --> M{Rollout Success?}
    M -->|No| N[Show Error Logs]
    M -->|Yes| O[Git Commit & Push]
    O --> P[Exit Success]

```

---

## 6. Development Phasing

### Phase 1: The "Rigging" (MVP)

* Setup Rust project structure.
* Implement XDG Config loading.
* Implement `clap` for basic args.
* Implement `inquire` for Environment and Service selection (handling the disambiguation logic).
* Mock the Image Selection list.
* Output: Prints the `kubectl` command it *would* run.

### Phase 2: The "Winch" (Execution)

* Connect to Artifact Registry (via `gcloud` JSON output).
* Implement the YAML modification logic.
* Implement the Visual Diff.
* Execute `kubectl apply`.

### Phase 3: The "View" (Dashboard)

* Build the `ratatui` interface.
* Implement `kube-rs` watchers for Pod status.
* Implement dual-log streaming.
* Add Git automation.
