use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    api::{ListParams, LogParams},
    config::KubeConfigOptions,
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListDirection, ListItem, Paragraph},
};
use std::{collections::HashSet, io, time::Duration};
use tokio::sync::mpsc;

pub struct Dashboard {
    service: String,
    env_name: String,
    tag: String,
    kubectl_context: String,
    namespace: Option<String>,
    selector: Option<String>,
    container_name: String,
    pods: Vec<PodInfo>,
    old_logs: Vec<String>,
    new_logs: Vec<String>,
    tailed_pods: HashSet<String>,
    pod_rx: mpsc::UnboundedReceiver<Vec<Pod>>,
    pod_tx: mpsc::UnboundedSender<Vec<Pod>>,
    log_rx: mpsc::UnboundedReceiver<LogLine>,
    log_tx: mpsc::UnboundedSender<LogLine>,
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
    restarts: i32,
    age: String,
    is_new: bool,
}

impl Dashboard {
    pub fn new(
        service: String,
        env_name: String,
        tag: String,
        kubectl_context: String,
        namespace: Option<String>,
        selector: Option<String>,
        container_name: String,
    ) -> Self {
        let (pod_tx, pod_rx) = mpsc::unbounded_channel();
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        Self {
            service,
            env_name,
            tag,
            kubectl_context,
            namespace,
            selector,
            container_name,
            pods: Vec::new(),
            old_logs: Vec::new(),
            new_logs: Vec::new(),
            tailed_pods: HashSet::new(),
            pod_rx,
            pod_tx,
            log_rx,
            log_tx,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
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
    ) -> Result<()>
    where
        B::Error: std::fmt::Display,
    {
        let ns = self.namespace.as_deref().unwrap_or("default");
        let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);

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
                        .and_then(|s| s.containers.first())
                        .map(|c| {
                            c.image
                                .as_ref()
                                .map(|i| i.contains(&self.tag))
                                .unwrap_or(false)
                        })
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
                                            let raw_content = line.trim().to_string();
                                            let mut log_line = LogLine {
                                                pod_name: p_name.clone(),
                                                content: raw_content.clone(),
                                                level: None,
                                                timestamp: None,
                                                is_new,
                                            };

                                            // Attempt JSON parsing
                                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(
                                                &raw_content,
                                            ) {
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
                        restarts,
                        age,
                        is_new,
                    });
                }
                self.pods = current_pods;
            }

            // 2. Consume logs
            for _ in 0..200 {
                let Ok(log) = self.log_rx.try_recv() else {
                    break;
                };
                let display_line = self.format_log_line(&log);
                if log.is_new {
                    self.new_logs.push(display_line);
                    if self.new_logs.len() > 100 {
                        self.new_logs.remove(0);
                    }
                } else {
                    self.old_logs.push(display_line);
                    if self.old_logs.len() > 100 {
                        self.old_logs.remove(0);
                    }
                }
            }

            // 3. Render
            terminal
                .draw(|f| self.ui(f))
                .map_err(|e| anyhow::anyhow!("Draw error: {}", e))?;

            // 4. Handle input
            if event::poll(Duration::from_millis(50))? {
                loop {
                    if let Event::Key(key) = event::read()?
                        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && let KeyCode::Char('q') = key.code
                    {
                        return Ok(());
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Percentage(60),
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
            .take(50)
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
            .take(50)
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
