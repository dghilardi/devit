# Agent Guidelines for Davit

This document identifies the best practices, coding standards, and workflows that all AI agents (including Antigravity) must follow when contributing to the Davit project.

## 🛠 Tech Stack

- **Language:** Rust (Latest Stable)
- **Deployment Platform:** Kubernetes (GKE)
- **IaC Repo Structure:** YAML files organized by environment and service.
- **Key Crates:** `clap`, `ratatui`, `kube-rs`, `inquire`, `tokio`, `serde`, `anyhow`.

## 📐 Coding Standards & Best Practices

1.  **Format and Lints:**
    - Always run `cargo fmt` and `cargo clippy` before finalizing any code changes.
    - Adhere strictly to the standard Rust formatting.
2.  **Error Handling:**
    - Use `anyhow` for top-level application errors or CLI execution logic.
    - Use `thiserror` for library-level or domain-specific errors.
    - Avoid `unwrap()` or `expect()` unless it is mathematically impossible to fail.
3.  **Concurrency:**
    - Use `tokio` for asynchronous operations, especially when interacting with Kubernetes APIs via `kube-rs`.
4.  **TUI Design:**
    - Keep the `ratatui` dashboard responsive and clean.
    - Use `inquire` for interactive prompts to ensure a standard user experience.
5.  **Project Context:**
    - Always refer to `docs/REQUIREMENTS.md` before implementing a new feature to ensure compliance with the original vision.

## 🌿 Versioning & Branching Strategy

We follow a **Trunk-Based Development** model with short-lived **Feature Branches**.

- **Trunk-Based Development:** The `main` branch must always remain in a deployable state.
- **Feature Branches:** 
    - Create a branch for every new feature or bug fix (e.g., `feature/add-log-tailing` or `fix/config-xdg-path`).
    - Merge into `main` via Pull Requests.
    - Branches should be deleted immediately after merging.
- **Commits:** Follow conventional commit messages (e.g., `feat: ...`, `fix: ...`, `docs: ...`).

## 📑 Required Documentation Updates

Whenever a feature is added, removed, or modified, you **MUST** update the following:

1.  **CHANGELOG.md:** Add a New entry under the `[Unreleased]` section following the [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) format.
2.  **README.md:** If usage, configuration, or prerequisites change, update the README accordingly.
3.  **docs/REQUIREMENTS.md:** If a functional requirement is fulfilled or shifted, update the PRD to reflect the current state if appropriate.

## 🤖 Instructions for Agents

- **Proactivity:** Be proactive in suggesting improvements but never skip verification.
- **Verification:** Always verify that documentation (README, CHANGELOG) reflects the current state of the codebase after your changes.
- **Context Awareness:** Before starting any work, read `AGENT.md` (this file) and `docs/REQUIREMENTS.md`.
