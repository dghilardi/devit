use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use console::style;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{Event, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::ListParams;
use kube::config::KubeConfigOptions;
use kube::{Api, Client};
use std::collections::BTreeMap;

use crate::config::{Environment, ServiceSource};
use crate::git::Git;

/// Convert a k8s Time (jiff::Timestamp) to chrono::DateTime<Utc>.
fn to_chrono(t: &Time) -> DateTime<Utc> {
    let ts = &t.0;
    DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32).unwrap_or_default()
}

struct WorkloadInfo {
    kind: String,
    name: String,
    creation_time: Option<DateTime<Utc>>,
    desired_replicas: Option<i32>,
    ready_replicas: Option<i32>,
    available_replicas: Option<i32>,
    updated_replicas: Option<i32>,
    update_strategy: Option<String>,
    conditions: Vec<WorkloadCondition>,
    running_images: Vec<String>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
}

struct WorkloadCondition {
    condition_type: String,
    status: String,
}

struct PodDetail {
    name: String,
    phase: String,
    node: Option<String>,
    start_time: Option<DateTime<Utc>>,
    restart_count: i32,
    ready: bool,
    container_image: String,
    resource_requests: Option<ResourceSpec>,
    resource_limits: Option<ResourceSpec>,
}

struct ResourceSpec {
    cpu: Option<String>,
    memory: Option<String>,
}

struct GitCommitInfo {
    hash: String,
    author: String,
    date: String,
    message: String,
}

struct EventInfo {
    event_type: String,
    reason: String,
    message: String,
    count: Option<i32>,
    last_seen: Option<DateTime<Utc>>,
}

struct ServiceInfo {
    workload: WorkloadInfo,
    namespace: String,
    pods: Vec<PodDetail>,
    last_commit: Option<GitCommitInfo>,
    events: Vec<EventInfo>,
    image_comparison: ImageComparison,
}

struct ImageComparison {
    yaml_image: String,
    cluster_images: Vec<String>,
    drift: bool,
}

pub async fn show_info(env: &Environment, service: &ServiceSource) -> Result<()> {
    let ns = service.namespace.as_deref().unwrap_or("default");

    // Build kube client
    let options = KubeConfigOptions {
        context: Some(env.kubectl_context.clone()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options)
        .await
        .context("Failed to load kubeconfig")?;
    let client = Client::try_from(config).context("Failed to create Kubernetes client")?;

    // Fetch all data
    let workload = fetch_workload_info(&client, ns, service).await?;
    let pods = fetch_pod_details(&client, ns, service).await?;
    let events = fetch_events(&client, ns, &service.name).await;
    let last_commit = fetch_git_info(service);
    let image_comparison = build_image_comparison(service, &workload.running_images);

    let info = ServiceInfo {
        workload,
        namespace: ns.to_string(),
        pods,
        last_commit,
        events,
        image_comparison,
    };

    print_info(&info, service, env);
    Ok(())
}

fn normalize_image_ref(image: &str) -> String {
    image.trim().to_ascii_lowercase()
}

fn split_image_ref(image: &str) -> (&str, Option<&str>) {
    let trimmed = image.trim();

    if let Some((base, digest)) = trimmed.split_once('@') {
        return (base, Some(digest));
    }

    if let Some((base, tag)) = trimmed.rsplit_once(':')
        && !tag.contains('/')
    {
        return (base, Some(tag));
    }

    (trimmed, None)
}

fn same_image_ref(lhs: &str, rhs: &str) -> bool {
    let (lhs_base, lhs_ver) = split_image_ref(lhs);
    let (rhs_base, rhs_ver) = split_image_ref(rhs);

    normalize_image_ref(lhs_base) == normalize_image_ref(rhs_base) && lhs_ver == rhs_ver
}

fn build_image_comparison(service: &ServiceSource, running_images: &[String]) -> ImageComparison {
    let yaml_image = service.image_path.clone();
    let cluster_images = running_images.to_vec();
    let drift = if cluster_images.is_empty() {
        false
    } else {
        !cluster_images
            .iter()
            .any(|cluster_img| same_image_ref(cluster_img, &yaml_image))
    };

    ImageComparison {
        yaml_image,
        cluster_images,
        drift,
    }
}

async fn fetch_workload_info(
    client: &Client,
    namespace: &str,
    service: &ServiceSource,
) -> Result<WorkloadInfo> {
    match service.kind.as_str() {
        "Deployment" => {
            let api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
            let dep = api
                .get(&service.name)
                .await
                .context("Failed to fetch Deployment from cluster")?;
            Ok(extract_deployment_info(dep))
        }
        "StatefulSet" => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
            let sts = api
                .get(&service.name)
                .await
                .context("Failed to fetch StatefulSet from cluster")?;
            Ok(extract_statefulset_info(sts))
        }
        "DaemonSet" => {
            let api: Api<DaemonSet> = Api::namespaced(client.clone(), namespace);
            let ds = api
                .get(&service.name)
                .await
                .context("Failed to fetch DaemonSet from cluster")?;
            Ok(extract_daemonset_info(ds))
        }
        "Job" => {
            let api: Api<Job> = Api::namespaced(client.clone(), namespace);
            let job = api
                .get(&service.name)
                .await
                .context("Failed to fetch Job from cluster")?;
            Ok(extract_job_info(job))
        }
        "CronJob" => {
            let api: Api<CronJob> = Api::namespaced(client.clone(), namespace);
            let cj = api
                .get(&service.name)
                .await
                .context("Failed to fetch CronJob from cluster")?;
            Ok(extract_cronjob_info(cj))
        }
        other => Err(anyhow::anyhow!("Unsupported workload kind: {}", other)),
    }
}

fn extract_labels_and_annotations(
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let labels = meta.labels.clone().unwrap_or_default();

    let annotations = meta
        .annotations
        .as_ref()
        .map(|a| {
            a.iter()
                .filter(|(k, _)| {
                    !k.starts_with("kubectl.kubernetes.io/")
                        && !k.starts_with("deployment.kubernetes.io/")
                        && *k != "meta.helm.sh/release-name"
                        && *k != "meta.helm.sh/release-namespace"
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    (labels, annotations)
}

fn extract_deployment_info(dep: Deployment) -> WorkloadInfo {
    let meta = dep.metadata;
    let (labels, annotations) = extract_labels_and_annotations(&meta);

    let spec = dep.spec;
    let status = dep.status;

    let strategy = spec.as_ref().and_then(|s| {
        s.strategy.as_ref().and_then(|st| {
            st.type_.as_ref().map(|t| {
                if t == "RollingUpdate" {
                    st.rolling_update
                        .as_ref()
                        .map(|r| {
                            format!(
                                "RollingUpdate (maxSurge: {}, maxUnavailable: {})",
                                r.max_surge
                                    .as_ref()
                                    .map(|v| format!("{v:?}"))
                                    .unwrap_or_else(|| "-".to_string()),
                                r.max_unavailable
                                    .as_ref()
                                    .map(|v| format!("{v:?}"))
                                    .unwrap_or_else(|| "-".to_string())
                            )
                        })
                        .unwrap_or_else(|| "RollingUpdate".to_string())
                } else {
                    t.clone()
                }
            })
        })
    });

    let conditions = status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| {
            conds
                .iter()
                .map(|c| WorkloadCondition {
                    condition_type: c.type_.clone(),
                    status: c.status.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let running_images = spec
        .as_ref()
        .and_then(|s| {
            s.template.spec.as_ref().map(|ps| {
                ps.containers
                    .iter()
                    .filter_map(|c| c.image.clone())
                    .collect()
            })
        })
        .unwrap_or_default();

    WorkloadInfo {
        kind: "Deployment".to_string(),
        name: meta.name.unwrap_or_default(),
        creation_time: meta.creation_timestamp.as_ref().map(to_chrono),
        desired_replicas: spec.and_then(|s| s.replicas),
        ready_replicas: status.as_ref().and_then(|s| s.ready_replicas),
        available_replicas: status.as_ref().and_then(|s| s.available_replicas),
        updated_replicas: status.as_ref().and_then(|s| s.updated_replicas),
        update_strategy: strategy,
        conditions,
        running_images,
        labels,
        annotations,
    }
}

fn extract_statefulset_info(sts: StatefulSet) -> WorkloadInfo {
    let meta = sts.metadata;
    let (labels, annotations) = extract_labels_and_annotations(&meta);

    let spec = sts.spec;
    let status = sts.status;

    let strategy = spec
        .as_ref()
        .and_then(|s| s.update_strategy.as_ref().and_then(|st| st.type_.clone()));

    let conditions = Vec::new(); // StatefulSet doesn't have conditions in the same way

    let running_images = spec
        .as_ref()
        .and_then(|s| {
            s.template.spec.as_ref().map(|ps| {
                ps.containers
                    .iter()
                    .filter_map(|c| c.image.clone())
                    .collect()
            })
        })
        .unwrap_or_default();

    WorkloadInfo {
        kind: "StatefulSet".to_string(),
        name: meta.name.unwrap_or_default(),
        creation_time: meta.creation_timestamp.as_ref().map(to_chrono),
        desired_replicas: spec.and_then(|s| s.replicas),
        ready_replicas: status.as_ref().and_then(|s| s.ready_replicas),
        available_replicas: status.as_ref().and_then(|s| s.available_replicas),
        updated_replicas: status.as_ref().and_then(|s| s.updated_replicas),
        update_strategy: strategy,
        conditions,
        running_images,
        labels,
        annotations,
    }
}

fn extract_daemonset_info(ds: DaemonSet) -> WorkloadInfo {
    let meta = ds.metadata;
    let (labels, annotations) = extract_labels_and_annotations(&meta);

    let spec = ds.spec;
    let status = ds.status;

    let strategy = spec
        .as_ref()
        .and_then(|s| s.update_strategy.as_ref().and_then(|st| st.type_.clone()));

    let conditions = Vec::new();

    let running_images = spec
        .as_ref()
        .and_then(|s| {
            s.template.spec.as_ref().map(|ps| {
                ps.containers
                    .iter()
                    .filter_map(|c| c.image.clone())
                    .collect()
            })
        })
        .unwrap_or_default();

    WorkloadInfo {
        kind: "DaemonSet".to_string(),
        name: meta.name.unwrap_or_default(),
        creation_time: meta.creation_timestamp.as_ref().map(to_chrono),
        desired_replicas: status.as_ref().map(|s| s.desired_number_scheduled),
        ready_replicas: status.as_ref().map(|s| s.number_ready),
        available_replicas: status.as_ref().and_then(|s| s.number_available),
        updated_replicas: status.as_ref().and_then(|s| s.updated_number_scheduled),
        update_strategy: strategy,
        conditions,
        running_images,
        labels,
        annotations,
    }
}

fn extract_job_info(job: Job) -> WorkloadInfo {
    let meta = job.metadata;
    let (labels, annotations) = extract_labels_and_annotations(&meta);

    let spec = job.spec;
    let status = job.status;

    let conditions = status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| {
            conds
                .iter()
                .map(|c| WorkloadCondition {
                    condition_type: c.type_.clone(),
                    status: c.status.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let running_images = spec
        .as_ref()
        .and_then(|s| {
            s.template.spec.as_ref().map(|ps| {
                ps.containers
                    .iter()
                    .filter_map(|c| c.image.clone())
                    .collect()
            })
        })
        .unwrap_or_default();

    WorkloadInfo {
        kind: "Job".to_string(),
        name: meta.name.unwrap_or_default(),
        creation_time: meta.creation_timestamp.as_ref().map(to_chrono),
        desired_replicas: None,
        ready_replicas: status.as_ref().and_then(|s| s.ready),
        available_replicas: None,
        updated_replicas: None,
        update_strategy: None,
        conditions,
        running_images,
        labels,
        annotations,
    }
}

fn extract_cronjob_info(cj: CronJob) -> WorkloadInfo {
    let meta = cj.metadata;
    let (labels, annotations) = extract_labels_and_annotations(&meta);

    let spec = cj.spec;

    let running_images = spec
        .as_ref()
        .and_then(|s| {
            s.job_template.spec.as_ref().and_then(|js| {
                js.template.spec.as_ref().map(|ps| {
                    ps.containers
                        .iter()
                        .filter_map(|c| c.image.clone())
                        .collect()
                })
            })
        })
        .unwrap_or_default();

    let schedule = spec.as_ref().map(|s| format!("Schedule: {}", s.schedule));

    WorkloadInfo {
        kind: "CronJob".to_string(),
        name: meta.name.unwrap_or_default(),
        creation_time: meta.creation_timestamp.as_ref().map(to_chrono),
        desired_replicas: None,
        ready_replicas: None,
        available_replicas: None,
        updated_replicas: None,
        update_strategy: schedule,
        conditions: Vec::new(),
        running_images,
        labels,
        annotations,
    }
}

async fn fetch_pod_details(
    client: &Client,
    namespace: &str,
    service: &ServiceSource,
) -> Result<Vec<PodDetail>> {
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = service
        .selector
        .clone()
        .unwrap_or_else(|| format!("app={}", service.name));
    let lp = ListParams::default().labels(&selector);

    let pod_list = pods_api.list(&lp).await.context("Failed to list pods")?;

    let details: Vec<PodDetail> = pod_list
        .items
        .into_iter()
        .map(|pod| {
            let meta = pod.metadata;
            let spec = pod.spec;
            let status = pod.status;

            let phase = status
                .as_ref()
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            let node = spec.as_ref().and_then(|s| s.node_name.clone());

            let start_time = status
                .as_ref()
                .and_then(|s| s.start_time.as_ref())
                .map(to_chrono);

            let (restart_count, ready) = status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|statuses| {
                    let restarts: i32 = statuses.iter().map(|cs| cs.restart_count).sum();
                    let all_ready = statuses.iter().all(|cs| cs.ready);
                    (restarts, all_ready)
                })
                .unwrap_or((0, false));

            let container_image = spec
                .as_ref()
                .and_then(|s| {
                    s.containers
                        .iter()
                        .find(|c| c.name == service.container_name)
                        .or_else(|| s.containers.first())
                })
                .and_then(|c| c.image.clone())
                .unwrap_or_default();

            let (resource_requests, resource_limits) = spec
                .as_ref()
                .and_then(|s| {
                    s.containers
                        .iter()
                        .find(|c| c.name == service.container_name)
                        .or_else(|| s.containers.first())
                })
                .and_then(|c| c.resources.as_ref())
                .map(|r| {
                    let req = r.requests.as_ref().map(|m| ResourceSpec {
                        cpu: m.get("cpu").map(|v| v.0.clone()),
                        memory: m.get("memory").map(|v| v.0.clone()),
                    });
                    let lim = r.limits.as_ref().map(|m| ResourceSpec {
                        cpu: m.get("cpu").map(|v| v.0.clone()),
                        memory: m.get("memory").map(|v| v.0.clone()),
                    });
                    (req, lim)
                })
                .unwrap_or((None, None));

            PodDetail {
                name: meta.name.unwrap_or_default(),
                phase,
                node,
                start_time,
                restart_count,
                ready,
                container_image,
                resource_requests,
                resource_limits,
            }
        })
        .collect();

    Ok(details)
}

async fn fetch_events(client: &Client, namespace: &str, workload_name: &str) -> Vec<EventInfo> {
    let events_api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let lp = ListParams::default().fields(&format!("involvedObject.name={}", workload_name));

    let event_list = match events_api.list(&lp).await {
        Ok(list) => list,
        Err(_) => return Vec::new(),
    };

    let mut events: Vec<EventInfo> = event_list
        .items
        .into_iter()
        .map(|e| EventInfo {
            event_type: e.type_.unwrap_or_else(|| "Normal".to_string()),
            reason: e.reason.unwrap_or_default(),
            message: e.message.unwrap_or_default(),
            count: e.count,
            last_seen: e.last_timestamp.as_ref().map(to_chrono),
        })
        .collect();

    events.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    events.truncate(10);
    events
}

fn fetch_git_info(service: &ServiceSource) -> Option<GitCommitInfo> {
    match Git::last_commit_for_file(&service.source_root, &service.yaml_path) {
        Ok(Some(entry)) => {
            let short_hash = if entry.hash.len() >= 8 {
                entry.hash[..8].to_string()
            } else {
                entry.hash.clone()
            };
            Some(GitCommitInfo {
                hash: short_hash,
                author: entry.author,
                date: entry.date,
                message: entry.message,
            })
        }
        _ => None,
    }
}

fn format_age(since: DateTime<Utc>) -> String {
    let duration = Utc::now().signed_duration_since(since);
    if duration.num_days() > 0 {
        format!("{}d", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{}h", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{}m", duration.num_minutes())
    } else {
        "< 1m".to_string()
    }
}

fn print_info(info: &ServiceInfo, service: &ServiceSource, env: &Environment) {
    println!();

    // Header
    println!(
        "  {}",
        style(format!(
            "{} / {} / {}",
            env.name, info.namespace, info.workload.name
        ))
        .bold()
    );

    let relative_yaml = pathdiff::diff_paths(&service.yaml_path, &service.source_root)
        .unwrap_or_else(|| service.yaml_path.clone());
    println!(
        "  {}",
        style(format!(
            "Kind: {}  |  YAML: [{}]/{}",
            info.workload.kind,
            service.source_name,
            relative_yaml.display()
        ))
        .dim()
    );
    println!();

    // Workload Status
    println!("  {}", style("WORKLOAD STATUS").underlined().bold());

    if let Some(desired) = info.workload.desired_replicas {
        let ready = info.workload.ready_replicas.unwrap_or(0);
        let available = info.workload.available_replicas.unwrap_or(0);
        let updated = info.workload.updated_replicas.unwrap_or(0);

        let ready_style = if ready == desired {
            style(format!("{}/{}", ready, desired)).green()
        } else {
            style(format!("{}/{}", ready, desired)).yellow()
        };

        println!(
            "  Replicas:    {} ready, {} available, {} updated",
            ready_style, available, updated
        );
    }

    if let Some(ref strategy) = info.workload.update_strategy {
        println!("  Strategy:    {}", strategy);
    }

    if let Some(created) = info.workload.creation_time {
        let age = format_age(created);
        println!(
            "  Created:     {} ({})",
            created.format("%Y-%m-%d %H:%M:%S UTC"),
            age
        );
    }

    for cond in &info.workload.conditions {
        let status_styled = if cond.status == "True" {
            style(&cond.status).green()
        } else {
            style(&cond.status).red()
        };
        println!(
            "  {:12} {}",
            format!("{}:", cond.condition_type),
            status_styled
        );
    }
    println!();

    // Current Image
    println!("  {}", style("CURRENT IMAGE").underlined().bold());
    for img in &info.workload.running_images {
        let (base, tag) = img.rsplit_once(':').unwrap_or((img.as_str(), "(no tag)"));
        println!("  {}:{}", base, style(tag).cyan().bold());
    }
    println!();

    // YAML vs Cluster Image Drift
    println!("  {}", style("YAML VS CLUSTER").underlined().bold());
    println!("  YAML:     {}", info.image_comparison.yaml_image);
    if info.image_comparison.cluster_images.is_empty() {
        println!("  Cluster:  {}", style("(no running image found)").yellow());
    } else {
        for (idx, img) in info.image_comparison.cluster_images.iter().enumerate() {
            if idx == 0 {
                println!("  Cluster:  {}", img);
            } else {
                println!("            {}", img);
            }
        }
    }
    if info.image_comparison.drift {
        println!(
            "  Status:   {}",
            style("DIFF: cluster image does not match YAML").yellow().bold()
        );
    } else {
        println!("  Status:   {}", style("OK: cluster and YAML are aligned").green());
    }
    println!();

    // Git / Release Info
    println!("  {}", style("LAST RELEASE COMMIT").underlined().bold());
    if let Some(ref commit) = info.last_commit {
        println!(
            "  Commit:   {} {}",
            style(&commit.hash).yellow(),
            commit.message
        );
        println!("  Author:   {}", commit.author);
        println!("  Date:     {}", commit.date);
    } else {
        println!("  {}", style("(no git history available)").dim());
    }
    println!();

    // Labels & Annotations
    let has_labels = !info.workload.labels.is_empty();
    let has_annotations = !info.workload.annotations.is_empty();
    if has_labels || has_annotations {
        println!("  {}", style("LABELS & ANNOTATIONS").underlined().bold());
        for (k, v) in &info.workload.labels {
            println!("  {}: {}", style(k).dim(), v);
        }
        for (k, v) in &info.workload.annotations {
            let display_val = if v.len() > 80 {
                format!("{}...", &v[..77])
            } else {
                v.clone()
            };
            println!("  {}: {}", style(k).dim(), display_val);
        }
        println!();
    }

    // Pod Details
    println!("  {}", style("PODS").underlined().bold());
    if info.pods.is_empty() {
        println!("  {}", style("No pods found matching selector").yellow());
    } else {
        println!(
            "  {:<45} {:<12} {:<7} {:<10} {:<8} {}",
            style("NAME").dim(),
            style("STATUS").dim(),
            style("READY").dim(),
            style("RESTARTS").dim(),
            style("AGE").dim(),
            style("NODE").dim()
        );

        // Check if pods have different images (rollout in progress)
        let unique_images: std::collections::HashSet<&str> = info
            .pods
            .iter()
            .map(|p| p.container_image.as_str())
            .collect();
        let show_image_per_pod = unique_images.len() > 1;

        for pod in &info.pods {
            let status_styled = match pod.phase.as_str() {
                "Running" => style(&pod.phase).green(),
                "Pending" | "ContainerCreating" => style(&pod.phase).yellow(),
                "Failed" | "Error" | "CrashLoopBackOff" => style(&pod.phase).red(),
                "Succeeded" => style(&pod.phase).cyan(),
                _ => style(&pod.phase).dim(),
            };

            let ready_str = if pod.ready {
                style("Yes".to_string()).green()
            } else {
                style("No".to_string()).red()
            };

            let restart_styled = if pod.restart_count > 0 {
                style(pod.restart_count.to_string()).yellow()
            } else {
                style(pod.restart_count.to_string()).dim()
            };

            let age = pod
                .start_time
                .map(format_age)
                .unwrap_or_else(|| "-".to_string());

            let node_or_image = if show_image_per_pod {
                let tag = pod
                    .container_image
                    .rsplit_once(':')
                    .map(|(_, t)| t)
                    .unwrap_or("?");
                format!(
                    "{} [{}]",
                    pod.node.as_deref().unwrap_or("-"),
                    style(tag).cyan()
                )
            } else {
                pod.node.as_deref().unwrap_or("-").to_string()
            };

            println!(
                "  {:<45} {:<12} {:<7} {:<10} {:<8} {}",
                pod.name, status_styled, ready_str, restart_styled, age, node_or_image
            );
        }

        // Resources from first pod
        if let Some(pod) = info.pods.first()
            && (pod.resource_requests.is_some() || pod.resource_limits.is_some())
        {
            println!();
            println!("  {}", style("RESOURCES").underlined().bold());
            if let Some(ref req) = pod.resource_requests {
                println!(
                    "  Requests:  CPU: {}  Memory: {}",
                    req.cpu.as_deref().unwrap_or("-"),
                    req.memory.as_deref().unwrap_or("-")
                );
            }
            if let Some(ref lim) = pod.resource_limits {
                println!(
                    "  Limits:    CPU: {}  Memory: {}",
                    lim.cpu.as_deref().unwrap_or("-"),
                    lim.memory.as_deref().unwrap_or("-")
                );
            }
        }
    }
    println!();

    // Events
    if !info.events.is_empty() {
        println!("  {}", style("RECENT EVENTS").underlined().bold());
        for event in &info.events {
            let type_styled = if event.event_type == "Warning" {
                style(&event.event_type).yellow()
            } else {
                style(&event.event_type).dim()
            };

            let age = event
                .last_seen
                .map(format_age)
                .unwrap_or_else(|| "-".to_string());

            let count_str = event.count.map(|c| format!("(x{})", c)).unwrap_or_default();

            println!(
                "  {:<8} {:<10} {:<25} {} {}",
                age, type_styled, event.reason, event.message, count_str
            );
        }
        println!();
    }
}
