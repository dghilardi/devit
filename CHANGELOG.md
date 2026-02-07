# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Refactored `repo_root` to `env_yaml_dir` in configuration for better clarity.
- Improved Git repository detection to correctly identify repositories when operating from subdirectories.
- Integrated an automatic `git pull` as the first step of the deployment process, with a user prompt on failure.
- Implemented a commit recap and confirmation prompt before pushing deployment changes to Git.
- Added a `--dry-run` flag to the `deploy` command to preview actions without executing them.
- Initial project structure.
- Requirements document (`docs/REQUIREMENTS.md`).
- Project documentation: `README.md`, `CHANGELOG.md`, and `AGENT.md`.
- Coding standards and best practices guidelines.
- Versioning strategy definition (Trunk-based development).
