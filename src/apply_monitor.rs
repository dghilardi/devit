use anyhow::{Context, Result};
use serde_json::Value;

use crate::apply::{Manifest, Resource, documents};
use crate::config::Environment;
use crate::dashboard::{Dashboard, DashboardExit};
use crate::prompt::PromptPolicy;

fn kubectl(env: &Environment, resource: &Resource) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("kubectl");
    command.args(["--context", &env.kubectl_context, "-n", &resource.namespace]);
    command
}

fn target(resource: &Resource) -> String {
    format!("{}/{}", resource.kind.to_lowercase(), resource.name)
}

fn selector(workload: &Value) -> Result<String> {
    let mut expressions = Vec::new();
    if let Some(labels) = workload["spec"]["selector"]["matchLabels"].as_object() {
        expressions.extend(
            labels
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| format!("{key}={value}"))
                        .context("Invalid workload label selector")
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }
    if let Some(selectors) = workload["spec"]["selector"]["matchExpressions"].as_array() {
        for expression in selectors {
            let key = expression["key"].as_str().context("Missing selector key")?;
            let values = expression["values"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().context("Invalid selector value"))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default()
                .join(",");
            expressions.push(match expression["operator"].as_str() {
                Some("In") => format!("{key} in ({values})"),
                Some("NotIn") => format!("{key} notin ({values})"),
                Some("Exists") => key.into(),
                Some("DoesNotExist") => format!("!{key}"),
                _ => anyhow::bail!("Unsupported workload selector operator"),
            });
        }
    }
    anyhow::ensure!(
        !expressions.is_empty(),
        "Cannot monitor a workload without a pod selector"
    );
    Ok(expressions.join(","))
}

fn dashboard(
    env: &Environment,
    resource: &Resource,
    workload: &Value,
    restart: bool,
) -> Result<Dashboard> {
    let template = &workload["spec"]["template"];
    let containers = template["spec"]["containers"]
        .as_array()
        .context("Workload has no containers to monitor")?;
    let default_container =
        template["metadata"]["annotations"]["kubectl.kubernetes.io/default-container"].as_str();
    let container = containers
        .iter()
        .find(|container| {
            container["name"].as_str() == default_container && default_container.is_some()
        })
        .or_else(|| {
            containers.iter().find(|container| {
                container["name"].as_str() == Some(&resource.name)
                    || container["name"].as_str() == Some("app")
            })
        })
        .or_else(|| containers.first())
        .and_then(|container| container["name"].as_str())
        .context("Workload has no named container")?;
    Ok(Dashboard::new(
        resource.name.clone(),
        resource.kind.clone(),
        env.name.clone(),
        if restart {
            "configuration restart".into()
        } else {
            "configuration update".into()
        },
        env.kubectl_context.clone(),
        Some(resource.namespace.clone()),
        Some(selector(workload)?),
        container.into(),
        false,
    )
    .with_configuration_template(template.clone()))
}

fn require_completion(result: DashboardExit) -> Result<()> {
    match result {
        DashboardExit::RolloutCompleted => Ok(()),
        DashboardExit::UserQuit => anyhow::bail!(
            "Dashboard closed before rollout confirmation; local edits retained without committing"
        ),
    }
}

async fn live_workload(env: &Environment, resource: &Resource) -> Result<Value> {
    let output = kubectl(env, resource)
        .args(["get", &target(resource), "-o", "json"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "Cannot read {} for rollout monitoring (command output hidden)",
        target(resource)
    );
    serde_json::from_slice(&output.stdout).context("Invalid workload response")
}

pub async fn rollout(
    env: &Environment,
    resource: &Resource,
    restart: bool,
    policy: PromptPolicy,
) -> Result<()> {
    if restart {
        let status = kubectl(env, resource)
            .args(["rollout", "restart", &target(resource)])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "Restart failed for {}", target(resource));
    }
    if policy.is_interactive() {
        let workload = live_workload(env, resource).await?;
        let mut dashboard = dashboard(env, resource, &workload, restart)?;
        require_completion(dashboard.run().await?)?;
    } else {
        let status = kubectl(env, resource)
            .args(["rollout", "status", &target(resource), "--timeout=300s"])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "Rollout failed for {}", target(resource));
    }
    Ok(())
}

pub async fn helm(
    env: &Environment,
    manifest: &Manifest,
    policy: PromptPolicy,
) -> Result<Option<Resource>> {
    let release = manifest.helm.first().context("Missing Helm release")?;
    if !policy.is_interactive() {
        crate::apply_helm::upgrade(release, &env.kubectl_context).await?;
        return Ok(None);
    }
    let primary = manifest
        .resources
        .iter()
        .find(|resource| resource.workload() && resource.name == release.name)
        .or_else(|| {
            manifest
                .resources
                .iter()
                .find(|resource| resource.workload())
        });
    let Some(resource) = primary else {
        crate::apply_helm::upgrade(release, &env.kubectl_context).await?;
        return Ok(None);
    };
    let rendered = documents(&manifest.content)?;
    let workload = rendered
        .iter()
        .find(|doc| {
            doc["kind"].as_str() == Some(&resource.kind)
                && doc["metadata"]["name"].as_str() == Some(&resource.name)
                && doc["metadata"]["namespace"]
                    .as_str()
                    .unwrap_or(&resource.namespace)
                    == resource.namespace
        })
        .context("Missing rendered Helm workload")?;
    let mut dashboard = dashboard(env, resource, &serde_json::to_value(workload)?, false)?;
    let release = release.clone();
    let context = env.kubectl_context.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let operation = tokio::spawn(async move {
        let result = crate::apply_helm::upgrade(&release, &context).await;
        let _ = sender.send(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| error.to_string()),
        );
        result
    });
    dashboard.monitor_deployment(receiver);
    let monitoring = dashboard.run().await;
    if !operation.is_finished() {
        println!(
            "Logs closed. Waiting for Helm to finish, including automatic rollback on failure..."
        );
    }
    finish_helm(operation, monitoring).await?;
    Ok(Some(resource.clone()))
}

async fn finish_helm(
    operation: tokio::task::JoinHandle<Result<()>>,
    monitoring: Result<DashboardExit>,
) -> Result<()> {
    operation.await.context("Helm upgrade task failed")??;
    require_completion(monitoring.context("Helm may have succeeded; rollout monitoring failed, local edits retained without committing")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_supports_labels_and_set_based_expressions() {
        let workload = serde_json::json!({"spec": {"selector": {
            "matchLabels": {"app.kubernetes.io/name": "api"},
            "matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["backend", "worker"]},
                {"key": "legacy", "operator": "DoesNotExist"}
            ]
        }}});
        assert_eq!(
            selector(&workload).unwrap(),
            "app.kubernetes.io/name=api,tier in (backend,worker),!legacy"
        );
        assert!(selector(&serde_json::json!({})).is_err());
    }

    #[tokio::test]
    async fn closing_logs_waits_for_helm_and_preserves_unconfirmed_state() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(async move {
            receiver.await.unwrap();
            Ok(())
        });
        let finish = finish_helm(operation, Ok(DashboardExit::UserQuit));
        tokio::pin!(finish);
        tokio::select! {
            result = &mut finish => panic!("Returned before Helm finished: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        sender.send(()).unwrap();
        assert!(
            finish
                .await
                .unwrap_err()
                .to_string()
                .contains("without committing")
        );
    }

    #[tokio::test]
    async fn helm_failure_takes_precedence_over_dashboard_exit() {
        let operation = tokio::spawn(async { anyhow::bail!("Helm failed after rollback") });
        let error = finish_helm(operation, Ok(DashboardExit::UserQuit))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("after rollback"));
    }

    #[tokio::test]
    async fn confirmed_dashboard_waits_for_successful_helm() {
        let operation = tokio::spawn(async { Ok(()) });
        finish_helm(operation, Ok(DashboardExit::RolloutCompleted))
            .await
            .unwrap();
    }
}
