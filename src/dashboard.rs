use std::{io, time::Duration, collections::HashSet};
use anyhow::{Result, Context};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph, List, ListItem},
    style::{Color, Style, Modifier},
    Frame, Terminal,
};
use kube::{Client, Api, api::{ListParams, LogParams}};
use k8s_openapi::api::core::v1::Pod;
use tokio::sync::mpsc;
use futures::StreamExt;

pub struct Dashboard {
    service: String,
    env_name: String,
    tag: String,
    pods: Vec<PodInfo>,
    old_logs: Vec<String>,
    new_logs: Vec<String>,
    tailed_pods: HashSet<String>,
    log_rx: mpsc::UnboundedReceiver<LogLine>,
    log_tx: mpsc::UnboundedSender<LogLine>,
}

struct LogLine {
    pod_name: String,
    content: String,
    is_new: bool,
}

struct PodInfo {
    name: String,
    status: String,
    is_new: bool,
}

impl Dashboard {
    pub fn new(service: String, env_name: String, tag: String) -> Self {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        Self {
            service,
            env_name,
            tag,
            pods: Vec::new(),
            old_logs: Vec::new(),
            new_logs: Vec::new(),
            tailed_pods: HashSet::new(),
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

        let client = Client::try_default().await.context("Failed to create K8s client")?;
        
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

    async fn run_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>, client: Client) -> Result<()> 
    where B::Error: std::fmt::Display
    {
        let pods_api: Api<Pod> = Api::default_namespaced(client.clone());
        let lp = ListParams::default().labels(&format!("app={}", self.service));

        loop {
            // 1. Update pod list
            if let Ok(pod_list) = pods_api.list(&lp).await {
                let mut current_pods = Vec::new();
                for p in pod_list.items {
                    let name = p.metadata.name.clone().unwrap_or_default();
                    let status = p.status.as_ref()
                        .and_then(|s| s.phase.clone())
                        .unwrap_or_else(|| "Unknown".to_string());
                    
                    let is_new = p.spec.as_ref()
                        .and_then(|s| s.containers.first())
                        .map(|c| c.image.as_ref().map(|i| i.contains(&self.tag)).unwrap_or(false))
                        .unwrap_or(false);

                    if !self.tailed_pods.contains(&name) && status == "Running" {
                        self.tailed_pods.insert(name.clone());
                        let tx = self.log_tx.clone();
                        let api = pods_api.clone();
                        let p_name = name.clone();
                        tokio::spawn(async move {
                            let mut lp = LogParams::default();
                            lp.follow = true;
                            lp.tail_lines = Some(10);

                            if let Ok(stream) = api.log_stream(&p_name, &lp).await {
                                use futures::io::AsyncBufReadExt;
                                let mut lines = stream.lines();
                                while let Some(res) = lines.next().await {
                                    if let Ok(line) = res {
                                        let line: String = line;
                                        let _ = tx.send(LogLine { 
                                            pod_name: p_name.clone(), 
                                            content: line.trim().to_string(), 
                                            is_new 
                                        });
                                    }
                                }
                            }
                        });
                    }

                    current_pods.push(PodInfo { name, status, is_new });
                }
                self.pods = current_pods;
            }

            // 2. Consume logs
            while let Ok(log) = self.log_rx.try_recv() {
                if log.is_new {
                    self.new_logs.push(format!("[{}] {}", log.pod_name.split('-').last().unwrap_or(""), log.content));
                    if self.new_logs.len() > 100 { self.new_logs.remove(0); }
                } else {
                    self.old_logs.push(format!("[{}] {}", log.pod_name.split('-').last().unwrap_or(""), log.content));
                    if self.old_logs.len() > 100 { self.old_logs.remove(0); }
                }
            }

            // 3. Render
            terminal.draw(|f| self.ui(f)).map_err(|e| anyhow::anyhow!("Draw error: {}", e))?;

            // 4. Handle input
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if let KeyCode::Char('q') = key.code {
                        return Ok(());
                    }
                }
            }
        }
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

        let pods: Vec<ListItem> = self.pods.iter().map(|p| {
            let style = if p.is_new {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let prefix = if p.is_new { " [NEW] " } else { " [OLD] " };
            ListItem::new(format!("{}{} -> {}", prefix, p.name, p.status)).style(style)
        }).collect();

        let pods_list = List::new(pods)
            .block(Block::default().title(" Pod Status ").borders(Borders::ALL));
        f.render_widget(pods_list, chunks[1]);

        let log_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);

        let old_logs: Vec<ListItem> = self.old_logs.iter().rev().take(50).map(|l| ListItem::new(l.as_str()).style(Style::default().fg(Color::DarkGray))).collect();
        let old_list = List::new(old_logs).block(Block::default().title(" Old Pod Logs ").borders(Borders::ALL));
        f.render_widget(old_list, log_chunks[0]);

        let new_logs: Vec<ListItem> = self.new_logs.iter().rev().take(50).map(|l| ListItem::new(l.as_str()).style(Style::default().fg(Color::Green))).collect();
        let new_list = List::new(new_logs).block(Block::default().title(" New Pod Logs ").borders(Borders::ALL));
        f.render_widget(new_list, log_chunks[1]);
    }
}
