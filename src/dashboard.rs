use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    api::{ListParams, LogParams},
    config::KubeConfigOptions,
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListDirection, ListItem, Paragraph, Wrap},
};
use std::{
    collections::{HashSet, VecDeque},
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_LOG_LINES: usize = 100;
const VISIBLE_LOG_LINES: usize = 50;
const LOG_BATCH_SIZE: usize = 400;
const UI_POLL_INTERVAL: Duration = Duration::from_millis(16);
const HEADER_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const HEADER_HEIGHT: u16 = 3;
const POD_PANEL_MIN_HEIGHT: u16 = 4;
const POD_PANEL_MAX_HEIGHT: u16 = 10;
const LOG_PANEL_MIN_HEIGHT: u16 = 6;

pub enum DashboardExit {
    UserQuit,
    RolloutCompleted,
}

pub struct Dashboard {
    service: String,
    workload_kind: String,
    env_name: String,
    tag: String,
    kubectl_context: String,
    namespace: Option<String>,
    selector: Option<String>,
    container_name: String,
    pods: Vec<PodInfo>,
    old_logs: VecDeque<String>,
    new_logs: VecDeque<String>,
    tailed_pods: HashSet<String>,
    pod_rx: mpsc::UnboundedReceiver<Vec<Pod>>,
    pod_tx: mpsc::UnboundedSender<Vec<Pod>>,
    rollout_status: RolloutStatus,
    rollout_rx: mpsc::UnboundedReceiver<RolloutStatus>,
    rollout_tx: mpsc::UnboundedSender<RolloutStatus>,
    log_rx: mpsc::UnboundedReceiver<LogLine>,
    log_tx: mpsc::UnboundedSender<LogLine>,
    completion_modal_visible: bool,
    completion_acknowledged: bool,
    auto_close_on_rollout_complete: bool,
}

struct LogLine {
    pod_name: String,
    content: String,
    level: Option<String>,
    timestamp: Option<String>,
    is_new: bool,
}

struct PodInfo {
    name: String,
    status: String,
    ready: String,
    ready_count: usize,
    total_containers: usize,
    restarts: i32,
    age: String,
    is_new: bool,
}

#[derive(Clone, Default)]
struct RolloutStatus {
    template_matches_tag: bool,
    workload_complete: bool,
}

impl Dashboard {
    pub fn new(
        service: String,
        workload_kind: String,
        env_name: String,
        tag: String,
        kubectl_context: String,
        namespace: Option<String>,
        selector: Option<String>,
        container_name: String,
        auto_close_on_rollout_complete: bool,
    ) -> Self {
        let (pod_tx, pod_rx) = mpsc::unbounded_channel();
        let (rollout_tx, rollout_rx) = mpsc::unbounded_channel();
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        Self {
            service,
            workload_kind,
            env_name,
            tag,
            kubectl_context,
            namespace,
            selector,
            container_name,
            pods: Vec::new(),
            old_logs: VecDeque::with_capacity(MAX_LOG_LINES),
            new_logs: VecDeque::with_capacity(MAX_LOG_LINES),
            tailed_pods: HashSet::new(),
            pod_rx,
            pod_tx,
            rollout_status: RolloutStatus::default(),
            rollout_rx,
            rollout_tx,
            log_rx,
            log_tx,
            completion_modal_visible: false,
            completion_acknowledged: false,
            auto_close_on_rollout_complete,
        }
    }

    pub async fn run(&mut self) -> Result<DashboardExit> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let options = KubeConfigOptions {
            context: Some(self.kubectl_context.clone()),
            ..Default::default()
        };
        let config = kube::Config::from_kubeconfig(&options)
            .await
            .context("Failed to load kubeconfig")?;
        let client = Client::try_from(config).context("Failed to create K8s client")?;

        let res = self.run_loop(&mut terminal, client).await;

        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        res
    }

    async fn run_loop<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        client: Client,
    ) -> Result<DashboardExit>
    where
        B::Error: std::fmt::Display,
    {
        let namespace = self
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let pods_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);

        let selector = self
            .selector
            .clone()
            .unwrap_or_else(|| format!("app={}", self.service));
        let lp = ListParams::default().labels(&selector);

        let pod_tx = self.pod_tx.clone();
        let pods_api_refresh = pods_api.clone();
        let lp_refresh = lp.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(pod_list) = pods_api_refresh.list(&lp_refresh).await {
                    let _ = pod_tx.send(pod_list.items);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        let rollout_tx = self.rollout_tx.clone();
        let rollout_kind = self.workload_kind.clone();
        let rollout_name = self.service.clone();
        let rollout_tag = self.tag.clone();
        let rollout_container_name = self.container_name.clone();
        let rollout_client = client.clone();
        let rollout_namespace = namespace.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(status) = fetch_rollout_status(
                    rollout_client.clone(),
                    &rollout_namespace,
                    &rollout_kind,
                    &rollout_name,
                    &rollout_tag,
                    &rollout_container_name,
                )
                .await
                {
                    let _ = rollout_tx.send(status);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        let mut last_header_refresh = Instant::now();
        let mut needs_redraw = true;

        loop {
            // 1. Update pod list
            while let Ok(pod_list) = self.pod_rx.try_recv() {
                let mut current_pods = Vec::new();
                for p in pod_list {
                    let name = p.metadata.name.clone().unwrap_or_default();
                    let status = p
                        .status
                        .as_ref()
                        .and_then(|s| s.phase.clone())
                        .unwrap_or_else(|| "Unknown".to_string());

                    let is_new = p
                        .spec
                        .as_ref()
                        .and_then(|s| {
                            s.containers
                                .iter()
                                .find(|c| c.name == self.container_name)
                                .or_else(|| s.containers.first())
                        })
                        .and_then(|c| c.image.as_ref())
                        .map(|image| image.contains(&self.tag))
                        .unwrap_or(false);

                    if !self.tailed_pods.contains(&name) && status == "Running" {
                        self.tailed_pods.insert(name.clone());
                        let tx = self.log_tx.clone();
                        let api = pods_api.clone();
                        let p_name = name.clone();
                        let container = self.container_name.clone();
                        tokio::spawn(async move {
                            let lp = LogParams {
                                follow: true,
                                tail_lines: Some(10),
                                container: Some(container),
                                ..Default::default()
                            };

                            match api.log_stream(&p_name, &lp).await {
                                Ok(stream) => {
                                    use futures::io::AsyncBufReadExt;
                                    let mut lines = stream.lines();
                                    while let Some(res) = lines.next().await {
                                        if let Ok(line) = res {
                                            let raw_content = line.trim();
                                            let mut log_line = LogLine {
                                                pod_name: p_name.clone(),
                                                content: raw_content.to_string(),
                                                level: None,
                                                timestamp: None,
                                                is_new,
                                            };

                                            // Attempt JSON parsing only when the line looks like JSON.
                                            if raw_content.starts_with('{')
                                                && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw_content)
                                            {
                                                // Extract level - GKE uses 'severity', others 'level'
                                                log_line.level = v
                                                    .get("severity")
                                                    .or_else(|| v.get("level"))
                                                    .and_then(|l| l.as_str())
                                                    .map(|s| s.to_uppercase());

                                                // Extract timestamp - GKE 'timestamp', others 'time' or 'timestamp'
                                                log_line.timestamp = v
                                                    .get("timestamp")
                                                    .or_else(|| v.get("time"))
                                                    .and_then(|t| t.as_str())
                                                    .map(|s| s.to_string());

                                                // Extract message - GKE 'message', others 'message' or 'msg' or 'fields.message'
                                                let msg = v
                                                    .get("message")
                                                    .or_else(|| v.get("msg"))
                                                    .or_else(|| v.get("textPayload"))
                                                    .or_else(|| {
                                                        v.get("fields")
                                                            .and_then(|f| f.get("message"))
                                                    })
                                                    .and_then(|m| m.as_str());

                                                if let Some(m) = msg {
                                                    log_line.content = m.to_string();
                                                }
                                            }

                                            let _ = tx.send(log_line);
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(LogLine {
                                        pod_name: p_name,
                                        content: format!("Error streaming logs: {}", e),
                                        level: Some("ERROR".to_string()),
                                        timestamp: None,
                                        is_new,
                                    });
                                }
                            }
                        });
                    }

                    let container_statuses = p
                        .status
                        .as_ref()
                        .and_then(|s| s.container_statuses.as_ref());
                    let total_containers = p.spec.as_ref().map(|s| s.containers.len()).unwrap_or(0);
                    let ready_count = container_statuses
                        .map(|cs| cs.iter().filter(|c| c.ready).count())
                        .unwrap_or(0);
                    let restarts = container_statuses
                        .map(|cs| cs.iter().map(|c| c.restart_count).sum())
                        .unwrap_or(0);
                    let age = p
                        .metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|t| format_age(t.0))
                        .unwrap_or_else(|| "-".to_string());

                    current_pods.push(PodInfo {
                        name,
                        status,
                        ready: format!("{}/{}", ready_count, total_containers),
                        ready_count,
                        total_containers,
                        restarts,
                        age,
                        is_new,
                    });
                }
                self.pods = current_pods;
                self.update_rollout_modal_state();
                if self.auto_close_on_rollout_complete && self.is_rollout_complete() {
                    terminal
                        .draw(|f| self.ui(f))
                        .map_err(|e| anyhow::anyhow!("Draw error: {}", e))?;
                    return Ok(DashboardExit::RolloutCompleted);
                }
                needs_redraw = true;
            }

            while let Ok(rollout_status) = self.rollout_rx.try_recv() {
                self.rollout_status = rollout_status;
                self.update_rollout_modal_state();
                if self.auto_close_on_rollout_complete && self.is_rollout_complete() {
                    terminal
                        .draw(|f| self.ui(f))
                        .map_err(|e| anyhow::anyhow!("Draw error: {}", e))?;
                    return Ok(DashboardExit::RolloutCompleted);
                }
                needs_redraw = true;
            }

            // 2. Consume logs
            for _ in 0..LOG_BATCH_SIZE {
                let Ok(log) = self.log_rx.try_recv() else {
                    break;
                };
                let display_line = self.format_log_line(&log);
                if log.is_new {
                    self.new_logs.push_back(display_line);
                    if self.new_logs.len() > MAX_LOG_LINES {
                        self.new_logs.pop_front();
                    }
                } else {
                    self.old_logs.push_back(display_line);
                    if self.old_logs.len() > MAX_LOG_LINES {
                        self.old_logs.pop_front();
                    }
                }
                needs_redraw = true;
            }

            if last_header_refresh.elapsed() >= HEADER_REFRESH_INTERVAL {
                last_header_refresh = Instant::now();
                needs_redraw = true;
            }

            // 3. Render
            if needs_redraw {
                terminal
                    .draw(|f| self.ui(f))
                    .map_err(|e| anyhow::anyhow!("Draw error: {}", e))?;
                needs_redraw = false;
            }

            // 4. Handle input
            if event::poll(UI_POLL_INTERVAL)? {
                loop {
                    if let Event::Key(key) = event::read()?
                        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    {
                        if self.completion_modal_visible {
                            match key.code {
                                KeyCode::Enter | KeyCode::Char('c') => {
                                    return Ok(DashboardExit::RolloutCompleted);
                                }
                                KeyCode::Esc | KeyCode::Char('k') => {
                                    self.completion_modal_visible = false;
                                    self.completion_acknowledged = true;
                                    needs_redraw = true;
                                }
                                _ => {}
                            }
                        } else if let KeyCode::Char('q') = key.code {
                            return Ok(DashboardExit::UserQuit);
                        }
                    }
                    if !event::poll(Duration::from_millis(0))? {
                        break;
                    }
                }
            }
        }
    }

    fn format_log_line(&self, log: &LogLine) -> String {
        let pod_id = log.pod_name.split('-').next_back().unwrap_or("");
        let ts = log
            .timestamp
            .as_deref()
            .and_then(|t| t.split('T').next_back())
            .map(|t| t.split('.').next().unwrap_or(t))
            .map(|t| format!("{} ", t))
            .unwrap_or_default();

        let level = log.level.as_deref().unwrap_or("INFO");
        format!("[{}] {}{} {}", pod_id, ts, level, log.content)
    }

    fn ui(&self, f: &mut Frame) {
        let pod_panel_height = self.pod_panel_height(f.area().height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(HEADER_HEIGHT),
                Constraint::Length(pod_panel_height),
                Constraint::Min(LOG_PANEL_MIN_HEIGHT),
            ])
            .split(f.area());

        let header = Paragraph::new(format!(
            " Davit Rollout: {} | Env: {} | Tag: {} (Press 'q' to exit)",
            self.service, self.env_name, self.tag
        ))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(header, chunks[0]);

        let pods: Vec<ListItem> = self
            .pods
            .iter()
            .map(|p| {
                let style = if p.is_new {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if p.is_new { "NEW" } else { "OLD" };
                let restarts_str = if p.restarts > 0 {
                    format!("{}r", p.restarts)
                } else {
                    "0r".to_string()
                };
                ListItem::new(format!(
                    " [{prefix}] {name:<48} {status:<12} {ready:<6} {restarts:<5} {age}",
                    prefix = prefix,
                    name = p.name,
                    status = p.status,
                    ready = p.ready,
                    restarts = restarts_str,
                    age = p.age,
                ))
                .style(style)
            })
            .collect();

        let pods_list =
            List::new(pods).block(Block::default().title(" Pod Status ").borders(Borders::ALL));
        f.render_widget(pods_list, chunks[1]);

        let log_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);

        let old_logs: Vec<ListItem> = self
            .old_logs
            .iter()
            .rev()
            .take(VISIBLE_LOG_LINES)
            .map(|l| {
                let style = self.get_log_style(l, Color::DarkGray);
                ListItem::new(l.as_str()).style(style)
            })
            .collect();
        let old_list = List::new(old_logs).block(
            Block::default()
                .title(" Old Pod Logs ")
                .borders(Borders::ALL),
        )
        .direction(ListDirection::BottomToTop);
        f.render_widget(old_list, log_chunks[0]);

        let new_logs: Vec<ListItem> = self
            .new_logs
            .iter()
            .rev()
            .take(VISIBLE_LOG_LINES)
            .map(|l| {
                let style = self.get_log_style(l, Color::Green);
                ListItem::new(l.as_str()).style(style)
            })
            .collect();
        let new_list = List::new(new_logs).block(
            Block::default()
                .title(" New Pod Logs ")
                .borders(Borders::ALL),
        )
        .direction(ListDirection::BottomToTop);
        f.render_widget(new_list, log_chunks[1]);

        if self.completion_modal_visible {
            self.render_completion_modal(f);
        }
    }

    fn pod_panel_height(&self, total_height: u16) -> u16 {
        let desired_height = (self.pods.len() as u16).saturating_add(2);
        let clamped_height = desired_height.clamp(POD_PANEL_MIN_HEIGHT, POD_PANEL_MAX_HEIGHT);
        let available_height = total_height.saturating_sub(HEADER_HEIGHT);
        let max_pod_height = available_height.saturating_sub(LOG_PANEL_MIN_HEIGHT);

        clamped_height.clamp(POD_PANEL_MIN_HEIGHT, max_pod_height.max(POD_PANEL_MIN_HEIGHT))
    }

    fn update_rollout_modal_state(&mut self) {
        if self.is_rollout_complete() {
            if !self.completion_acknowledged {
                self.completion_modal_visible = true;
            }
        } else {
            self.completion_modal_visible = false;
            self.completion_acknowledged = false;
        }
    }

    fn is_rollout_complete(&self) -> bool {
        let has_new_pods = self.pods.iter().any(|pod| pod.is_new);
        let old_pods_gone = self.pods.iter().all(|pod| pod.is_new);
        let new_pods_ready = self
            .pods
            .iter()
            .filter(|pod| pod.is_new)
            .all(|pod| pod.status == "Running" && pod.ready_count == pod.total_containers);

        has_new_pods
            && old_pods_gone
            && new_pods_ready
            && self.rollout_status.template_matches_tag
            && self.rollout_status.workload_complete
    }

    fn render_completion_modal(&self, f: &mut Frame) {
        let area = centered_rect(72, 9, f.area());
        let text = "Release rollout completed.\nAll impacted pods are on the requested tag and reported ready.\n\nEnter/c: close dashboard and continue\nEsc/k: keep dashboard open";
        let modal = Paragraph::new(text)
            .block(
                Block::default()
                    .title(" Rollout Completed ")
                    .borders(Borders::ALL),
            )
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });

        f.render_widget(Clear, area);
        f.render_widget(modal, area);
    }

    fn get_log_style(&self, line: &str, default_color: Color) -> Style {
        if line.contains("ERROR") || line.contains("FATAL") {
            Style::default().fg(Color::Red)
        } else if line.contains("WARN") {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(default_color)
        }
    }
}

async fn fetch_rollout_status(
    client: Client,
    namespace: &str,
    workload_kind: &str,
    workload_name: &str,
    tag: &str,
    container_name: &str,
) -> Result<RolloutStatus> {
    match workload_kind {
        "Deployment" => {
            let api: Api<Deployment> = Api::namespaced(client, namespace);
            let deployment = api.get(workload_name).await?;
            Ok(RolloutStatus::from_deployment(
                &deployment,
                tag,
                container_name,
            ))
        }
        "StatefulSet" => {
            let api: Api<StatefulSet> = Api::namespaced(client, namespace);
            let statefulset = api.get(workload_name).await?;
            Ok(RolloutStatus::from_statefulset(
                &statefulset,
                tag,
                container_name,
            ))
        }
        "DaemonSet" => {
            let api: Api<DaemonSet> = Api::namespaced(client, namespace);
            let daemonset = api.get(workload_name).await?;
            Ok(RolloutStatus::from_daemonset(
                &daemonset,
                tag,
                container_name,
            ))
        }
        _ => Ok(RolloutStatus::default()),
    }
}

impl RolloutStatus {
    fn from_deployment(deployment: &Deployment, tag: &str, container_name: &str) -> Self {
        let spec = deployment.spec.as_ref();
        let status = deployment.status.as_ref();
        let desired_replicas = spec.and_then(|s| s.replicas);
        let ready_replicas = status.and_then(|s| s.ready_replicas);
        let available_replicas = status.and_then(|s| s.available_replicas);
        let updated_replicas = status.and_then(|s| s.updated_replicas);
        let desired = desired_replicas.unwrap_or(1);

        Self {
            template_matches_tag: workload_template_matches_tag(spec, container_name, tag),
            workload_complete: desired > 0
                && ready_replicas.unwrap_or(0) >= desired
                && available_replicas.unwrap_or(0) >= desired
                && updated_replicas.unwrap_or(0) >= desired,
        }
    }

    fn from_statefulset(statefulset: &StatefulSet, tag: &str, container_name: &str) -> Self {
        let spec = statefulset.spec.as_ref();
        let status = statefulset.status.as_ref();
        let desired_replicas = spec.and_then(|s| s.replicas);
        let ready_replicas = status.and_then(|s| s.ready_replicas);
        let updated_replicas = status.and_then(|s| s.updated_replicas);
        let desired = desired_replicas.unwrap_or(1);

        Self {
            template_matches_tag: workload_template_matches_tag(spec, container_name, tag),
            workload_complete: desired > 0
                && ready_replicas.unwrap_or(0) >= desired
                && updated_replicas.unwrap_or(0) >= desired,
        }
    }

    fn from_daemonset(daemonset: &DaemonSet, tag: &str, container_name: &str) -> Self {
        let spec = daemonset.spec.as_ref();
        let status = daemonset.status.as_ref();
        let desired_replicas = status.map(|s| s.desired_number_scheduled);
        let ready_replicas = status.map(|s| s.number_ready);
        let available_replicas = status.and_then(|s| s.number_available);
        let updated_replicas = status.and_then(|s| s.updated_number_scheduled);
        let desired = desired_replicas.unwrap_or(1);

        Self {
            template_matches_tag: workload_template_matches_tag(spec, container_name, tag),
            workload_complete: desired > 0
                && ready_replicas.unwrap_or(0) >= desired
                && available_replicas.unwrap_or(0) >= desired
                && updated_replicas.unwrap_or(0) >= desired,
        }
    }
}

fn workload_template_matches_tag<T>(spec: Option<&T>, container_name: &str, tag: &str) -> bool
where
    T: WorkloadTemplateSpec,
{
    spec.and_then(|s| s.container_image(container_name))
        .map(|image| image.contains(tag))
        .unwrap_or(false)
}

trait WorkloadTemplateSpec {
    fn container_image(&self, container_name: &str) -> Option<String>;
}

impl WorkloadTemplateSpec for k8s_openapi::api::apps::v1::DeploymentSpec {
    fn container_image(&self, container_name: &str) -> Option<String> {
        self.template.spec.as_ref().and_then(|pod_spec| {
            pod_spec
                .containers
                .iter()
                .find(|container| container.name == container_name)
                .or_else(|| pod_spec.containers.first())
                .and_then(|container| container.image.clone())
        })
    }
}

impl WorkloadTemplateSpec for k8s_openapi::api::apps::v1::StatefulSetSpec {
    fn container_image(&self, container_name: &str) -> Option<String> {
        self.template.spec.as_ref().and_then(|pod_spec| {
            pod_spec
                .containers
                .iter()
                .find(|container| container.name == container_name)
                .or_else(|| pod_spec.containers.first())
                .and_then(|container| container.image.clone())
        })
    }
}

impl WorkloadTemplateSpec for k8s_openapi::api::apps::v1::DaemonSetSpec {
    fn container_image(&self, container_name: &str) -> Option<String> {
        self.template.spec.as_ref().and_then(|pod_spec| {
            pod_spec
                .containers
                .iter()
                .find(|container| container.name == container_name)
                .or_else(|| pod_spec.containers.first())
                .and_then(|container| container.image.clone())
        })
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}

fn format_age(created: k8s_openapi::jiff::Timestamp) -> String {
    let now = k8s_openapi::jiff::Timestamp::now();
    let secs = (now - created).get_seconds();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        format!("{}d{}h", days, hours)
    }
}
