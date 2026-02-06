# Davit Implementation Plan: From Zero to Release

This document outlines the incremental, step-by-step implementation plan for Davit. Each step is designed to be independently verifiable.

---

## Phase 1: The Skeleton (Foundations)

Goal: A CLI tool that can be installed and correctly identifies its environment.

### 1.1 Project Initialization
- **Task:** Setup Cargo project with initial dependencies (`clap`, `serde`, `directories`, `anyhow`).
- **Verification:** `cargo run -- --version` returns a valid version.

### 1.2 Configuration Layer (XDG)
- **Task:** Implement XDG configuration loading from `$XDG_CONFIG_HOME/davit/config.toml`. Define the `Environment` and `Defaults` structs.
- **Verification:** Create a dummy `config.toml`. Run a command like `davit config show` that prints the parsed configuration.

### 1.3 Basic CLI Arguments
- **Task:** Define `clap` commands for `deploy`. Wire up `--env`, `--service`, and `--tag`.
- **Verification:** `davit deploy --env staging --service auth` correctly captures and prints these values to the terminal.

---

## Phase 2: The Navigator (Interactive Wizard)

Goal: Seamlessly resolve inputs when they are missing or ambiguous.

### 2.1 Basic Selection Wizard
- **Task:** Integrate `inquire`. If `--env` or `--service` are missing, show a selection list derived from the configuration.
- **Verification:** Run `davit deploy` without arguments; it should prompt for Environment, then Service.

### 2.2 Smart Disambiguation
- **Task:** Implement the disambiguation logic:
    - Exact match: Skip prompt.
    - Partial match (unique): "Did you mean...?" prompt.
    - Ambiguous: Filtered selection list.
- **Verification:** Try `davit deploy --service sh` (matching `shipping`) and `davit deploy --service api` (matching multiple). Verify correct prompts appear.

---

## Phase 3: The Cargo (Artifact Registry)

Goal: Dynamically retrieve and select deployment artifacts.

### 3.1 GCloud Wrapper
- **Task:** Implement a module to execute `gcloud artifacts docker images list --format=json`. Parse result.
- **Verification:** Run `davit images list` and see a list of real image tags from the registry.

### 3.2 Metadata-Rich Selection
- **Task:** Display Tag, Age, and Hash in the `inquire` list. Sort by newest first.
- **Verification:** Run the image selection step; verify "2 hours ago" or similar age strings appear correctly.

---

## Phase 4: The Blueprint (YAML & Diffs)

Goal: Safely prepare and visualize infrastructure changes.

### 4.1 YAML Modification
- **Task:** Map services to local YAML paths. Implement Logic to update the `image:` tag while preserving comments/structure (using `serde_yaml` or similar).
- **Verification:** Run a mock deploy; check that the local `deployment.yaml` has been updated with the new tag.

### 4.2 Visual Diff
- **Task:** Implement a terminal diff viewer (colored unified diff).
- **Verification:** After selecting a tag, the CLI clears the screen and shows exactly which lines in the YAML changed.

---

## Phase 5: The Watchtower (Rollout Dashboard)

Goal: Monitor the deployment in real-time.

### 5.1 Deployment Execution
- **Task:** Integrate `std::process::Command` to run `kubectl apply -f <path>`. Capture errors.
- **Verification:** Run a deploy and verify `kubectl` receives the updated YAML.

### 5.2 Ratatui Dashboard Layout
- **Task:** Build the `ratatui` UI structure: Rollout status (top) and Log stream (bottom).
- **Verification:** A static dashboard renders with placeholder data.

### 5.3 Kubernetes Watchers (`kube-rs`)
- **Task:** Use `kube-rs` to watch Pod events in the target namespace/service. Map Events to the TUI.
- **Verification:** Start a deployment; see the TUI update as pods transition from `Pending` to `Running`.

### 5.4 Live Log Streaming
- **Task:** Implement dual-column log tailing. Left: Old Pod (terminating), Right: New Pod (starting).
- **Verification:** Deployment TUI shows intermingled logs from both versions during the handoff.

---

## Phase 6: The Anchor (Automation & Safety)

Goal: Finalize the state and provide safety nets.

### 6.1 Git Automation
- **Task:** Implement `git add`, `git commit`, and `git push` upon successful rollout detection.
- **Verification:** Check the Git log after a successful deployment.

### 6.2 Atomic Revert & Cleanup
- **Task:** Implement a signal handler (Ctrl+C) and error handling that offers to revert the local YAML changes if the deployment is aborted or fails.
- **Verification:** Interrupt a rollout; verify the local YAML is restored to its previous state.

### 6.3 Production Protection
- **Task:** Implement the `protected = true` check (type environment name to confirm).
- **Verification:** Try deploying to an environment marked `protected` and verify the mandatory text input confirmation.
