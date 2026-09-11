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
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
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
    SetHfToken,
    ToggleServer,
    InstallPackage(String),
    InstallWorkflow(String),
    InstallArtifact { id: String, force: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Overview,
    Models,
    Workflows,
    Downloads,
    System,
}

impl Section {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Models,
        Self::Workflows,
        Self::Downloads,
        Self::System,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Models => "Models",
            Self::Workflows => "Workflows",
            Self::Downloads => "Downloads",
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
    cfg: &'a AppConfig,
    catalog: &'a Catalog,
    section: usize,
    model_index: usize,
    workflow_index: usize,
    download_index: usize,
    comfy_running: bool,
    state: ManagedState,
}

pub fn run(cfg: &AppConfig, catalog: &Catalog) -> Result<DashboardAction> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        anyhow::bail!(
            "the dashboard requires an interactive terminal; use a subcommand for non-interactive runs"
        );
    }

    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear()?;

    let state = ManagedState::load().unwrap_or_default();
    let comfy_running = detect_comfy_running(cfg, &state);
    let mut app = Dashboard {
        cfg,
        catalog,
        section: 0,
        model_index: 0,
        workflow_index: 0,
        download_index: 0,
        comfy_running,
        state,
    };

    let mut needs_draw = true;
    loop {
        if needs_draw {
            terminal.draw(|frame| app.render(frame))?;
            needs_draw = false;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
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
            Section::Workflows => self.render_workflows(frame, vertical[2]),
            Section::Downloads => self.render_downloads(frame, vertical[2]),
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
            .map(|section| Line::from(format!(" {} ", section.title())))
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
        let token = if let Some(credential) = hf_credential() {
            Span::styled(
                format!("present · {}", credential.source().label()),
                Style::default().fg(GREEN),
            )
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
            Line::from(vec![label("HF token"), token]),
            Line::from(vec![label("Endpoint"), Span::raw(endpoint)]),
        ];
        frame.render_widget(card(" Environment ", Text::from(lines)), area);
    }

    fn render_library_card(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(root) = self.valid_comfy_root() else {
            frame.render_widget(
                card(
                    " Library ",
                    Text::from("Configure a valid ComfyUI installation to scan models."),
                ),
                area,
            );
            return;
        };
        let inventory = Inventory {
            comfy_root: root,
            catalog: self.catalog,
        };
        let mut ready = 0;
        let mut missing = 0;
        let mut paused = 0;
        let mut broken = 0;
        for artifact in &self.catalog.artifacts {
            match inventory.status(artifact) {
                ArtifactStatus::Installed => ready += 1,
                ArtifactStatus::Missing => missing += 1,
                ArtifactStatus::Partial { .. } => paused += 1,
                ArtifactStatus::SizeMismatch { .. } => broken += 1,
            }
        }
        let lines = vec![
            metric_line("Ready", ready, GREEN),
            metric_line("Missing", missing, MUTED),
            metric_line("Paused", paused, YELLOW),
            metric_line("Broken", broken, RED),
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
        if !has_hf_token() {
            lines.push(warning_line("HF_TOKEN is not set.", YELLOW));
            lines.push(Line::from(
                "  Public downloads work; press t to save a token for gated repositories.",
            ));
        }
        if let Some(root) = self.valid_comfy_root() {
            let inventory = Inventory {
                comfy_root: root,
                catalog: self.catalog,
            };
            let paused = self
                .catalog
                .artifacts
                .iter()
                .filter(|a| matches!(inventory.status(a), ArtifactStatus::Partial { .. }))
                .count();
            let broken = self
                .catalog
                .artifacts
                .iter()
                .filter(|a| matches!(inventory.status(a), ArtifactStatus::SizeMismatch { .. }))
                .count();
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
        let columns = content_columns(area);
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
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default()
            .with_selected(Some(self.model_index.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(PANEL).add_modifier(Modifier::BOLD))
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
                    Text::from(vec![
                        Line::from(Span::styled(
                            &package.name,
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(format!("Family: {}", package.family)),
                        Line::from(format!("Required artifacts: {required}")),
                        Line::from(format!(
                            "Optional groups: {}",
                            package.optional_groups.len()
                        )),
                        Line::from(""),
                        Line::from(package.description.as_deref().unwrap_or(
                            "Enter installs the package and its required dependencies.",
                        )),
                    ])
                })
                .unwrap_or_default();
            frame.render_widget(card(" Details ", details), columns[1]);
        }
    }

    fn render_workflows(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = content_columns(area);
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
        let mut state = ListState::default()
            .with_selected(Some(self.workflow_index.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(PANEL).add_modifier(Modifier::BOLD))
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
                Text::from(vec![
                    Line::from(Span::styled(&workflow.name, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
                    Line::from(format!("File: {}", workflow.file)),
                    Line::from(format!("Models: {}", workflow.artifact_ids.len())),
                    Line::from(format!("Custom nodes: {}", workflow.custom_node_ids.len())),
                    Line::from(""),
                    Line::from("Enter installs the workflow, models, VAEs, text encoders, LoRAs, and custom nodes."),
                ])
            }).unwrap_or_default();
            frame.render_widget(card(" Install plan ", details), columns[1]);
        }
    }

    fn render_downloads(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let columns = content_columns(area);
        self.download_index = self
            .download_index
            .min(self.catalog.artifacts.len().saturating_sub(1));
        let artifacts = self.sorted_artifacts();
        let items = artifacts
            .iter()
            .map(|artifact| {
                let (health, detail) = self.artifact_health_detail(artifact);
                ListItem::new(Line::from(vec![
                    status_badge(health),
                    Span::raw(" "),
                    Span::styled(artifact.name.clone(), Style::default().fg(Color::White)),
                    Span::styled(format!("  {detail}"), Style::default().fg(MUTED)),
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
                        .title(" Artifact downloads ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(PANEL)),
                ),
            columns[0],
            &mut state,
        );
        if columns.len() > 1 {
            let details = artifacts
                .get(self.download_index)
                .map(|artifact| {
                    let (health, detail) = self.artifact_health_detail(artifact);
                    let hint = match health {
                        Health::Paused => "Enter retries; verified ranges resume where supported.",
                        Health::Broken => "Press f to force a safe replacement.",
                        Health::Blocked => "Press t to save an HF token, then retry.",
                        Health::Missing => "Enter starts this download.",
                        Health::Ready => "The artifact is installed.",
                        Health::Discovery => "",
                    };
                    Text::from(vec![
                        Line::from(Span::styled(
                            &artifact.name,
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(format!("State: {} — {detail}", health.label())),
                        Line::from(format!("Target: {}", artifact.relative_path)),
                        Line::from(format!(
                            "Integrity: {}",
                            if artifact.sha256.is_some() {
                                "SHA-256 pinned"
                            } else if artifact.size_bytes.is_some() {
                                "size pinned"
                            } else {
                                "remote size"
                            }
                        )),
                        Line::from(""),
                        Line::from(hint),
                    ])
                })
                .unwrap_or_default();
            frame.render_widget(card(" Recovery ", details), columns[1]);
        }
    }

    fn render_system(&self, frame: &mut Frame<'_>, area: Rect) {
        let root = self.valid_comfy_root();
        let git = command_available("git");
        let python = root.and_then(find_python);
        let disk = root.and_then(disk_free_for);
        let credential = hf_credential();
        let token_status = credential
            .as_ref()
            .map(|value| format!("present · {}", value.source().label()))
            .unwrap_or_else(|| "missing".into());
        let lines = vec![
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
                label("HF_TOKEN"),
                state_span(&token_status, credential.is_some()),
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
        let help = if area.width < 100 {
            match Section::ALL[self.section] {
                Section::Models | Section::Workflows => {
                    " ←/→ tabs  ↑/↓ select  Enter install  s server  t token  r refresh  q quit "
                        .into()
                }
                Section::Downloads => {
                    " ←/→ tabs  ↑/↓ select  Enter retry  f repair  t token  r refresh  q quit "
                        .into()
                }
                _ => " ←/→ tabs  s server  l locate  i install  t token  r refresh  q quit ".into(),
            }
        } else {
            let section_hint = match Section::ALL[self.section] {
                Section::Models | Section::Workflows => " Enter install ",
                Section::Downloads => " Enter resume/install  f repair ",
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(DashboardAction::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(DashboardAction::Quit);
            }
            KeyCode::Right | KeyCode::Tab => self.section = (self.section + 1) % Section::ALL.len(),
            KeyCode::Left | KeyCode::BackTab => {
                self.section = (self.section + Section::ALL.len() - 1) % Section::ALL.len()
            }
            KeyCode::Char('1'..='5') => {
                if let KeyCode::Char(value) = key.code {
                    self.section = value.to_digit(10).unwrap_or(1) as usize - 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('s') if self.valid_comfy_root().is_some() => {
                return Some(DashboardAction::ToggleServer);
            }
            KeyCode::Char('l') => return Some(DashboardAction::LocateComfyUi),
            KeyCode::Char('i') => return Some(DashboardAction::InstallComfyUi),
            KeyCode::Char('t') => return Some(DashboardAction::SetHfToken),
            KeyCode::Char('p') if self.valid_comfy_root().is_some() => {
                return Some(DashboardAction::InstallPythonDeps);
            }
            KeyCode::Char('r') => {
                self.state = ManagedState::load().unwrap_or_default();
                self.comfy_running = detect_comfy_running(self.cfg, &self.state);
            }
            KeyCode::Enter => return self.selected_action(false),
            KeyCode::Char('f') if Section::ALL[self.section] == Section::Downloads => {
                return self.selected_action(true);
            }
            _ => {}
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let (index, len) = match Section::ALL[self.section] {
            Section::Models => (&mut self.model_index, self.catalog.packages.len()),
            Section::Workflows => (&mut self.workflow_index, self.catalog.workflows.len()),
            Section::Downloads => (&mut self.download_index, self.catalog.artifacts.len()),
            _ => return,
        };
        if len == 0 {
            *index = 0;
        } else {
            *index = ((*index as isize + delta).rem_euclid(len as isize)) as usize;
        }
    }

    fn selected_action(&self, force: bool) -> Option<DashboardAction> {
        self.valid_comfy_root()?;
        match Section::ALL[self.section] {
            Section::Models => {
                let package = self.catalog.packages.get(self.model_index)?;
                if package.primary_artifact_ids.is_empty()
                    || self.package_health(package) == Health::Blocked
                {
                    None
                } else {
                    Some(DashboardAction::InstallPackage(package.id.clone()))
                }
            }
            Section::Workflows => {
                let workflow = self.catalog.workflows.get(self.workflow_index)?;
                (self.workflow_health(workflow) != Health::Blocked)
                    .then(|| DashboardAction::InstallWorkflow(workflow.id.clone()))
            }
            Section::Downloads => {
                let artifact = self.sorted_artifacts().get(self.download_index).copied()?;
                if self.artifact_health_detail(artifact).0 == Health::Blocked {
                    return None;
                }
                if matches!(
                    self.artifact_status(artifact),
                    Some(ArtifactStatus::Installed)
                ) && !force
                {
                    None
                } else {
                    Some(DashboardAction::InstallArtifact {
                        id: artifact.id.clone(),
                        force,
                    })
                }
            }
            _ => None,
        }
    }

    fn valid_comfy_root(&self) -> Option<&Path> {
        self.cfg
            .comfy_path
            .as_deref()
            .filter(|path| ComfyManager::is_comfy_root(path))
    }

    fn artifact_status(&self, artifact: &Artifact) -> Option<ArtifactStatus> {
        let root = self.valid_comfy_root()?;
        Some(
            Inventory {
                comfy_root: root,
                catalog: self.catalog,
            }
            .status(artifact),
        )
    }

    fn artifact_health_detail(&self, artifact: &Artifact) -> (Health, String) {
        match self.artifact_status(artifact) {
            Some(ArtifactStatus::Installed) => (Health::Ready, size_label(artifact.size_bytes)),
            Some(ArtifactStatus::SizeMismatch { actual, expected }) => (
                Health::Broken,
                format!(
                    "{:.1} GiB found · {:.1} expected",
                    gib(actual),
                    gib(expected)
                ),
            ),
            _ if artifact.gated && !has_hf_token() => (
                Health::Blocked,
                "gated repository · HF_TOKEN missing".into(),
            ),
            Some(ArtifactStatus::Missing) | None => {
                (Health::Missing, size_label(artifact.size_bytes))
            }
            Some(ArtifactStatus::Partial {
                downloaded_bytes,
                expected_bytes,
            }) => {
                let detail = if let Some(total) = expected_bytes {
                    format!(
                        "{:.1}% · {:.1}/{:.1} GiB",
                        downloaded_bytes as f64 * 100.0 / total.max(1) as f64,
                        gib(downloaded_bytes),
                        gib(total)
                    )
                } else {
                    format!("{:.1} GiB retained", gib(downloaded_bytes))
                };
                (Health::Paused, detail)
            }
        }
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
        let Some(root) = self.valid_comfy_root() else {
            return Health::Missing;
        };
        let inventory = Inventory {
            comfy_root: root,
            catalog: self.catalog,
        };
        let mut health = Health::Ready;
        for id in ids {
            let Some(artifact) = self.catalog.artifact(id) else {
                return Health::Broken;
            };
            health = match inventory.status(artifact) {
                ArtifactStatus::SizeMismatch { .. } => return Health::Broken,
                ArtifactStatus::Installed => health,
                _ if artifact.gated && !has_hf_token() => Health::Blocked,
                ArtifactStatus::Partial { .. } if health != Health::Blocked => Health::Paused,
                ArtifactStatus::Missing if !matches!(health, Health::Paused | Health::Blocked) => {
                    Health::Missing
                }
                _ => health,
            };
        }
        health
    }

    fn custom_nodes_health<'a>(&self, ids: impl Iterator<Item = &'a String>) -> Health {
        let Some(root) = self.valid_comfy_root() else {
            return Health::Missing;
        };
        for id in ids {
            let Some(node) = self.catalog.custom_node(id) else {
                return Health::Broken;
            };
            if !root.join("custom_nodes").join(&node.folder_name).is_dir() {
                return Health::Missing;
            }
        }
        Health::Ready
    }

    fn sorted_artifacts(&self) -> Vec<&Artifact> {
        let mut artifacts = self.catalog.artifacts.iter().collect::<Vec<_>>();
        artifacts.sort_by(|left, right| {
            let left_health = self.artifact_health_detail(left).0;
            let right_health = self.artifact_health_detail(right).0;
            health_rank(left_health)
                .cmp(&health_rank(right_health))
                .then_with(|| left.name.cmp(&right.name))
        });
        artifacts
    }
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

fn size_label(bytes: Option<u64>) -> String {
    bytes
        .map(|value| format!("{:.1} GiB", gib(value)))
        .unwrap_or_else(|| "remote size".into())
}

fn hf_credential() -> Option<auth::HfCredential> {
    auth::resolve_hf_token().ok().flatten()
}

fn has_hf_token() -> bool {
    hf_credential().is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_priority_puts_recovery_first() {
        assert!(health_rank(Health::Broken) < health_rank(Health::Paused));
        assert!(health_rank(Health::Paused) < health_rank(Health::Missing));
        assert!(health_rank(Health::Missing) < health_rank(Health::Ready));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(size_label(Some(1_073_741_824)), "1.0 GiB");
        assert_eq!(size_label(None), "remote size");
    }
}
