use crate::download_queue::{DownloadQueue, JobStatus};
use anyhow::{Context, Result};
use comfybox_core::{
    auth,
    catalog::{Artifact, Catalog, Package, WorkflowDefinition},
    comfy::ComfyManager,
    config::AppConfig,
    inventory::{ArtifactStatus, Inventory},
    state::ManagedState,
    storage,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

const ACCENT: Color = Color::Rgb(116, 192, 252);
const GREEN: Color = Color::Rgb(126, 231, 135);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const MUTED: Color = Color::Rgb(127, 132, 156);
const PANEL: Color = Color::Rgb(49, 50, 68);

#[derive(Debug)]
pub enum DashboardAction {
    Quit,
    InstallComfyUi,
    LocateComfyUi,
    InstallPythonDeps,
    InstallSystemDeps,
    ConfigurePypi,
    SetHfToken,
    ToggleServer,
    InstallPackage(InstallSelection),
    InstallArtifact(InstallSelection),
    InstallCustomNode(String),
    InstallWorkflow(InstallSelection),
}

#[derive(Debug)]
pub struct InstallSelection {
    pub id: String,
    pub artifact_sources: Vec<(String, usize)>,
    pub custom_node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PlanTarget {
    Package(String),
    Artifact(String),
    Workflow(String),
}

#[derive(Clone, Debug)]
struct PlanEditor {
    target: PlanTarget,
    cursor: usize,
    deps_expanded: bool,
    expanded_artifacts: HashSet<String>,
    selected_sources: HashMap<String, usize>,
    disabled_dependencies: HashSet<String>,
}

#[derive(Clone, Debug)]
enum PlanRow {
    Model(String),
    ModelSource(String, usize),
    Dependencies,
    DependencyArtifact(String),
    DependencySource(String, usize),
    DependencyNode(String),
    Ok,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Overview,
    Models,
    Loras,
    Vaes,
    TextEncoders,
    Upscalers,
    RuntimeModels,
    CustomNodes,
    Workflows,
    Downloads,
    Logs,
    Settings,
    System,
}

impl Section {
    const ALL: [Self; 13] = [
        Self::Overview,
        Self::Models,
        Self::Loras,
        Self::Vaes,
        Self::TextEncoders,
        Self::Upscalers,
        Self::RuntimeModels,
        Self::CustomNodes,
        Self::Workflows,
        Self::Downloads,
        Self::Logs,
        Self::Settings,
        Self::System,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Models => "Models",
            Self::Loras => "LoRAs",
            Self::Vaes => "VAEs",
            Self::TextEncoders => "Text Encoders",
            Self::Upscalers => "Upscalers",
            Self::RuntimeModels => "Runtime",
            Self::CustomNodes => "Custom Nodes",
            Self::Workflows => "Workflows",
            Self::Downloads => "Downloads",
            Self::Logs => "Logs",
            Self::Settings => "Settings",
            Self::System => "System",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Ready,
    Missing,
    Paused,
    Broken,
    Blocked,
    Discovery,
}

impl Health {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Missing => "MISSING",
            Self::Paused => "PAUSED",
            Self::Broken => "BROKEN",
            Self::Blocked => "TOKEN",
            Self::Discovery => "DISCOVER",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Ready => GREEN,
            Self::Missing => MUTED,
            Self::Paused => YELLOW,
            Self::Broken => RED,
            Self::Blocked => YELLOW,
            Self::Discovery => ACCENT,
        }
    }
}

struct Dashboard<'a> {
    cfg: &'a mut AppConfig,
    catalog: &'a Catalog,
    queue: &'a mut DownloadQueue,
    section: usize,
    model_index: usize,
    artifact_index: usize,
    custom_node_index: usize,
    workflow_index: usize,
    download_index: usize,
    download_detail: bool,
    plan_editor: Option<PlanEditor>,
    comfy_running: bool,
    state: ManagedState,
    last_server_check: Instant,
    artifact_health: ArtifactHealthSnapshot,
    system: SystemSnapshot,
    installed_custom_nodes: HashSet<String>,
    server_status_receiver: mpsc::Receiver<bool>,
    server_status_sender: mpsc::Sender<bool>,
    server_check_pending: bool,
    clear_before_draw: bool,
}

#[derive(Default)]
struct ArtifactHealthSnapshot {
    ready: usize,
    missing: usize,
    paused: usize,
    broken: usize,
    by_id: HashMap<String, Health>,
}

struct SystemSnapshot {
    git: bool,
    python: Option<PathBuf>,
    disk: Option<u64>,
    token_status: String,
    has_token: bool,
    dependency_status: Vec<(String, bool)>,
    python_dependencies_ready: bool,
}

pub fn run(
    cfg: &mut AppConfig,
    catalog: &Catalog,
    queue: &mut DownloadQueue,
) -> Result<DashboardAction> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        anyhow::bail!(
            "the dashboard requires an interactive terminal; use a subcommand for non-interactive runs"
        );
    }

    // Collect potentially slow filesystem/process state before clearing the terminal.
    // The previous screen remains visible while these snapshots are refreshed.
    let state = ManagedState::load().unwrap_or_default();
    let comfy_running = detect_comfy_running(cfg, &state);
    let artifact_health = collect_artifact_health(cfg, catalog);
    let system = collect_system_snapshot(cfg, catalog);
    let installed_custom_nodes = collect_installed_custom_nodes(cfg, catalog);
    let (server_status_sender, server_status_receiver) = mpsc::channel();

    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear()?;

    let mut app = Dashboard {
        cfg,
        catalog,
        queue,
        section: 0,
        model_index: 0,
        artifact_index: 0,
        custom_node_index: 0,
        workflow_index: 0,
        download_index: 0,
        download_detail: false,
        plan_editor: None,
        comfy_running,
        state,
        last_server_check: Instant::now(),
        artifact_health,
        system,
        installed_custom_nodes,
        server_status_receiver,
        server_status_sender,
        server_check_pending: false,
        clear_before_draw: false,
    };

    let mut needs_draw = true;
    loop {
        if app.queue.tick(app.cfg.max_concurrent_downloads) {
            needs_draw = true;
        }
        if Section::ALL[app.section] == Section::Logs
            && let Some(path) = app.state.comfy_log.as_deref()
            && app.queue.tail_comfyui_log(Path::new(path))
        {
            needs_draw = true;
        }
        if let Ok(running) = app.server_status_receiver.try_recv() {
            app.comfy_running = running;
            app.server_check_pending = false;
            needs_draw = true;
        }
        if app.last_server_check.elapsed() >= Duration::from_secs(2) && !app.server_check_pending {
            let cfg = app.cfg.clone();
            let sender = app.server_status_sender.clone();
            std::thread::spawn(move || {
                let running = detect_comfy_running(&cfg, &ManagedState::default());
                let _ = sender.send(running);
            });
            app.server_check_pending = true;
            app.last_server_check = Instant::now();
        }
        if needs_draw {
            if app.clear_before_draw {
                terminal.clear()?;
                app.clear_before_draw = false;
            }
            terminal.draw(|frame| app.render(frame))?;
            needs_draw = false;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                app.clear_before_draw = true;
                needs_draw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(action) = app.handle_key(key) {
            return Ok(action);
        }
        needs_draw = true;
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Dashboard<'_> {
    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < 72 || area.height < 20 {
            frame.render_widget(
                Paragraph::new("ComfyBox needs a terminal at least 72 × 20.\nResize the window or press q to quit.")
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL).title(" ComfyBox ")),
                area,
            );
            return;
        }

        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(12),
                Constraint::Length(2),
            ])
            .split(area);
        self.render_header(frame, vertical[0]);
        self.render_tabs(frame, vertical[1]);
        match Section::ALL[self.section] {
            Section::Overview => self.render_overview(frame, vertical[2]),
            Section::Models => self.render_models(frame, vertical[2]),
            Section::Loras
            | Section::Vaes
            | Section::TextEncoders
            | Section::Upscalers
            | Section::RuntimeModels => self.render_artifacts(frame, vertical[2]),
            Section::CustomNodes => self.render_custom_nodes(frame, vertical[2]),
            Section::Workflows => self.render_workflows(frame, vertical[2]),
            Section::Downloads => self.render_downloads(frame, vertical[2]),
            Section::Logs => self.render_logs(frame, vertical[2]),
            Section::Settings => self.render_settings(frame, vertical[2]),
            Section::System => self.render_system(frame, vertical[2]),
        }
        self.render_footer(frame, vertical[3]);
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let configured = self.valid_comfy_root().is_some();
        let server = if self.comfy_running {
            Span::styled(
                " ● RUNNING ",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(" ○ STOPPED ", Style::default().fg(MUTED))
        };
        let config = if configured {
            Span::styled(" COMFYUI READY ", Style::default().fg(GREEN))
        } else {
            Span::styled(
                " SETUP REQUIRED ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )
        };
        let header = Line::from(vec![
            Span::styled(
                " COMFY",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "BOX ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}  ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(MUTED),
            ),
            config,
            Span::raw("  "),
            server,
        ]);
        frame.render_widget(
            Paragraph::new(header).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(PANEL)),
            ),
            area,
        );
    }

    fn render_tabs(&self, frame: &mut Frame<'_>, area: Rect) {
        let titles = Section::ALL
            .iter()
            .map(|section| {
                let title = if area.width < 100 {
                    match section {
                        Section::Overview => "Home",
                        Section::Models => "Models",
                        Section::Loras => "LoRA",
                        Section::Vaes => "VAE",
                        Section::TextEncoders => "Text",
                        Section::Upscalers => "Up",
                        Section::RuntimeModels => "Run",
                        Section::CustomNodes => "Nodes",
                        Section::Workflows => "Flows",
                        Section::Downloads => "DL",
                        Section::Logs => "Logs",
                        Section::Settings => "Set",
                        Section::System => "Sys",
                    }
                } else {
                    section.title()
                };
                Line::from(format!(" {title} "))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Tabs::new(titles)
                .select(self.section)
                .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                .divider(Span::styled("│", Style::default().fg(PANEL)))
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(Style::default().fg(PANEL)),
                ),
            area,
        );
    }

    fn render_overview(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(area);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        self.render_environment_card(frame, top[0]);
        self.render_library_card(frame, top[1]);
        self.render_attention_card(frame, rows[1]);
    }

    fn render_environment_card(&self, frame: &mut Frame<'_>, area: Rect) {
        let path = self
            .cfg
            .comfy_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not configured".into());
        let python = self
            .valid_comfy_root()
            .and_then(find_python)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not detected".into());
        let dependencies = self.system.python_dependencies_ready;
        let token = if self.system.has_token {
            Span::styled(self.system.token_status.clone(), Style::default().fg(GREEN))
        } else {
            Span::styled("missing", Style::default().fg(YELLOW))
        };
        let endpoint = self
            .cfg
            .hf_endpoint
            .clone()
            .or_else(|| env::var("HF_ENDPOINT").ok())
            .unwrap_or_else(|| "https://huggingface.co".into());
        let lines = vec![
            Line::from(vec![label("Path"), Span::raw(path)]),
            Line::from(vec![label("Python"), Span::raw(python)]),
            Line::from(vec![
                label("Dependencies"),
                state_span(
                    if dependencies {
                        "ready"
                    } else {
                        "not installed"
                    },
                    dependencies,
                ),
            ]),
            Line::from(vec![label("HF token"), token]),
            Line::from(vec![label("Endpoint"), Span::raw(endpoint)]),
        ];
        frame.render_widget(card(" Environment ", Text::from(lines)), area);
    }

    fn render_library_card(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(_root) = self.valid_comfy_root() else {
            frame.render_widget(
                card(
                    " Library ",
                    Text::from("Configure a valid ComfyUI installation to scan models."),
                ),
                area,
            );
            return;
        };
        let health = &self.artifact_health;
        let lines = vec![
            metric_line("Ready", health.ready, GREEN),
            metric_line("Missing", health.missing, MUTED),
            metric_line("Paused", health.paused, YELLOW),
            metric_line("Broken", health.broken, RED),
        ];
        frame.render_widget(card(" Artifact health ", Text::from(lines)), area);
    }

    fn render_attention_card(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        if self.valid_comfy_root().is_none() {
            lines.push(warning_line(
                "No valid ComfyUI installation is configured.",
                RED,
            ));
            lines.push(Line::from(
                "  Press l to locate one or i to install ComfyUI.",
            ));
        }
        if !self.system.has_token {
            lines.push(warning_line("HF_TOKEN is not set.", YELLOW));
            lines.push(Line::from(
                "  Public downloads work; press t to save a token for gated repositories.",
            ));
        }
        if self.valid_comfy_root().is_some() && !self.system.python_dependencies_ready {
            lines.push(warning_line(
                "ComfyUI Python dependencies are not ready.",
                YELLOW,
            ));
            lines.push(Line::from("  Press p to install them in the background; server start/stop is locked until complete."));
        }
        if self.valid_comfy_root().is_some() {
            let paused = self.artifact_health.paused;
            let broken = self.artifact_health.broken;
            if paused > 0 {
                lines.push(warning_line(
                    &format!("{paused} interrupted download(s) can be resumed from Downloads."),
                    YELLOW,
                ));
            }
            if broken > 0 {
                lines.push(warning_line(
                    &format!("{broken} mismatched artifact(s) need force repair."),
                    RED,
                ));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    "✓ ",
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ),
                Span::raw("No action needed. ComfyBox is ready."),
            ]));
        }
        frame.render_widget(card(" Needs attention ", Text::from(lines)), area);
    }

    fn render_models(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = plan_columns(area);
        let items = self
            .catalog
            .packages
            .iter()
            .map(|package| {
                let health = self.package_health(package);
                ListItem::new(Line::from(vec![
                    status_badge(health),
                    Span::raw(" "),
                    Span::styled(package.name.clone(), Style::default().fg(Color::White)),
                    Span::styled(format!("  {}", package.id), Style::default().fg(MUTED)),
                    Span::styled(
                        format!(
                            "  {}",
                            self.artifact_ids_size(
                                package
                                    .primary_artifact_ids
                                    .iter()
                                    .chain(&package.dependency_artifact_ids)
                            )
                        ),
                        Style::default().fg(MUTED),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        let list_focused = self.plan_editor.is_none();
        let mut state = ListState::default()
            .with_selected(Some(self.model_index.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(if list_focused { "› " } else { "  " })
                .highlight_style(if list_focused {
                    Style::default().bg(PANEL).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                })
                .block(
                    Block::default()
                        .title(" Model packages ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = self
                .catalog
                .packages
                .get(self.model_index)
                .map(|package| {
                    let required =
                        package.primary_artifact_ids.len() + package.dependency_artifact_ids.len();
                    let mut lines = vec![
                        Line::from(Span::styled(
                            &package.name,
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(format!("Family: {}", package.family)),
                        Line::from(format!("Required artifacts: {required}")),
                        Line::from(format!(
                            "Download size: {}",
                            self.artifact_ids_size(
                                package
                                    .primary_artifact_ids
                                    .iter()
                                    .chain(&package.dependency_artifact_ids)
                            )
                        )),
                        Line::from(format!(
                            "Optional groups: {}",
                            package.optional_groups.len()
                        )),
                        Line::from(""),
                        Line::from(package.description.as_deref().unwrap_or(
                            "Enter installs the package and its required dependencies.",
                        )),
                    ];
                    lines.push(Line::from(""));
                    if self.plan_editor.as_ref().is_some_and(|editor| {
                        editor.target == PlanTarget::Package(package.id.clone())
                    }) {
                        lines.extend(self.render_plan_lines());
                    } else {
                        lines.push(Line::from(
                            "[ Enter ] Configure install    [ r ] Refresh checks",
                        ));
                    }
                    Text::from(lines)
                })
                .unwrap_or_default();
            frame.render_widget(
                plan_card(" Details ", details, self.plan_editor.as_ref(), columns[1]),
                columns[1],
            );
        }
    }

    fn artifacts_for_section(&self) -> Vec<&Artifact> {
        let kind = match Section::ALL[self.section] {
            Section::Loras => "models/loras/",
            Section::Vaes => "models/vae/",
            Section::TextEncoders => "models/text_encoders/",
            Section::Upscalers => "models/upscale_models/",
            Section::RuntimeModels => "runtime_models/",
            _ => return Vec::new(),
        };
        self.catalog
            .artifacts
            .iter()
            .filter(|artifact| artifact.relative_path.starts_with(kind))
            .collect()
    }

    fn render_artifacts(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = plan_columns(area);
        let artifacts = self.artifacts_for_section();
        let items = artifacts
            .iter()
            .map(|artifact| {
                let health = self
                    .artifact_health
                    .by_id
                    .get(&artifact.id)
                    .copied()
                    .unwrap_or(Health::Missing);
                ListItem::new(Line::from(vec![
                    status_badge(health),
                    Span::raw(" "),
                    Span::styled(artifact.name.clone(), Style::default().fg(Color::White)),
                ]))
            })
            .collect::<Vec<_>>();
        let selected = self.artifact_index.min(items.len().saturating_sub(1));
        let list_focused = self.plan_editor.is_none();
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(if list_focused { "› " } else { "  " })
                .highlight_style(if list_focused {
                    Style::default().bg(PANEL).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                })
                .block(
                    Block::default()
                        .title(format!(" {} ", Section::ALL[self.section].title()))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = artifacts.get(selected).map(|artifact| {
                let mut lines = vec![
                    Line::from(Span::styled(
                        &artifact.name,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!("Path: {}", artifact.relative_path)),
                    Line::from(format!("Sources: {}", artifact.sources.len().max(1))),
                    Line::from(""),
                    Line::from(
                        artifact
                            .description
                            .as_deref()
                            .unwrap_or("Enter downloads this artifact."),
                    ),
                ];
                lines.push(Line::from(""));
                if self.plan_editor.as_ref().is_some_and(|editor| {
                    editor.target == PlanTarget::Artifact(artifact.id.clone())
                }) {
                    lines.extend(self.render_plan_lines());
                } else {
                    lines.push(Line::from(
                        "[ Enter ] Configure install    [ r ] Refresh checks",
                    ));
                }
                Text::from(lines)
            });
            frame.render_widget(
                plan_card(
                    " Install plan ",
                    details.unwrap_or_default(),
                    self.plan_editor.as_ref(),
                    columns[1],
                ),
                columns[1],
            );
        }
    }

    fn render_custom_nodes(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = content_columns(area);
        let items = self
            .catalog
            .custom_nodes
            .iter()
            .map(|node| {
                let health = if self.installed_custom_nodes.contains(&node.id) {
                    Health::Ready
                } else {
                    Health::Missing
                };
                ListItem::new(Line::from(vec![
                    status_badge(health),
                    Span::raw(" "),
                    Span::styled(node.name.clone(), Style::default().fg(Color::White)),
                ]))
            })
            .collect::<Vec<_>>();
        let selected = self.custom_node_index.min(items.len().saturating_sub(1));
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(PANEL).add_modifier(Modifier::BOLD))
                .block(
                    Block::default()
                        .title(" Custom Nodes ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = self.catalog.custom_nodes.get(selected).map(|node| {
                Text::from(vec![
                    Line::from(Span::styled(
                        &node.name,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!("Repository: {}", node.git_url)),
                    Line::from(format!("Folder: {}", node.folder_name)),
                    Line::from(format!("Aliases: {}", node.node_types.join(", "))),
                    Line::from(""),
                    Line::from("[ Enter ] Install    [ r ] Refresh checks"),
                ])
            });
            frame.render_widget(card(" Details ", details.unwrap_or_default()), columns[1]);
        }
    }

    fn render_workflows(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = plan_columns(area);
        let items = self
            .catalog
            .workflows
            .iter()
            .map(|workflow| {
                let health = self.workflow_health(workflow);
                ListItem::new(Line::from(vec![
                    status_badge(health),
                    Span::raw(" "),
                    Span::styled(workflow.name.clone(), Style::default().fg(Color::White)),
                    Span::styled(format!("  {}", workflow.id), Style::default().fg(MUTED)),
                ]))
            })
            .collect::<Vec<_>>();
        let list_focused = self.plan_editor.is_none();
        let mut state = ListState::default()
            .with_selected(Some(self.workflow_index.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(if list_focused { "› " } else { "  " })
                .highlight_style(if list_focused {
                    Style::default().bg(PANEL).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                })
                .block(
                    Block::default()
                        .title(" Workflows ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = self.catalog.workflows.get(self.workflow_index).map(|workflow| {
                let mut lines = vec![
                    Line::from(Span::styled(&workflow.name, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
                    Line::from(format!("File: {}", workflow.file)),
                    Line::from(format!("Models: {}", workflow.artifact_ids.len())),
                    Line::from(format!(
                        "Download size: {}",
                        self.artifact_ids_size(workflow.artifact_ids.iter())
                    )),
                    Line::from(format!("Custom nodes: {}", workflow.custom_node_ids.len())),
                    Line::from(""),
                    Line::from("Enter installs the workflow, models, VAEs, text encoders, LoRAs, and custom nodes."),
                    Line::from(""),
                ];
                if self.plan_editor.as_ref().is_some_and(|editor| {
                    editor.target == PlanTarget::Workflow(workflow.id.clone())
                }) {
                    lines.extend(self.render_plan_lines());
                } else {
                    lines.push(Line::from("[ Enter ] Configure install    [ r ] Refresh checks"));
                }
                Text::from(lines)
            }).unwrap_or_default();
            frame.render_widget(
                plan_card(
                    " Install plan ",
                    details,
                    self.plan_editor.as_ref(),
                    columns[1],
                ),
                columns[1],
            );
        }
    }

    fn plan_artifacts(&self, target: &PlanTarget) -> (Vec<String>, Vec<String>, Vec<String>) {
        match target {
            PlanTarget::Package(id) => self
                .catalog
                .package(id)
                .map(|package| {
                    let mut dependencies = package.dependency_artifact_ids.clone();
                    let mut nodes = package.custom_node_ids.clone();
                    for group in &package.optional_groups {
                        dependencies.extend(group.artifact_ids.iter().cloned());
                        nodes.extend(group.custom_node_ids.iter().cloned());
                    }
                    dependencies.sort();
                    dependencies.dedup();
                    nodes.sort();
                    nodes.dedup();
                    (package.primary_artifact_ids.clone(), dependencies, nodes)
                })
                .unwrap_or_default(),
            PlanTarget::Artifact(id) => self
                .catalog
                .artifact(id)
                .map(|_| (vec![id.clone()], Vec::new(), Vec::new()))
                .unwrap_or_default(),
            PlanTarget::Workflow(id) => self
                .catalog
                .workflow(id)
                .map(|workflow| {
                    let mut models = workflow
                        .artifact_ids
                        .iter()
                        .filter(|id| {
                            self.catalog.artifact(id).is_some_and(|artifact| {
                                artifact
                                    .relative_path
                                    .starts_with("models/diffusion_models/")
                                    || artifact.relative_path.starts_with("models/checkpoints/")
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if models.is_empty()
                        && let Some(first) = workflow.artifact_ids.first()
                    {
                        models.push(first.clone());
                    }
                    let dependencies = workflow
                        .artifact_ids
                        .iter()
                        .filter(|id| !models.contains(id))
                        .cloned()
                        .collect();
                    (models, dependencies, workflow.custom_node_ids.clone())
                })
                .unwrap_or_default(),
        }
    }

    fn artifact_ids_size<'b>(&self, ids: impl Iterator<Item = &'b String>) -> String {
        let mut total = 0u64;
        let mut unknown = 0usize;
        for id in ids {
            match self.catalog.artifact(id).and_then(|artifact| {
                artifact
                    .sources
                    .first()
                    .and_then(|source| source.size_bytes)
                    .or(artifact.size_bytes)
            }) {
                Some(size) => total = total.saturating_add(size),
                None => unknown += 1,
            }
        }
        match (total, unknown) {
            (0, 0) => "0 B".into(),
            (0, _) => "size unknown".into(),
            (_, 0) => bytes_label(total),
            (_, count) => format!("{} + {count} unknown", bytes_label(total)),
        }
    }

    fn plan_rows(&self) -> Vec<PlanRow> {
        let Some(editor) = &self.plan_editor else {
            return Vec::new();
        };
        let (models, dependencies, nodes) = self.plan_artifacts(&editor.target);
        let mut rows = Vec::new();
        for id in models {
            rows.push(PlanRow::Model(id.clone()));
            if editor.expanded_artifacts.contains(&id)
                && let Some(artifact) = self.catalog.artifact(&id)
            {
                rows.extend(
                    (0..artifact.sources.len().max(1))
                        .map(|index| PlanRow::ModelSource(id.clone(), index)),
                );
            }
        }
        rows.push(PlanRow::Dependencies);
        if editor.deps_expanded {
            for id in dependencies {
                rows.push(PlanRow::DependencyArtifact(id.clone()));
                if editor.expanded_artifacts.contains(&id)
                    && let Some(artifact) = self.catalog.artifact(&id)
                {
                    rows.extend(
                        (0..artifact.sources.len().max(1))
                            .map(|index| PlanRow::DependencySource(id.clone(), index)),
                    );
                }
            }
            rows.extend(nodes.into_iter().map(PlanRow::DependencyNode));
        }
        rows.push(PlanRow::Ok);
        rows.push(PlanRow::Cancel);
        rows
    }

    fn render_plan_lines(&self) -> Vec<Line<'static>> {
        let Some(editor) = &self.plan_editor else {
            return Vec::new();
        };
        let rows = self.plan_rows();
        rows.iter()
            .enumerate()
            .map(|(row_index, row)| {
                let selected = row_index == editor.cursor.min(rows.len().saturating_sub(1));
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let text = match row {
                    PlanRow::Model(id) => {
                        let artifact = self.catalog.artifact(id);
                        let health = self
                            .artifact_health
                            .by_id
                            .get(id)
                            .copied()
                            .unwrap_or(Health::Missing);
                        let size = artifact
                            .and_then(|item| {
                                let source = editor.selected_sources.get(id).copied().unwrap_or(0);
                                item.sources
                                    .get(source)
                                    .and_then(|source| source.size.clone())
                                    .or_else(|| item.size_bytes.map(bytes_label))
                            })
                            .unwrap_or_else(|| "size unknown".into());
                        format!(
                            "{} Model  {}  [{} · {}]",
                            if editor.expanded_artifacts.contains(id) {
                                "▾"
                            } else {
                                "▸"
                            },
                            artifact.map(|item| item.name.as_str()).unwrap_or(id),
                            health.label(),
                            size
                        )
                    }
                    PlanRow::ModelSource(id, index) | PlanRow::DependencySource(id, index) => {
                        let source = self
                            .catalog
                            .artifact(id)
                            .and_then(|a| a.sources.get(*index));
                        let health = self
                            .artifact_health
                            .by_id
                            .get(id)
                            .copied()
                            .unwrap_or(Health::Missing);
                        let checked =
                            editor.selected_sources.get(id).copied().unwrap_or(0) == *index;
                        format!(
                            "      {} {}  [{} · {}]",
                            if checked { "●" } else { "○" },
                            source
                                .map(|item| item.title.as_str())
                                .or_else(|| {
                                    self.catalog
                                        .artifact(id)
                                        .map(|artifact| artifact.name.as_str())
                                })
                                .unwrap_or("source"),
                            health.label(),
                            source
                                .and_then(|item| item.size.as_deref())
                                .map(str::to_owned)
                                .or_else(|| {
                                    self.catalog
                                        .artifact(id)
                                        .and_then(|artifact| artifact.size_bytes)
                                        .map(bytes_label)
                                })
                                .unwrap_or_else(|| "size unknown".into())
                        )
                    }
                    PlanRow::Dependencies => {
                        let (_, dependencies, nodes) = self.plan_artifacts(&editor.target);
                        let enabled = dependencies
                            .iter()
                            .chain(nodes.iter())
                            .filter(|id| !editor.disabled_dependencies.contains(*id))
                            .count();
                        format!(
                            "{} Dependencies  {enabled}/{} selected",
                            if editor.deps_expanded { "▾" } else { "▸" },
                            dependencies.len() + nodes.len()
                        )
                    }
                    PlanRow::DependencyArtifact(id) => {
                        let artifact = self.catalog.artifact(id);
                        let enabled = !editor.disabled_dependencies.contains(id);
                        let health = self
                            .artifact_health
                            .by_id
                            .get(id)
                            .copied()
                            .unwrap_or(Health::Missing);
                        let source_index = editor.selected_sources.get(id).copied().unwrap_or(0);
                        let size = artifact
                            .and_then(|item| {
                                item.sources
                                    .get(source_index)
                                    .and_then(|source| source.size.clone())
                                    .or_else(|| item.size_bytes.map(bytes_label))
                            })
                            .unwrap_or_else(|| "size unknown".into());
                        format!(
                            "    {} [{}] {}  [{} · {}]",
                            if editor.expanded_artifacts.contains(id) {
                                "▾"
                            } else {
                                "▸"
                            },
                            if enabled { "x" } else { " " },
                            artifact.map(|item| item.name.as_str()).unwrap_or(id),
                            health.label(),
                            size
                        )
                    }
                    PlanRow::DependencyNode(id) => {
                        let enabled = !editor.disabled_dependencies.contains(id);
                        let ready = self.installed_custom_nodes.contains(id);
                        format!(
                            "    [{}] {}  [{}]",
                            if enabled { "x" } else { " " },
                            self.catalog
                                .custom_node(id)
                                .map(|node| node.name.as_str())
                                .unwrap_or(id),
                            if ready { "READY" } else { "MISSING" }
                        )
                    }
                    PlanRow::Ok => "              [ OK — start downloads ]".into(),
                    PlanRow::Cancel => "              [ Cancel ]".into(),
                };
                Line::from(Span::styled(
                    format!("{} {text}", if selected { "›" } else { " " }),
                    style,
                ))
            })
            .collect()
    }

    fn render_downloads(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let jobs = self.queue.snapshots();
        self.download_index = self.download_index.min(jobs.len().saturating_sub(1));
        if self.download_detail {
            let mut text = jobs
                .get(self.download_index)
                .map(job_details)
                .unwrap_or_else(|| Text::from("No download selected."));
            if jobs
                .get(self.download_index)
                .is_some_and(|job| job.artifact_id == "python-dependencies")
            {
                text.lines.push(Line::from(""));
                text.lines.push(Line::from(Span::styled(
                    "Live process output",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )));
                text.lines.extend(
                    self.queue
                        .logs()
                        .rev()
                        .filter(|line| line.contains("[PYTHON]"))
                        .take(8)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|line| Line::from(line.to_owned())),
                );
            }
            frame.render_widget(card(" Download details · b/Esc background ", text), area);
            return;
        }
        let columns = content_columns(area);
        let items = jobs
            .iter()
            .map(|job| {
                let progress = job
                    .total_bytes
                    .map(|total| {
                        format!(
                            "{:5.1}%  {} / {}",
                            job.downloaded_bytes as f64 * 100.0 / total.max(1) as f64,
                            bytes_label(job.downloaded_bytes),
                            bytes_label(total)
                        )
                    })
                    .unwrap_or_else(|| bytes_label(job.downloaded_bytes));
                ListItem::new(Line::from(vec![
                    job_badge(job.status),
                    Span::raw(" "),
                    Span::styled(job.name.clone(), Style::default().fg(Color::White)),
                    Span::styled(format!("  {progress}"), Style::default().fg(MUTED)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.download_index));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(PANEL).add_modifier(Modifier::BOLD))
                .block(
                    Block::default()
                        .title(" Download manager ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = jobs
                .get(self.download_index)
                .map(job_details)
                .unwrap_or_else(|| {
                    Text::from(
                        "No downloads yet.\nInstall a model package or workflow to add jobs.",
                    )
                });
            frame.render_widget(card(" Progress and recovery ", details), columns[1]);
        }
    }

    fn render_logs(&self, frame: &mut Frame<'_>, area: Rect) {
        let visible = area.height.saturating_sub(2) as usize;
        let lines = self
            .queue
            .logs()
            .rev()
            .take(visible)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| Line::from(line.to_owned()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .title(" Application log · progress events filtered ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(PANEL)),
            ),
            area,
        );
    }

    fn render_settings(&self, frame: &mut Frame<'_>, area: Rect) {
        let active = self
            .queue
            .snapshots()
            .iter()
            .filter(|job| job.status == JobStatus::Downloading)
            .count();
        let lines = vec![
            Line::from(vec![
                label("Files"),
                Span::styled(
                    self.cfg.max_concurrent_downloads.to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  −/+ change"),
            ]),
            Line::from(vec![
                label("Chunks/file"),
                Span::styled(
                    self.cfg.download_parallelism.to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  [/ ] change"),
            ]),
            Line::from(vec![
                label("Chunk size"),
                Span::raw(format!(
                    "{:.0} MiB",
                    self.cfg.chunk_size_bytes as f64 / 1_048_576.0
                )),
            ]),
            Line::from(vec![label("Active now"), Span::raw(active.to_string())]),
            Line::from(vec![
                label("Python index"),
                Span::styled(
                    if self.cfg.pypi_index_url.contains("mirrors.aliyun.com") {
                        "China mirror (Alibaba Cloud)"
                    } else if self
                        .cfg
                        .pypi_index_url
                        .contains("pypi.tuna.tsinghua.edu.cn")
                    {
                        "China mirror (Tsinghua)"
                    } else {
                        "Official PyPI"
                    },
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  y change"),
            ]),
            Line::from(""),
            Line::from("Increasing the file limit starts queued downloads immediately."),
            Line::from(
                "Lowering it never kills active transfers; they finish, and new jobs wait until the active count is below the new limit.",
            ),
            Line::from("Settings are saved for future runs."),
        ];
        frame.render_widget(card(" Download settings ", Text::from(lines)), area);
    }

    fn render_system(&self, frame: &mut Frame<'_>, area: Rect) {
        let root = self.valid_comfy_root();
        let git = self.system.git;
        let python = self.system.python.as_ref();
        let disk = self.system.disk;
        let mut lines = vec![
            Line::from(vec![
                label("ComfyUI"),
                state_span(
                    if root.is_some() {
                        "configured"
                    } else {
                        "not configured"
                    },
                    root.is_some(),
                ),
            ]),
            Line::from(vec![
                label("Server"),
                state_span(
                    if self.comfy_running {
                        "running"
                    } else {
                        "stopped"
                    },
                    self.comfy_running,
                ),
            ]),
            Line::from(vec![
                label("Git"),
                state_span(if git { "available" } else { "missing" }, git),
            ]),
            Line::from(vec![
                label("Python"),
                state_span(
                    if python.is_some() {
                        "available"
                    } else {
                        "missing"
                    },
                    python.is_some(),
                ),
            ]),
            Line::from(vec![
                label("Py deps"),
                state_span(
                    if self.system.python_dependencies_ready {
                        "ready"
                    } else {
                        "not installed"
                    },
                    self.system.python_dependencies_ready,
                ),
            ]),
            Line::from(vec![
                label("HF_TOKEN"),
                state_span(&self.system.token_status, self.system.has_token),
            ]),
            Line::from(vec![
                label("Free disk"),
                Span::raw(
                    disk.map(|bytes| format!("{:.1} GiB", gib(bytes)))
                        .unwrap_or_else(|| "unknown".into()),
                ),
            ]),
            Line::from(vec![
                label("Parallelism"),
                Span::raw(self.cfg.download_parallelism.to_string()),
            ]),
            Line::from(vec![
                label("Chunk size"),
                Span::raw(format!(
                    "{:.0} MiB",
                    self.cfg.chunk_size_bytes as f64 / 1_048_576.0
                )),
            ]),
            Line::from(vec![
                label("State records"),
                Span::raw(format!(
                    "{} packages / {} workflows",
                    self.state.packages.len(),
                    self.state.workflows.len()
                )),
            ]),
            Line::from(vec![
                label("Log"),
                Span::raw(self.state.comfy_log.as_deref().unwrap_or("not created")),
            ]),
        ];
        if !self.catalog.system_dependencies.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Manifest system dependencies",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            for (dependency, (_, ready)) in self
                .catalog
                .system_dependencies
                .iter()
                .zip(&self.system.dependency_status)
            {
                let ready = *ready;
                lines.push(Line::from(vec![
                    label(&dependency.package),
                    state_span(if ready { "available" } else { "missing" }, ready),
                    Span::styled(
                        format!("  {}", dependency.required_by.join(", ")),
                        Style::default().fg(MUTED),
                    ),
                ]));
                if !ready {
                    lines.push(Line::from(Span::styled(
                        format!("  install: {}", dependency.install),
                        Style::default().fg(MUTED),
                    )));
                }
            }
        }
        if let Some(policy) = self.catalog.python_dependencies.as_ref() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Manifest Python dependency policy",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            if let Some(environment) = policy.get("environment").and_then(|value| value.as_str()) {
                lines.push(Line::from(vec![
                    label("Environment"),
                    Span::raw(environment),
                ]));
            }
            if let Some(requirements) = policy
                .get("custom_node_requirements")
                .and_then(|value| value.as_str())
            {
                lines.push(Line::from(Span::styled(
                    requirements,
                    Style::default().fg(MUTED),
                )));
            }
        }
        if !self.catalog.system_dependencies.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[ Enter ] Install missing system dependencies",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .title(" Runtime and configuration ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(PANEL)),
            ),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.plan_editor.is_some() {
            frame.render_widget(
                Paragraph::new(
                    " ↑/↓ choose  Enter expand/select source  Space enable/disable dep  Esc back  OK download ",
                )
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
                area,
            );
            return;
        }
        let help = if area.width < 100 {
            match Section::ALL[self.section] {
                Section::Models
                | Section::Loras
                | Section::Vaes
                | Section::TextEncoders
                | Section::Upscalers
                | Section::RuntimeModels
                | Section::CustomNodes
                | Section::Workflows => {
                    " ←/→ tabs  ↑/↓ select  Enter install  s server  t token  r refresh  q quit "
                        .into()
                }
                Section::Downloads => {
                    " ←/→ tabs  ↑/↓ select  Enter watch  x pause  c continue  r retry  q quit "
                        .into()
                }
                Section::Settings => " ←/→ tabs  −/+ files  [/] chunks  y PyPI  q quit ".into(),
                Section::System => " ←/→ tabs  Enter install system deps  q quit ".into(),
                _ => " ←/→ tabs  s server  l locate  i install  t token  r refresh  q quit ".into(),
            }
        } else {
            let section_hint = match Section::ALL[self.section] {
                Section::Models
                | Section::Loras
                | Section::Vaes
                | Section::TextEncoders
                | Section::Upscalers
                | Section::RuntimeModels
                | Section::CustomNodes
                | Section::Workflows => " Enter install  r refresh checks ",
                Section::Downloads => " Enter watch  x pause  c continue  r retry  b background ",
                Section::Settings => " −/+ files  [/] chunks  y PyPI source ",
                Section::System => " Enter install system deps ",
                _ => "",
            };
            format!(
                " ←/→ tabs  ↑/↓ select {section_hint} s start/stop  l locate  i install ComfyUI  p Python deps  t token  r refresh  q quit "
            )
        };
        frame.render_widget(
            Paragraph::new(help)
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.plan_editor.is_some() {
            return self.handle_plan_key(key);
        }
        match key.code {
            KeyCode::Esc if self.download_detail => self.download_detail = false,
            KeyCode::Char('b') if Section::ALL[self.section] == Section::Downloads => {
                self.download_detail = false
            }
            KeyCode::Char('q') | KeyCode::Esc => return Some(DashboardAction::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(DashboardAction::Quit);
            }
            KeyCode::Right | KeyCode::Tab => {
                self.section = (self.section + 1) % Section::ALL.len();
                self.clear_before_draw = true;
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.section = (self.section + Section::ALL.len() - 1) % Section::ALL.len();
                self.clear_before_draw = true;
            }
            KeyCode::Char('1'..='9') => {
                if let KeyCode::Char(value) = key.code {
                    self.section = value.to_digit(10).unwrap_or(1) as usize - 1;
                    self.clear_before_draw = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('s') if self.valid_comfy_root().is_some() => {
                if self
                    .valid_comfy_root()
                    .is_some_and(crate::download_queue::python_dependencies_ready)
                {
                    return Some(DashboardAction::ToggleServer);
                }
                self.queue
                    .record("server action blocked: install Python dependencies first");
            }
            KeyCode::Char('l') => return Some(DashboardAction::LocateComfyUi),
            KeyCode::Char('i') => return Some(DashboardAction::InstallComfyUi),
            KeyCode::Char('t') => return Some(DashboardAction::SetHfToken),
            KeyCode::Char('p') => {
                if self.valid_comfy_root().is_some() {
                    return Some(DashboardAction::InstallPythonDeps);
                }
                self.queue
                    .record("Python dependency install blocked: install or locate ComfyUI first");
            }
            KeyCode::Char('y') if Section::ALL[self.section] == Section::Settings => {
                return Some(DashboardAction::ConfigurePypi);
            }
            KeyCode::Char('x') if Section::ALL[self.section] == Section::Downloads => {
                let _ = self.queue.stop(self.download_index);
            }
            KeyCode::Char('c') if Section::ALL[self.section] == Section::Downloads => {
                let _ = self.queue.resume(self.download_index);
            }
            KeyCode::Char('r') if Section::ALL[self.section] == Section::Downloads => {
                let _ = self.queue.retry(self.download_index);
            }
            KeyCode::Char('+') | KeyCode::Char('=')
                if Section::ALL[self.section] == Section::Settings =>
            {
                self.cfg.max_concurrent_downloads = (self.cfg.max_concurrent_downloads + 1).min(32);
                let _ = self.cfg.save();
            }
            KeyCode::Char('-') if Section::ALL[self.section] == Section::Settings => {
                self.cfg.max_concurrent_downloads =
                    self.cfg.max_concurrent_downloads.saturating_sub(1).max(1);
                let _ = self.cfg.save();
            }
            KeyCode::Char(']') if Section::ALL[self.section] == Section::Settings => {
                self.cfg.download_parallelism = (self.cfg.download_parallelism + 1).min(32);
                self.queue
                    .update_chunk_parallelism(self.cfg.download_parallelism);
                let _ = self.cfg.save();
            }
            KeyCode::Char('[') if Section::ALL[self.section] == Section::Settings => {
                self.cfg.download_parallelism =
                    self.cfg.download_parallelism.saturating_sub(1).max(1);
                self.queue
                    .update_chunk_parallelism(self.cfg.download_parallelism);
                let _ = self.cfg.save();
            }
            KeyCode::Char('r') => {
                self.state = ManagedState::load().unwrap_or_default();
                self.comfy_running = detect_comfy_running(self.cfg, &self.state);
                self.artifact_health = collect_artifact_health(self.cfg, self.catalog);
                self.system = collect_system_snapshot(self.cfg, self.catalog);
                self.installed_custom_nodes =
                    collect_installed_custom_nodes(self.cfg, self.catalog);
                self.queue.record("refreshed and cached dependency health");
            }
            KeyCode::Enter if Section::ALL[self.section] == Section::Downloads => {
                self.download_detail = true
            }
            KeyCode::Enter
                if matches!(
                    Section::ALL[self.section],
                    Section::Models
                        | Section::Loras
                        | Section::Vaes
                        | Section::TextEncoders
                        | Section::Upscalers
                        | Section::RuntimeModels
                        | Section::Workflows
                ) =>
            {
                self.open_plan_editor();
            }
            KeyCode::Enter => return self.selected_action(),
            _ => {}
        }
        None
    }

    fn open_plan_editor(&mut self) {
        if self.valid_comfy_root().is_none() {
            return;
        }
        let target = match Section::ALL[self.section] {
            Section::Models => {
                let Some(package) = self.catalog.packages.get(self.model_index) else {
                    return;
                };
                if package.primary_artifact_ids.is_empty() {
                    return;
                }
                PlanTarget::Package(package.id.clone())
            }
            Section::Workflows => {
                let Some(workflow) = self.catalog.workflows.get(self.workflow_index) else {
                    return;
                };
                PlanTarget::Workflow(workflow.id.clone())
            }
            Section::Loras
            | Section::Vaes
            | Section::TextEncoders
            | Section::Upscalers
            | Section::RuntimeModels => {
                let artifacts = self.artifacts_for_section();
                let Some(artifact) =
                    artifacts.get(self.artifact_index.min(artifacts.len().saturating_sub(1)))
                else {
                    return;
                };
                PlanTarget::Artifact(artifact.id.clone())
            }
            _ => return,
        };
        let mut disabled_dependencies = HashSet::new();
        if let PlanTarget::Package(id) = &target
            && let Some(package) = self.catalog.package(id)
        {
            for group in &package.optional_groups {
                disabled_dependencies.extend(group.artifact_ids.iter().cloned());
                disabled_dependencies.extend(group.custom_node_ids.iter().cloned());
            }
        }
        let (models, dependencies, _) = self.plan_artifacts(&target);
        let expanded_artifacts = models.into_iter().chain(dependencies).collect();
        self.plan_editor = Some(PlanEditor {
            target,
            cursor: 0,
            deps_expanded: true,
            expanded_artifacts,
            selected_sources: HashMap::new(),
            disabled_dependencies,
        });
    }

    fn handle_plan_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if key.code == KeyCode::Esc {
            self.plan_editor = None;
            return None;
        }
        let rows = self.plan_rows();
        let target = self.plan_editor.as_ref()?.target.clone();
        let plan_artifacts = self.plan_artifacts(&target);
        let editor = self.plan_editor.as_mut()?;
        let mut cancel = false;
        editor.cursor = editor.cursor.min(rows.len().saturating_sub(1));
        match key.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                editor.cursor = (editor.cursor + 1) % rows.len().max(1);
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                editor.cursor = (editor.cursor + rows.len().saturating_sub(1)) % rows.len().max(1);
            }
            KeyCode::Left => {
                cancel = true;
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                let row = rows.get(editor.cursor)?.clone();
                let enable_toggle = key.code == KeyCode::Char(' ');
                match row {
                    PlanRow::Model(id) => {
                        if !editor.expanded_artifacts.insert(id.clone()) {
                            editor.expanded_artifacts.remove(&id);
                        }
                    }
                    PlanRow::ModelSource(id, index) | PlanRow::DependencySource(id, index) => {
                        editor.selected_sources.insert(id, index);
                    }
                    PlanRow::Dependencies => editor.deps_expanded = !editor.deps_expanded,
                    PlanRow::DependencyArtifact(id) => {
                        if enable_toggle {
                            if !editor.disabled_dependencies.insert(id.clone()) {
                                editor.disabled_dependencies.remove(&id);
                            }
                        } else if !editor.expanded_artifacts.insert(id.clone()) {
                            editor.expanded_artifacts.remove(&id);
                        }
                    }
                    PlanRow::DependencyNode(id) => {
                        if !editor.disabled_dependencies.insert(id.clone()) {
                            editor.disabled_dependencies.remove(&id);
                        }
                    }
                    PlanRow::Ok => {
                        let (models, dependencies, nodes) = plan_artifacts;
                        let artifact_sources = models
                            .into_iter()
                            .chain(dependencies)
                            .filter(|id| !editor.disabled_dependencies.contains(id))
                            .map(|id| {
                                let source = editor.selected_sources.get(&id).copied().unwrap_or(0);
                                (id, source)
                            })
                            .collect();
                        let custom_node_ids = nodes
                            .into_iter()
                            .filter(|id| !editor.disabled_dependencies.contains(id))
                            .collect();
                        let action = match &editor.target {
                            PlanTarget::Package(id) => {
                                DashboardAction::InstallPackage(InstallSelection {
                                    id: id.clone(),
                                    artifact_sources,
                                    custom_node_ids,
                                })
                            }
                            PlanTarget::Artifact(id) => {
                                DashboardAction::InstallArtifact(InstallSelection {
                                    id: id.clone(),
                                    artifact_sources,
                                    custom_node_ids,
                                })
                            }
                            PlanTarget::Workflow(id) => {
                                DashboardAction::InstallWorkflow(InstallSelection {
                                    id: id.clone(),
                                    artifact_sources,
                                    custom_node_ids,
                                })
                            }
                        };
                        return Some(action);
                    }
                    PlanRow::Cancel => cancel = true,
                }
            }
            _ => {}
        }
        let cursor = editor.cursor;
        let _ = editor;
        if cancel {
            self.plan_editor = None;
            return None;
        }
        let new_len = self.plan_rows().len();
        if let Some(editor) = self.plan_editor.as_mut() {
            editor.cursor = cursor.min(new_len.saturating_sub(1));
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let (index, len) = match Section::ALL[self.section] {
            Section::Models => (&mut self.model_index, self.catalog.packages.len()),
            Section::Loras
            | Section::Vaes
            | Section::TextEncoders
            | Section::Upscalers
            | Section::RuntimeModels => {
                let len = self.artifacts_for_section().len();
                (&mut self.artifact_index, len)
            }
            Section::CustomNodes => (&mut self.custom_node_index, self.catalog.custom_nodes.len()),
            Section::Workflows => (&mut self.workflow_index, self.catalog.workflows.len()),
            Section::Downloads => (&mut self.download_index, self.queue.len()),
            _ => return,
        };
        if len == 0 {
            *index = 0;
        } else {
            *index = ((*index as isize + delta).rem_euclid(len as isize)) as usize;
        }
    }

    fn selected_action(&self) -> Option<DashboardAction> {
        match Section::ALL[self.section] {
            Section::Models => None,
            Section::Workflows => None,
            Section::Loras
            | Section::Vaes
            | Section::TextEncoders
            | Section::Upscalers
            | Section::RuntimeModels => {
                self.valid_comfy_root()?;
                let artifacts = self.artifacts_for_section();
                let artifact =
                    artifacts.get(self.artifact_index.min(artifacts.len().checked_sub(1)?))?;
                Some(DashboardAction::InstallArtifact(InstallSelection {
                    id: artifact.id.clone(),
                    artifact_sources: vec![(artifact.id.clone(), 0)],
                    custom_node_ids: Vec::new(),
                }))
            }
            Section::CustomNodes => {
                self.valid_comfy_root()?;
                self.catalog
                    .custom_nodes
                    .get(self.custom_node_index)
                    .map(|node| DashboardAction::InstallCustomNode(node.id.clone()))
            }
            Section::Downloads => None,
            Section::System => (!self.catalog.system_dependencies.is_empty())
                .then_some(DashboardAction::InstallSystemDeps),
            _ => None,
        }
    }

    fn valid_comfy_root(&self) -> Option<&Path> {
        self.cfg
            .comfy_path
            .as_deref()
            .filter(|path| ComfyManager::is_comfy_root(path))
    }

    fn package_health(&self, package: &Package) -> Health {
        if package.primary_artifact_ids.is_empty() {
            return Health::Discovery;
        }
        worse_health(
            self.ids_health(
                package
                    .primary_artifact_ids
                    .iter()
                    .chain(&package.dependency_artifact_ids),
            ),
            self.custom_nodes_health(package.custom_node_ids.iter()),
        )
    }

    fn workflow_health(&self, workflow: &WorkflowDefinition) -> Health {
        let deps = worse_health(
            self.ids_health(workflow.artifact_ids.iter()),
            self.custom_nodes_health(workflow.custom_node_ids.iter()),
        );
        let Some(root) = self.valid_comfy_root() else {
            return Health::Missing;
        };
        let installed = root
            .join("user/default/workflows")
            .join(&workflow.file)
            .is_file();
        if !installed && matches!(deps, Health::Ready | Health::Missing) {
            Health::Missing
        } else {
            deps
        }
    }

    fn ids_health<'a>(&self, ids: impl Iterator<Item = &'a String>) -> Health {
        if self.valid_comfy_root().is_none() {
            return Health::Missing;
        }
        let mut health = Health::Ready;
        for id in ids {
            let Some(artifact) = self.catalog.artifact(id) else {
                return Health::Broken;
            };
            health = match self.artifact_health.by_id.get(id).copied() {
                Some(Health::Broken) => return Health::Broken,
                Some(Health::Ready) => health,
                _ if artifact.gated && !self.system.has_token => Health::Blocked,
                Some(Health::Paused) if health != Health::Blocked => Health::Paused,
                Some(Health::Missing) if !matches!(health, Health::Paused | Health::Blocked) => {
                    Health::Missing
                }
                _ => health,
            };
        }
        health
    }

    fn custom_nodes_health<'a>(&self, ids: impl Iterator<Item = &'a String>) -> Health {
        if self.valid_comfy_root().is_none() {
            return Health::Missing;
        }
        for id in ids {
            if self.catalog.custom_node(id).is_none() {
                return Health::Broken;
            }
            if !self.installed_custom_nodes.contains(id) {
                return Health::Missing;
            }
        }
        Health::Ready
    }
}

fn job_badge(status: JobStatus) -> Span<'static> {
    let color = match status {
        JobStatus::Completed => GREEN,
        JobStatus::Downloading => ACCENT,
        JobStatus::Queued => MUTED,
        JobStatus::Paused => YELLOW,
        JobStatus::Failed => RED,
    };
    Span::styled(
        format!("{:<8}", status.label()),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn job_details(job: &crate::download_queue::JobSnapshot) -> Text<'static> {
    let progress = job
        .total_bytes
        .map(|total| {
            format!(
                "{:.1}% · {} / {}",
                job.downloaded_bytes as f64 * 100.0 / total.max(1) as f64,
                bytes_label(job.downloaded_bytes),
                bytes_label(total)
            )
        })
        .unwrap_or_else(|| bytes_label(job.downloaded_bytes));
    let eta = job
        .total_bytes
        .filter(|_| job.bytes_per_second > 0.0)
        .map(|total| {
            Duration::from_secs_f64(
                total.saturating_sub(job.downloaded_bytes) as f64 / job.bytes_per_second,
            )
            .as_secs()
        })
        .map(|seconds| format!("{}m {:02}s", seconds / 60, seconds % 60))
        .unwrap_or_else(|| "—".into());
    let mut lines = vec![
        Line::from(Span::styled(
            job.name.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("State: {}", job.status.label())),
        Line::from(format!("Progress: {progress}")),
        Line::from(format!(
            "Speed: {}/s",
            bytes_label(job.bytes_per_second as u64)
        )),
        Line::from(format!("ETA: {eta}")),
        Line::from(format!("Target: {}", job.relative_path)),
        Line::from(format!(
            "Source: {}",
            job.endpoint.as_deref().unwrap_or("https://huggingface.co")
        )),
        Line::from(format!("ID: {}", job.artifact_id)),
        Line::from(""),
        Line::from("x pauses; c continues a paused job; r retries a failed job."),
        Line::from("Enter focuses this view; b returns the transfer to the background."),
    ];
    if let Some(error) = &job.error {
        lines.push(Line::from(Span::styled(
            format!("Error: {error}"),
            Style::default().fg(RED),
        )));
    }
    Text::from(lines)
}

fn bytes_label(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn card<'a>(title: &'a str, text: Text<'a>) -> Paragraph<'a> {
    Paragraph::new(text).wrap(Wrap { trim: true }).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(PANEL)),
    )
}

fn content_columns(area: Rect) -> Vec<Rect> {
    if area.width >= 108 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(area)
            .to_vec()
    } else {
        vec![area]
    }
}

fn plan_columns(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area)
        .to_vec()
}

fn plan_card<'a>(
    title: &'a str,
    text: Text<'a>,
    editor: Option<&PlanEditor>,
    area: Rect,
) -> Paragraph<'a> {
    let scroll = editor
        .map(|editor| {
            let selected_line = 8usize.saturating_add(editor.cursor);
            selected_line.saturating_sub(area.height.saturating_sub(4) as usize) as u16
        })
        .unwrap_or(0);
    let paragraph = Paragraph::new(text).scroll((scroll, 0));
    let paragraph = if editor.is_some() {
        paragraph
    } else {
        paragraph.wrap(Wrap { trim: true })
    };
    paragraph.block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if editor.is_some() { ACCENT } else { PANEL })),
    )
}

fn label(value: &str) -> Span<'static> {
    Span::styled(format!("{value:<13}"), Style::default().fg(MUTED))
}

fn state_span(value: &str, healthy: bool) -> Span<'static> {
    Span::styled(
        value.to_owned(),
        Style::default().fg(if healthy { GREEN } else { RED }),
    )
}

fn status_badge(health: Health) -> Span<'static> {
    Span::styled(
        format!("{:<8}", health.label()),
        Style::default()
            .fg(health.color())
            .add_modifier(Modifier::BOLD),
    )
}

fn metric_line(name: &str, value: usize, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {value:>3} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(name.to_owned()),
    ])
}

fn warning_line(message: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::raw(message.to_owned()),
    ])
}

fn health_rank(health: Health) -> usize {
    match health {
        Health::Broken => 0,
        Health::Paused => 1,
        Health::Blocked => 2,
        Health::Missing => 3,
        Health::Ready => 4,
        Health::Discovery => 5,
    }
}

fn worse_health(left: Health, right: Health) -> Health {
    if health_rank(left) <= health_rank(right) {
        left
    } else {
        right
    }
}

fn collect_artifact_health(cfg: &AppConfig, catalog: &Catalog) -> ArtifactHealthSnapshot {
    let Some(root) = cfg
        .comfy_path
        .as_deref()
        .filter(|path| ComfyManager::is_comfy_root(path))
    else {
        return ArtifactHealthSnapshot::default();
    };
    let inventory = Inventory {
        comfy_root: root,
        catalog,
    };
    let mut snapshot = ArtifactHealthSnapshot::default();
    for artifact in &catalog.artifacts {
        match inventory.status(artifact) {
            ArtifactStatus::Installed => {
                snapshot.ready += 1;
                snapshot.by_id.insert(artifact.id.clone(), Health::Ready);
            }
            ArtifactStatus::Missing => {
                snapshot.missing += 1;
                snapshot.by_id.insert(artifact.id.clone(), Health::Missing);
            }
            ArtifactStatus::Partial { .. } => {
                snapshot.paused += 1;
                snapshot.by_id.insert(artifact.id.clone(), Health::Paused);
            }
            ArtifactStatus::SizeMismatch { .. } => {
                snapshot.broken += 1;
                snapshot.by_id.insert(artifact.id.clone(), Health::Broken);
            }
        }
    }
    snapshot
}

fn collect_installed_custom_nodes(cfg: &AppConfig, catalog: &Catalog) -> HashSet<String> {
    let Some(root) = cfg
        .comfy_path
        .as_deref()
        .filter(|path| ComfyManager::is_comfy_root(path))
    else {
        return HashSet::new();
    };
    let base = root.join("custom_nodes");
    catalog
        .custom_nodes
        .iter()
        .filter(|node| {
            crate::download_queue::find_equivalent_node_folder(&base, &node.folder_name).is_some()
        })
        .map(|node| node.id.clone())
        .collect()
}

fn collect_system_snapshot(cfg: &AppConfig, catalog: &Catalog) -> SystemSnapshot {
    let root = cfg
        .comfy_path
        .as_deref()
        .filter(|path| ComfyManager::is_comfy_root(path));
    let credential = hf_credential();
    SystemSnapshot {
        git: command_available("git"),
        python: root.and_then(find_python),
        disk: root.and_then(disk_free_for),
        token_status: credential
            .as_ref()
            .map(|value| format!("present · {}", value.source().label()))
            .unwrap_or_else(|| "missing".into()),
        has_token: credential.is_some(),
        python_dependencies_ready: root
            .is_some_and(crate::download_queue::python_dependencies_ready),
        dependency_status: catalog
            .system_dependencies
            .iter()
            .map(|dependency| {
                (
                    dependency.id.clone(),
                    system_dependency_ready(&dependency.id),
                )
            })
            .collect(),
    }
}

fn hf_credential() -> Option<auth::HfCredential> {
    auth::resolve_hf_token().ok().flatten()
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1_073_741_824.0
}

fn detect_comfy_running(cfg: &AppConfig, _state: &ManagedState) -> bool {
    let Some(root) = cfg
        .comfy_path
        .as_deref()
        .filter(|path| ComfyManager::is_comfy_root(path))
    else {
        return false;
    };
    let mut state = ManagedState::load().unwrap_or_default();
    let instance = comfybox_core::comfy::ComfyInstance {
        root: root.to_path_buf(),
        python: find_python(root),
    };
    ComfyManager::status(&instance, &mut state).unwrap_or(false)
}

fn find_python(root: &Path) -> Option<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![
            root.join("python_embeded/python.exe"),
            root.join(".venv/Scripts/python.exe"),
            root.join("venv/Scripts/python.exe"),
        ]
    } else {
        vec![root.join(".venv/bin/python"), root.join("venv/bin/python")]
    };
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| {
            let command = if cfg!(windows) { "python" } else { "python3" };
            command_available(command).then(|| PathBuf::from(command))
        })
}

fn disk_free_for(root: &Path) -> Option<u64> {
    storage::storage_candidates()
        .ok()?
        .into_iter()
        .filter(|disk| root.starts_with(&disk.mount_point))
        .max_by_key(|disk| disk.mount_point.as_os_str().len())
        .map(|disk| disk.available_bytes)
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn system_dependency_ready(id: &str) -> bool {
    match id {
        "libegl" => Command::new("ldconfig")
            .arg("-p")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("libEGL.so.1")),
        "ffmpeg" => Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_priority_puts_recovery_first() {
        assert!(health_rank(Health::Broken) < health_rank(Health::Paused));
        assert!(health_rank(Health::Paused) < health_rank(Health::Missing));
        assert!(health_rank(Health::Missing) < health_rank(Health::Ready));
    }
}
