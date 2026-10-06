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
    fn helm_source(&self) {
        let repo = self.dir.path().join("repo");
        for dir in ["charts/app/templates", "clusters/test/apps", "env"] {
            fs::create_dir_all(repo.join(dir)).unwrap();
        }
        fs::write(
            repo.join("charts/app/Chart.yaml"),
            "apiVersion: v2\nname: app\nversion: 0.1.0\n",
        )
        .unwrap();
        fs::write(
            repo.join("charts/app/values.yaml"),
            "setting: old\nimage: nginx:old\n",
        )
        .unwrap();
        fs::write(
            repo.join("charts/app/templates/config.yaml"),
            "marker: initial\n{{ .Values.setting }}\n",
        )
        .unwrap();
        fs::write(repo.join("env/api.yaml"), "setting: old\n").unwrap();
        fs::write(repo.join("env/override.yaml"), "image: nginx:old\n").unwrap();
        fs::write(repo.join("clusters/test/apps/api.yaml"), "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: api\nspec:\n  destination:\n    namespace: test\n  sources:\n  - path: charts/app\n    helm:\n      releaseName: api-release\n      valueFiles:\n      - $values/env/api.yaml\n      - $values/env/override.yaml\n  - ref: values\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial chart"]);
        fs::write(self.dir.path().join("config.toml"), format!("[[environments]]\nname = 'test'\nkubectl_context = 'fake-context'\n[[environments.sources]]\nname = 'helm'\ntype = 'helm'\nrepo_root = '{}'\nhelm_cluster = 'test'\n", repo.display())).unwrap();
        let mock = self.dir.path().join("bin/helm");
        fs::write(&mock, r#"#!/bin/sh
printf 'helm:%s\n' "$*" >> "$COMMAND_LOG"
operation=$1
shift
case "$operation" in
  template) name=$1; shift; chart=$1; shift ;;
  upgrade) shift; name=$1; shift; chart=$1; shift ;;
  lint) exit 0 ;;
esac
setting=$(sed -n 's/^setting: //p' "$chart/values.yaml")
image=$(sed -n 's/^image: //p' "$chart/values.yaml")
marker=$(sed -n 's/^marker: //p' "$chart/templates/config.yaml")
secret=''
namespace=default
while [ "$#" -gt 0 ]; do
  case "$1" in
    -f) shift; value=$(sed -n 's/^setting: //p' "$1"); [ -z "$value" ] || setting=$value; value=$(sed -n 's/^image: //p' "$1"); [ -z "$value" ] || image=$value; value=$(sed -n 's/^secret: //p' "$1"); [ -z "$value" ] || secret=$value ;;
    --namespace) shift; namespace=$1 ;;
  esac
  shift
done
printf 'capture:%s:%s:%s:%s\n' "$operation" "$name" "$setting" "$image" >> "$COMMAND_LOG"
if [ "$operation" = upgrade ]; then
  if [ "$UPGRADE_FAIL" = "$name" ]; then echo private-upgrade-output >&2; exit 1; fi
  exit 0
fi
cat <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: $name-settings
  namespace: $namespace
data:
  setting: $setting
  marker: $marker
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $name
  namespace: $namespace
spec:
  template:
    spec:
      containers:
      - name: api
        image: $image
        envFrom:
        - configMapRef:
            name: $name-settings
YAML
if [ -n "$secret" ]; then
cat <<YAML
---
apiVersion: v1
kind: Secret
metadata:
  name: $name-password
stringData:
  password: $secret
YAML
fi
"#).unwrap();
        fs::set_permissions(mock, fs::Permissions::from_mode(0o755)).unwrap();
    }

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

#[test]
fn helm_applies_selected_values_once_and_preserves_unselected_changes() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    fs::write(repo.join("env/override.yaml"), "image: nginx:unselected\n").unwrap();
    fs::write(repo.join("unrelated.txt"), "staged").unwrap();
    git(&repo, &["add", "unrelated.txt"]);
    Fixture::success(
        f.command()
            .args([
                "--file",
                "env/api.yaml",
                "--message",
                "fix: helm config",
                "--yes",
                "--restart",
                "skip",
            ])
            .output()
            .unwrap(),
    );
    let log = f.log();
    assert!(log.contains("capture:upgrade:api-release:selected:nginx:old"));
    assert!(!log.contains("nginx:unselected"));
    assert!(log.contains("--kube-context fake-context --namespace test --atomic --wait"));
    assert!(
        log.lines()
            .filter(|l| l.contains(" apply "))
            .all(|l| l.contains("--dry-run=server"))
    );
    let committed = git(&repo, &["show", "--pretty=", "--name-only", "HEAD"]);
    assert_eq!(committed.trim(), "env/api.yaml");
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]).trim(),
        "unrelated.txt"
    );
    assert!(git(&repo, &["diff", "--name-only"]).contains("env/override.yaml"));
}

#[test]
fn helm_groups_selected_values_and_chart_yaml_into_one_upgrade() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    fs::write(repo.join("env/override.yaml"), "image: nginx:selected\n").unwrap();
    fs::write(
        repo.join("charts/app/templates/config.yaml"),
        "marker: changed\n{{ .Values.setting }}\n",
    )
    .unwrap();
    Fixture::success(
        f.command()
            .args([
                "--file",
                "env/api.yaml",
                "--file",
                "env/override.yaml",
                "--file",
                "charts/app/templates/config.yaml",
                "--message",
                "fix: chart",
                "--yes",
                "--restart",
                "skip",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(
        f.log()
            .lines()
            .filter(|l| l.starts_with("helm:upgrade"))
            .count(),
        1
    );
    assert!(
        f.log()
            .contains("capture:upgrade:api-release:selected:nginx:selected")
    );
    assert!(
        git(&repo, &["show", "--pretty=", "--name-only", "HEAD"])
            .contains("charts/app/templates/config.yaml")
    );
}

#[test]
fn shared_chart_changes_upgrade_all_affected_releases() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    let application = fs::read_to_string(repo.join("clusters/test/apps/api.yaml"))
        .unwrap()
        .replace("name: api", "name: worker")
        .replace("releaseName: api-release", "releaseName: worker-release");
    fs::write(repo.join("clusters/test/apps/worker.yaml"), application).unwrap();
    git(&repo, &["add", "clusters/test/apps/worker.yaml"]);
    git(&repo, &["commit", "-m", "second release"]);
    fs::write(
        repo.join("charts/app/templates/config.yaml"),
        "marker: shared-change\n{{ .Values.setting }}\n",
    )
    .unwrap();
    Fixture::success(
        f.command()
            .args([
                "--file",
                "charts/app/templates/config.yaml",
                "--message",
                "fix: shared chart",
                "--yes",
                "--restart",
                "skip",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(
        f.log()
            .lines()
            .filter(|l| l.starts_with("helm:upgrade"))
            .count(),
        2
    );
    assert!(f.log().contains("capture:upgrade:worker-release"));
}

#[test]
fn failed_helm_upgrade_keeps_edits_uncommitted_and_hides_error_payloads() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::write(
        repo.join("env/api.yaml"),
        "setting: selected\nsecret: super-private-value\n",
    )
    .unwrap();
    let output = f
        .command()
        .env("UPGRADE_FAIL", "api-release")
        .args([
            "--file",
            "env/api.yaml",
            "--message",
            "fix: helm",
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
    assert!(!printed.contains("private-upgrade-output"));
    assert!(printed.contains("automatic rollback was requested"));
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]).trim(),
        "initial chart"
    );
    assert!(
        fs::read_to_string(repo.join("env/api.yaml"))
            .unwrap()
            .contains("super-private-value")
    );
}

#[test]
fn helm_dry_run_renders_locally_without_cluster_or_upgrade() {
    let f = Fixture::new();
    f.helm_source();
    fs::write(
        f.dir.path().join("repo/env/api.yaml"),
        "setting: selected\n",
    )
    .unwrap();
    let output = f
        .command()
        .args(["--file", "env/api.yaml", "--dry-run"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("Dry-run: helm upgrade"));
    Fixture::success(output);
    assert!(
        f.log()
            .lines()
            .all(|l| l.starts_with("helm:") || l.starts_with("capture:"))
    );
    assert!(!f.log().contains("helm:upgrade"));
}

#[test]
fn rejects_unselected_application_routing_edits_before_any_cluster_access() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    let application = repo.join("clusters/test/apps/api.yaml");
    fs::write(
        &application,
        fs::read_to_string(&application)
            .unwrap()
            .replace("namespace: test", "namespace: other"),
    )
    .unwrap();
    let output = f
        .command()
        .args([
            "--file",
            "env/api.yaml",
            "--message",
            "fix: values",
            "--yes",
            "--restart",
            "skip",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("commit routing changes separately"));
    assert!(f.log().is_empty());
}

#[test]
fn mixes_explicit_manifest_subsources_and_helm_in_selected_order() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::create_dir(repo.join("k8s")).unwrap();
    f.manifest("k8s/manual.yaml", "manual");
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    let config = f.dir.path().join("config.toml");
    let mut content = fs::read_to_string(&config).unwrap();
    content.push_str(&format!(
        "[[environments.sources]]\nname = 'manifest'\ntype = 'manifest'\nrepo_root = '{}'\n",
        repo.join("k8s").display()
    ));
    fs::write(config, content).unwrap();
    Fixture::success(
        f.command()
            .args([
                "--file",
                "k8s/manual.yaml",
                "--file",
                "env/api.yaml",
                "--message",
                "fix: mixed config",
                "--yes",
                "--restart",
                "skip",
            ])
            .output()
            .unwrap(),
    );
    let log = f.log();
    let applied = log
        .lines()
        .filter(|l| l.contains(" apply ") && !l.contains("--dry-run=server"))
        .count();
    assert_eq!(applied, 1);
    assert!(log.rfind("resource:manual").unwrap() < log.find("helm:upgrade").unwrap());
    let committed = git(&repo, &["show", "--pretty=", "--name-only", "HEAD"]);
    assert!(committed.contains("env/api.yaml") && committed.contains("k8s/manual.yaml"));
}

#[test]
fn helm_configuration_restart_uses_rendered_references() {
    let f = Fixture::new();
    f.helm_source();
    fs::write(
        f.dir.path().join("repo/env/api.yaml"),
        "setting: selected\n",
    )
    .unwrap();
    let workloads = r#"{"items":[{"kind":"Deployment","metadata":{"name":"api-release","namespace":"test"},"spec":{"template":{"spec":{"containers":[{"envFrom":[{"configMapRef":{"name":"api-release-settings"}}]}]}}}}]}"#;
    Fixture::success(
        f.command()
            .env("WORKLOADS", workloads)
            .args([
                "--file",
                "env/api.yaml",
                "--message",
                "fix: settings",
                "--yes",
                "--restart",
                "all",
            ])
            .output()
            .unwrap(),
    );
    assert!(f.log().contains("rollout restart deployment/api-release"));
}

#[test]
fn multiple_helm_releases_with_changed_templates_do_not_restart_twice() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    let application = fs::read_to_string(repo.join("clusters/test/apps/api.yaml"))
        .unwrap()
        .replace("name: api", "name: worker")
        .replace("releaseName: api-release", "releaseName: worker-release");
    fs::write(repo.join("clusters/test/apps/worker.yaml"), application).unwrap();
    git(&repo, &["add", "clusters/test/apps/worker.yaml"]);
    git(&repo, &["commit", "-m", "second release"]);
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    fs::write(repo.join("env/override.yaml"), "image: nginx:new\n").unwrap();
    let items: Vec<_> = ["api-release", "worker-release"].into_iter().map(|name| serde_json::json!({
        "kind": "Deployment", "metadata": {"name": name, "namespace": "test"},
        "spec": {"template": {"spec": {"containers": [{"envFrom": [{"configMapRef": {"name": format!("{name}-settings")}}]}]}}}
    })).collect();
    let workloads = serde_json::json!({"items": items}).to_string();
    Fixture::success(
        f.command()
            .env("WORKLOADS", workloads)
            .args([
                "--file",
                "env/api.yaml",
                "--file",
                "env/override.yaml",
                "--message",
                "fix: both releases",
                "--yes",
                "--restart",
                "all",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(
        f.log()
            .lines()
            .filter(|l| l.starts_with("helm:upgrade"))
            .count(),
        2
    );
    assert!(!f.log().contains("rollout restart"));
}

#[test]
fn unrelated_unsupported_or_invalid_applications_do_not_block_valid_helm_changes() {
    for (unsupported, reason) in [
        (
            "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: cnpg\nspec:\n  sources:\n  - repoURL: https://cloudnative-pg.github.io/charts\n    chart: cloudnative-pg\n    targetRevision: 0.28.2\n    helm:\n      valueFiles:\n      - $values/env/cnpg.yaml\n  - ref: values\n",
            "Remote Helm chart 'cloudnative-pg'",
        ),
        (
            "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: other\nspec:\n  source:\n    chart: other\n",
            "spec.sources",
        ),
        ("kind: Application\nspec: [broken\n", "Invalid YAML"),
        (
            "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: inline\nspec:\n  sources:\n  - path: charts/app\n    helm:\n      valuesObject:\n        setting: inline\n",
            "inline Helm overrides",
        ),
    ] {
        let f = Fixture::new();
        f.helm_source();
        let repo = f.dir.path().join("repo");
        fs::write(
            repo.join("clusters/test/apps/unsupported.yaml"),
            unsupported,
        )
        .unwrap();
        fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
        let output = f
            .command()
            .args(["--file", "env/api.yaml", "--dry-run"])
            .output()
            .unwrap();
        let warnings = String::from_utf8_lossy(&output.stderr);
        assert!(
            warnings.contains("Skipping")
                && warnings.contains("unsupported.yaml")
                && warnings.contains(reason),
            "{warnings}"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Helm api-release"));
        Fixture::success(output);
        assert!(!f.log().contains("helm:upgrade"));
        assert!(!f.log().contains(" apply "));
    }
}

#[test]
fn valid_application_after_unsupported_document_in_same_file_is_discovered() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    let path = repo.join("clusters/test/apps/api.yaml");
    let valid = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("kind: Application\nspec:\n  sources:\n  - chart: unsupported\n---\n{valid}"),
    )
    .unwrap();
    git(&repo, &["add", "clusters/test/apps/api.yaml"]);
    git(&repo, &["commit", "-m", "mixed application documents"]);
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    let output = f
        .command()
        .args(["--file", "env/api.yaml", "--dry-run"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("Remote Helm chart 'unsupported'"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Helm api-release"));
    Fixture::success(output);
}

#[test]
fn unsupported_remote_chart_values_cannot_be_applied() {
    let f = Fixture::new();
    f.helm_source();
    let repo = f.dir.path().join("repo");
    fs::write(repo.join("clusters/test/apps/cnpg.yaml"), "kind: Application\nspec:\n  sources:\n  - chart: cloudnative-pg\n    helm:\n      valueFiles:\n      - $values/env/cnpg.yaml\n").unwrap();
    fs::write(repo.join("env/cnpg.yaml"), "setting: remote\n").unwrap();
    fs::write(repo.join("env/api.yaml"), "setting: selected\n").unwrap();
    let output = f
        .command()
        .args(["--file", "env/cnpg.yaml", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not an unambiguous modified manifest")
    );
    assert!(f.log().is_empty());
}
