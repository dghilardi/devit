# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Added validation for provided deploy tags against the registry tag list, including a `--wait-for-tag <tag>` mode with cleaner live status feedback while polling for the exact tag before continuing.
- Updated multi-repo YAML source refresh to run `git pull` in parallel batches of up to five repositories while keeping each repository's output grouped and ordered.
- Added a `deploy --auto-continue` mode that, after `kubectl apply`, keeps the rollout dashboard open until completion and then proceeds automatically through diff recap and Git push unless errors occur.
- Added rollout completion detection to the post-release dashboard and a confirmation modal to either continue to the next step or keep monitoring.
- Adjusted the post-release log dashboard so the pod status panel grows with the number of pods while keeping consistent minimum and maximum heights.

## [0.2.2] 2026-06-16

### Changed
- Reduced dashboard log rendering jitter by batching updates more efficiently and avoiding unnecessary redraws.
- Updated dashboard log panes to render like `kubectl logs`, with the newest lines appearing at the bottom.

## [0.2.1] 2026-06-15

### Changed
- Improved dashboard responsiveness so quitting with `q` stays immediate even when Kubernetes pod refreshes are slow.

## [0.2.0] 2026-06-11

### Changed
- Added support for named `env_yaml_dir_extra` YAML sources, with tolerant `git pull` across all configured sources and source-aware Git/file display behavior for multi-repo environments.
- Updated `davit info` to run `git pull` before loading services and to show a YAML vs cluster image comparison, highlighting version drift when present.

## [0.1.3] 2026-03-26

## [0.1.2] 2026-03-26

## [0.1.1] 2026-03-26

### Added
- Added `davit info` command to inspect deployed services: shows workload status, current image, last release commit (from git), labels/annotations, pod details, resources, and recent events.
- Added `--namespace` flag to `info` command for filtering services by Kubernetes namespace.
- Added `kind` field to `ServiceSource` for workload-type-aware K8s API queries.
- Added `Git::last_commit_for_file()` to retrieve the last commit that modified a specific file.
- Added Arch Linux AUR packaging files in `packaging/arch`.
- Refactored `repo_root` to `env_yaml_dir` in configuration for better clarity.
- Improved Git repository detection to correctly identify repositories when operating from subdirectories.
- Integrated an automatic `git pull` as the first step of the deployment process, with a user prompt on failure.
- Implemented a commit recap and confirmation prompt before pushing deployment changes to Git.
- Added a `--dry-run` flag to the `deploy` command to preview actions without executing them.
- Improved microservice disambiguation in selection menu (showing namespace and relative path for duplicates).
- Initial project structure.
- Requirements document (`docs/REQUIREMENTS.md`).
- Project documentation: `README.md`, `CHANGELOG.md`, and `AGENT.md`.
- Coding standards and best practices guidelines.
- Versioning strategy definition (Trunk-based development).
