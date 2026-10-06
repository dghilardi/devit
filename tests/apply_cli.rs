#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

struct Fixture {
    dir: tempfile::TempDir,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for name in ["repo", "remote", "bin"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let repo = dir.path().join("repo");
        git(&dir.path().join("remote"), &["init", "--bare"]);
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Davit Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("README"), "initial").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                dir.path().join("remote").to_str().unwrap(),
            ],
        );
        git(&repo, &["push", "-u", "origin", "HEAD"]);
        fs::write(dir.path().join("config.toml"), format!("[[environments]]\nname = 'test'\nkubectl_context = 'fake-context'\nenv_yaml_dir = '{}'\n", repo.display())).unwrap();
        let mock = dir.path().join("bin/kubectl");
        fs::write(&mock, r#"#!/bin/sh
printf '%s\n' "$*" >> "$COMMAND_LOG"
case "$*" in
  *"config view"*) printf 'test'; exit 0 ;;
  *"get deployments,statefulsets,daemonsets"*) printf '%s' "$WORKLOADS"; exit 0 ;;
  *"apply "*)
    for last do :; done
    name=$(sed -n 's/^  name: //p' "$last" | head -1)
    printf 'resource:%s\n' "$name" >> "$COMMAND_LOG"
    case "$*" in
      *--dry-run=server*) if [ "$VALIDATION_FAIL" = "$name" ]; then echo "sensitive-output" >&2; exit 1; fi ;;
      *) if [ "$APPLY_FAIL" = "$name" ]; then echo "sensitive-output" >&2; exit 1; fi ;;
    esac ;;
esac
exit 0
"#).unwrap();
        fs::set_permissions(mock, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn manifest(&self, file: &str, name: &str) {
        fs::write(
            self.dir.path().join("repo").join(file),
            format!(
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\ndata:\n  key: value\n"
            ),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_davit"));
        command
            .current_dir(self.dir.path().join("repo"))
            .env("DAVIT_CONFIG", self.dir.path().join("config.toml"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("COMMAND_LOG", self.dir.path().join("commands"))
            .env("WORKLOADS", "{\"items\":[]}")
            .args(["apply", "--env", "test", "--no-fetch", "--non-interactive"]);
        command
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("commands")).unwrap_or_default()
    }

    fn success(output: Output) {
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn dry_run_does_not_run_kubectl_or_commit() {
    let f = Fixture::new();
    f.manifest("one.yaml", "one");
    Fixture::success(
        f.command()
            .args(["--file", "one.yaml", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert!(f.log().is_empty());
    assert_eq!(
        git(&f.dir.path().join("repo"), &["log", "-1", "--format=%s"]).trim(),
        "initial"
    );
}

#[test]
fn validates_all_files_then_applies_in_requested_order_and_pushes_one_commit() {
    let f = Fixture::new();
    f.manifest("one.yaml", "one");
    f.manifest("two.yaml", "two");
    fs::write(f.dir.path().join("repo/unrelated.txt"), "staged").unwrap();
    git(&f.dir.path().join("repo"), &["add", "unrelated.txt"]);
    Fixture::success(
        f.command()
            .args([
                "--file",
                "two.yaml",
                "--file",
                "one.yaml",
                "--message",
                "feat: change settings",
                "--yes",
                "--restart",
                "skip",
            ])
            .output()
            .unwrap(),
    );
    let log = f.log();
    let resources: Vec<_> = log.lines().filter(|l| l.starts_with("resource:")).collect();
    assert_eq!(
        resources,
        [
            "resource:two",
            "resource:one",
            "resource:two",
            "resource:one"
        ]
    );
    let committed = git(
        &f.dir.path().join("repo"),
        &["show", "--pretty=", "--name-only", "HEAD"],
    );
    assert!(committed.contains("one.yaml") && committed.contains("two.yaml"));
    assert!(!committed.contains("unrelated"));
    assert_eq!(
        git(
            &f.dir.path().join("repo"),
            &["diff", "--cached", "--name-only"]
        )
        .trim(),
        "unrelated.txt"
    );
    assert_eq!(
        git(&f.dir.path().join("repo"), &["rev-parse", "HEAD"]),
        git(&f.dir.path().join("repo"), &["rev-parse", "@{upstream}"])
    );
}

#[test]
fn validation_failure_prevents_any_application() {
    let f = Fixture::new();
    f.manifest("one.yaml", "one");
    f.manifest("two.yaml", "two");
    let result = f
        .command()
        .env("VALIDATION_FAIL", "two")
        .args([
            "--file",
            "one.yaml",
            "--file",
            "two.yaml",
            "--message",
            "fix: settings",
            "--yes",
            "--restart",
            "skip",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        f.log()
            .lines()
            .filter(|l| l.contains(" apply "))
            .all(|l| l.contains("--dry-run=server"))
    );
    assert_eq!(
        git(&f.dir.path().join("repo"), &["log", "-1", "--format=%s"]).trim(),
        "initial"
    );
}

#[test]
fn partial_application_stops_and_keeps_edits_uncommitted() {
    let f = Fixture::new();
    for name in ["one", "two", "three"] {
        f.manifest(&format!("{name}.yaml"), name);
    }
    let result = f
        .command()
        .env("APPLY_FAIL", "two")
        .args([
            "--file",
            "one.yaml",
            "--file",
            "two.yaml",
            "--file",
            "three.yaml",
            "--message",
            "fix: settings",
            "--yes",
            "--restart",
            "skip",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("sensitive-output"));
    assert_eq!(
        f.log().lines().filter(|l| *l == "resource:three").count(),
        1
    );
    assert_eq!(
        git(&f.dir.path().join("repo"), &["log", "-1", "--format=%s"]).trim(),
        "initial"
    );
    assert!(f.dir.path().join("repo/two.yaml").exists());
}

#[test]
fn missing_noninteractive_restart_decision_fails_before_cluster_access() {
    let f = Fixture::new();
    f.manifest("one.yaml", "one");
    let output = f
        .command()
        .args(["--file", "one.yaml", "--message", "fix: settings", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--restart skip"));
    assert!(f.log().is_empty());
}

#[test]
fn restarts_only_referencing_workloads_then_waits_for_rollout() {
    let f = Fixture::new();
    f.manifest("one.yaml", "one");
    let workloads = r#"{"items":[{"kind":"Deployment","metadata":{"name":"api","namespace":"test"},"spec":{"template":{"spec":{"containers":[{"envFrom":[{"configMapRef":{"name":"one"}}]}]}}}},{"kind":"Deployment","metadata":{"name":"unrelated","namespace":"test"},"spec":{"template":{"spec":{}}}}]}"#;
    Fixture::success(
        f.command()
            .env("WORKLOADS", workloads)
            .args([
                "--file",
                "one.yaml",
                "--message",
                "fix: config",
                "--yes",
                "--restart",
                "all",
            ])
            .output()
            .unwrap(),
    );
    let log = f.log();
    assert!(log.contains("-n test rollout restart deployment/api"));
    assert!(log.contains("-n test rollout status deployment/api --timeout=300s"));
    assert!(!log.contains("deployment/unrelated"));
}

#[test]
fn secret_validation_errors_do_not_expose_values() {
    let f = Fixture::new();
    fs::write(f.dir.path().join("repo/secret.yaml"), "apiVersion: v1\nkind: Secret\nmetadata:\n  name: password\nstringData:\n  key: super-private-value\n").unwrap();
    let output = f
        .command()
        .env("VALIDATION_FAIL", "password")
        .args([
            "--file",
            "secret.yaml",
            "--message",
            "fix: secret",
            "--yes",
            "--restart",
            "skip",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains("super-private-value"));
    assert!(!printed.contains("sensitive-output"));
    assert!(printed.contains("Secret validation output hidden"));
}

#[test]
fn avoids_duplicate_restart_only_when_template_changed_after_configuration() {
    for (changed, workload_first, expected_restart) in [
        (true, false, false),
        (false, false, true),
        (true, true, true),
    ] {
        let f = Fixture::new();
        let deployment = |image| {
            format!(
                "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\nspec:\n  template:\n    spec:\n      containers:\n      - name: api\n        image: {image}\n        envFrom:\n        - configMapRef:\n            name: one\n"
            )
        };
        fs::write(f.dir.path().join("repo/api.yaml"), deployment("old")).unwrap();
        git(&f.dir.path().join("repo"), &["add", "api.yaml"]);
        git(
            &f.dir.path().join("repo"),
            &["commit", "-m", "initial workload"],
        );
        fs::write(
            f.dir.path().join("repo/api.yaml"),
            if changed {
                deployment("new")
            } else {
                format!("# manual comment\n{}", deployment("old"))
            },
        )
        .unwrap();
        f.manifest("one.yaml", "one");
        let workloads = r#"{"items":[{"kind":"Deployment","metadata":{"name":"api","namespace":"test"},"spec":{"template":{"spec":{"containers":[{"envFrom":[{"configMapRef":{"name":"one"}}]}]}}}}]}"#;
        let (first, second) = if workload_first {
            ("api.yaml", "one.yaml")
        } else {
            ("one.yaml", "api.yaml")
        };
        Fixture::success(
            f.command()
                .env("WORKLOADS", workloads)
                .args([
                    "--file",
                    first,
                    "--file",
                    second,
                    "--message",
                    "fix: config",
                    "--yes",
                    "--restart",
                    "all",
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(
            f.log().contains("rollout restart deployment/api"),
            expected_restart
        );
    }
}
