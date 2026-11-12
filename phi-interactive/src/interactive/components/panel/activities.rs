use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::FutureExt;
use phi_sdk::agent_instance::{BashExecution, BashMode, BashStatus};
use phi_sdk::agent_manager::AgentRegistrationId;
use phi_sdk::tasks::{TaskItemView, TaskListView, TaskStatusView};
use phi_tui::{
    InputContext, InputOutcome, KeyInput, MountOptions, NotifyKind, OverlayOptions, OverlaySize,
    Panel, RenderArea, RenderContext, SelectMenuState, StyledLine, Theme, ThemeColor, Widget, text,
};

use crate::interactive::components::input::UiAction;
use crate::interactive::overlays::ActiveOverlay;
use crate::interactive::state::{SubagentStatus, SubagentView, UiState};
use crate::runtime::attachment::{AgentActivities, AgentAttachment, AgentChanges};

const MAX_PROGRESS_ROWS: usize = 3;
const PANEL_MAX_HEIGHT: usize = 24;
const TASK_OWNER_GAP: &str = "   ";
const WORK_PANEL_MAX_WIDTH: u16 = 96;
const WORK_PANEL_MIN_HEIGHT_PERCENT: usize = 50;
const WORK_PANEL_MAX_HEIGHT_PERCENT: u16 = 80;
const BASH_SELECTOR_MAX_WIDTH: u16 = 80;
const BASH_SELECTOR_VISIBLE_ROWS: usize = 6;

pub(in crate::interactive) fn work_panel_options() -> OverlayOptions {
    OverlayOptions {
        width: Some(OverlaySize::Percent(80)),
        max_width: Some(WORK_PANEL_MAX_WIDTH),
        max_height: Some(OverlaySize::Percent(WORK_PANEL_MAX_HEIGHT_PERCENT)),
        ..OverlayOptions::centered_panel()
    }
}

fn work_panel_lines(
    width: u16,
    height: usize,
    heading: StyledLine,
    hint: &str,
    content: Vec<StyledLine>,
) -> Vec<StyledLine> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let minimum_height = height
        .saturating_mul(WORK_PANEL_MIN_HEIGHT_PERCENT)
        .div_ceil(usize::from(WORK_PANEL_MAX_HEIGHT_PERCENT))
        .min(height);
    if width < 4 || height < 5 {
        let mut lines = vec![heading.fit(width)];
        lines.resize(
            minimum_height,
            StyledLine::new("", ThemeColor::Default).fit(width),
        );
        return lines;
    }
    let inner_width = width - 4;
    let horizontal = "─".repeat(usize::from(width - 2));
    let mut lines = vec![StyledLine::new(format!("╭{horizontal}╮"), ThemeColor::Dim)];
    for (index, row) in [heading, StyledLine::new(hint, ThemeColor::Dim)]
        .into_iter()
        .chain(content.into_iter().take(height.saturating_sub(5)))
        .enumerate()
    {
        let background = row.bg;
        let mut line = row.fit(inner_width).prepend_segment("│ ", ThemeColor::Dim);
        line.segments
            .push(phi_tui::StyledSegment::new(" │", ThemeColor::Dim));
        line.text.push_str(" │");
        line.bg = background;
        lines.push(line);
        if index == 1 {
            lines.push(StyledLine::new(format!("├{horizontal}┤"), ThemeColor::Dim));
        }
    }
    while lines.len() + 1 < minimum_height {
        lines.push(StyledLine::segments([
            ("│ ".to_owned(), ThemeColor::Dim),
            (" ".repeat(usize::from(inner_width)), ThemeColor::Default),
            (" │".to_owned(), ThemeColor::Dim),
        ]));
    }
    lines.push(StyledLine::new(format!("╰{horizontal}╯"), ThemeColor::Dim));
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskRowMode {
    Progress,
    Listed,
    Selected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::interactive) enum TaskVisibility {
    Visible,
    Hidden,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskRegion {
    Checklist,
    Activities,
}

#[derive(Clone)]
pub(in crate::interactive) struct TasksOverlay {
    pub(in crate::interactive) checklist: TaskPanel,
    pub(in crate::interactive) activities: ActivityPanel,
    region: TaskRegion,
}

impl TasksOverlay {
    pub(in crate::interactive) fn new(state: &UiState) -> Self {
        let mut activities =
            ActivityPanel::new(state.activity_agents.clone(), state.bash_views.clone());
        activities.restore_selection(state.activity_selection.as_ref());
        Self {
            checklist: state.tasks.clone(),
            activities,
            region: if state.tasks.items().is_empty() {
                TaskRegion::Activities
            } else {
                TaskRegion::Checklist
            },
        }
    }

    pub(in crate::interactive) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        match key {
            KeyInput::Ctrl('t') | KeyInput::Ctrl('c') | KeyInput::Esc => {
                UiAction::CloseActiveOverlay
            }
            KeyInput::Tab | KeyInput::Left | KeyInput::Right => {
                self.region = match self.region {
                    TaskRegion::Checklist => TaskRegion::Activities,
                    TaskRegion::Activities => TaskRegion::Checklist,
                };
                UiAction::None
            }
            _ => match self.region {
                TaskRegion::Checklist => self.checklist.handle_key(key),
                TaskRegion::Activities => self.activities.handle_key(key),
            },
        }
    }

    pub(in crate::interactive) fn render_lines(
        &self,
        width: u16,
        height: usize,
    ) -> Vec<StyledLine> {
        let inner_width = width.saturating_sub(4);
        let tabs = [
            (TaskRegion::Checklist, "checklist"),
            (TaskRegion::Activities, "agents & commands"),
        ];
        let mut used = 0;
        let mut segments = Vec::new();
        for (index, (region, label)) in tabs.iter().enumerate() {
            let selected = *region == self.region;
            let label = if selected {
                format!("[{label}]")
            } else {
                (*label).into()
            };
            let center = ((index * 2 + 1) * usize::from(inner_width)) / (tabs.len() * 2);
            let start = center.saturating_sub(text::display_width(&label) / 2);
            let padding = start.saturating_sub(used);
            segments.push((" ".repeat(padding), ThemeColor::Default));
            used += padding + text::display_width(&label);
            segments.push((
                label,
                if selected {
                    ThemeColor::Accent
                } else {
                    ThemeColor::Dim
                },
            ));
        }
        let hint = match self.region {
            TaskRegion::Checklist => {
                "tab / ← → switch tab · ↑↓ select · enter agent · ctrl+↑↓ details · esc close"
            }
            TaskRegion::Activities => {
                "tab / ← → switch tab · ↑↓ select · enter details · ctrl+d stop · esc close"
            }
        };
        let panel_height = height.saturating_sub(5);
        let content = match self.region {
            TaskRegion::Checklist => self.checklist.render_lines(inner_width, panel_height),
            TaskRegion::Activities => self.activities.render_lines(inner_width, panel_height),
        };
        work_panel_lines(width, height, StyledLine::segments(segments), hint, content)
    }
}

#[derive(Clone, Default)]
pub(in crate::interactive) struct TaskPanel {
    view: Option<Arc<TaskListView>>,
    owners: HashMap<String, String>,
    selected: usize,
    detail_offset: usize,
    loaded: bool,
    error: Option<String>,
    notice: Option<String>,
    pub(in crate::interactive) expanded: bool,
}

impl TaskPanel {
    fn items(&self) -> &[TaskItemView] {
        self.view.as_ref().map_or(&[], |view| view.tasks.as_slice())
    }

    fn replace(&mut self, view: Option<TaskListView>) -> bool {
        if self.loaded && self.error.is_none() && self.view.as_deref() == view.as_ref() {
            return false;
        }
        let selected = self.items().get(self.selected).map(|task| task.id.clone());
        self.view = view.map(Arc::new);
        self.selected = selected
            .and_then(|id| self.items().iter().position(|task| task.id == id))
            .unwrap_or_else(|| self.selected.min(self.items().len().saturating_sub(1)));
        self.loaded = true;
        self.error = None;
        true
    }

    pub(in crate::interactive) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        match key {
            KeyInput::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.detail_offset = 0;
                self.notice = None;
            }
            KeyInput::Down => {
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
                self.detail_offset = 0;
                self.notice = None;
            }
            KeyInput::CtrlUp => self.detail_offset = self.detail_offset.saturating_sub(1),
            KeyInput::CtrlDown => self.detail_offset = self.detail_offset.saturating_add(1),
            KeyInput::Enter => {
                if let Some(task) = self.items().get(self.selected) {
                    if let Some(owner) = &task.owner {
                        return UiAction::OpenTaskOwner(owner.as_str().to_owned());
                    }
                    self.notice = Some("This task has no assigned agent.".into());
                }
            }
            _ => {}
        }
        UiAction::None
    }

    fn summary(&self) -> String {
        let tasks = self.items();
        let completed = tasks
            .iter()
            .filter(|task| task.status == TaskStatusView::Completed)
            .count();
        let active = tasks
            .iter()
            .filter(|task| task.status == TaskStatusView::InProgress)
            .count();
        let blocked = tasks
            .iter()
            .filter(|task| !task.unresolved.is_empty() && task.status != TaskStatusView::Completed)
            .count();
        format!(
            "Tasks · {completed}/{} completed · {active} in progress · {blocked} blocked",
            tasks.len()
        )
    }

    fn task_row(&self, task: &TaskItemView, mode: TaskRowMode, width: u16) -> StyledLine {
        let (marker, color) = match task.status {
            TaskStatusView::Pending => ("[ ]", ThemeColor::Default),
            TaskStatusView::InProgress => ("[-]", ThemeColor::Warning),
            TaskStatusView::Completed => ("[x]", ThemeColor::Success),
        };
        let prefix = match mode {
            TaskRowMode::Selected => "> ",
            TaskRowMode::Listed | TaskRowMode::Progress => "  ",
        };
        let subject = if task.status == TaskStatusView::InProgress {
            task.active_form.as_deref().unwrap_or(&task.subject)
        } else {
            &task.subject
        };
        let owner = task
            .owner
            .as_ref()
            .map(|owner| {
                self.owners
                    .get(owner.as_str())
                    .map_or(owner.as_str(), String::as_str)
            })
            .unwrap_or("—");
        let owner_width = usize::from(width / 3).min(text::display_width(owner));
        let owner = text::truncate(owner, owner_width);
        let label_width = usize::from(width).saturating_sub(
            text::display_width(prefix) + marker.len() + owner_width + 1 + TASK_OWNER_GAP.len(),
        );
        let label = text::truncate(
            &format!("#{} {}", task.id, single_line(subject)),
            label_width,
        );
        let (prefix_color, label_color) = if mode == TaskRowMode::Progress {
            (ThemeColor::Dim, ThemeColor::Dim)
        } else {
            (ThemeColor::Accent, ThemeColor::Default)
        };
        let line = StyledLine::segments([
            (prefix.to_owned(), prefix_color),
            (marker.to_owned(), color),
            (format!(" {label}{TASK_OWNER_GAP}"), label_color),
            (owner, ThemeColor::Dim),
        ])
        .fit(width);
        if mode == TaskRowMode::Selected {
            line.bg(ThemeColor::SelectedBg)
        } else {
            line
        }
    }

    pub(in crate::interactive) fn progress_lines(&self, width: u16) -> Vec<StyledLine> {
        let mut lines = Vec::new();
        if !self.items().is_empty() {
            lines.push(StyledLine::new("", ThemeColor::Default).fit(width));
            lines.push(
                StyledLine::new(
                    format!("{} · /tasks · ctrl+t", self.summary()),
                    ThemeColor::Dim,
                )
                .fit(width),
            );
            if self.expanded {
                for task in [
                    TaskStatusView::InProgress,
                    TaskStatusView::Pending,
                    TaskStatusView::Completed,
                ]
                .into_iter()
                .flat_map(|status| {
                    self.items()
                        .iter()
                        .filter(move |task| task.status == status)
                })
                .take(MAX_PROGRESS_ROWS)
                {
                    lines.push(self.task_row(task, TaskRowMode::Progress, width));
                    if !task.unresolved.is_empty() {
                        lines.push(
                            StyledLine::new(
                                format!("    blocked by {}", dependencies(&task.unresolved)),
                                ThemeColor::Dim,
                            )
                            .fit(width),
                        );
                    }
                }
            }
        }
        if let Some(error) = &self.error {
            lines.push(StyledLine::new(single_line(error), ThemeColor::Error).fit(width));
        }
        lines
    }

    pub(in crate::interactive) fn render_lines(
        &self,
        width: u16,
        height: usize,
    ) -> Vec<StyledLine> {
        let height = height.min(PANEL_MAX_HEIGHT);
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let inner = width;
        let mut rows = Vec::new();
        if !self.items().is_empty() {
            rows.push(StyledLine::new(self.summary(), ThemeColor::Title));
        }
        if !self.loaded && self.error.is_none() {
            rows.push(StyledLine::new("Loading tasks…", ThemeColor::Dim));
        }
        for error in [self.error.as_ref(), self.notice.as_ref()]
            .into_iter()
            .flatten()
        {
            rows.push(StyledLine::new(single_line(error), ThemeColor::Error));
        }
        if !self.items().is_empty() {
            rows.push(StyledLine::new("", ThemeColor::Default));
        }
        let budget = height.saturating_sub(rows.len() + 1);
        let list_rows = budget.div_ceil(2).min(self.items().len());
        let start = self.selected.saturating_sub(list_rows.saturating_sub(1));
        for (index, task) in self.items().iter().enumerate().skip(start).take(list_rows) {
            let mode = if index == self.selected {
                TaskRowMode::Selected
            } else {
                TaskRowMode::Listed
            };
            rows.push(self.task_row(task, mode, inner));
        }
        if list_rows > 0 {
            rows.push(StyledLine::new("", ThemeColor::Default));
        }
        let detail_rows = height.saturating_sub(rows.len());
        if let Some(task) = self.items().get(self.selected) {
            let detail = format!(
                "Status: {} · Owner: {}\nPrerequisites: {}\nBlocked by: {} · Blocks: {}\n{}",
                match task.status {
                    TaskStatusView::Pending => "pending",
                    TaskStatusView::InProgress => "in_progress",
                    TaskStatusView::Completed => "completed",
                },
                task.owner.as_ref().map_or("—", |owner| owner.as_str()),
                dependencies(&task.blocked_by),
                dependencies(&task.unresolved),
                dependencies(&task.blocks),
                task.description,
            );
            let detail = text::wrap(&detail, usize::from(inner));
            let start = self
                .detail_offset
                .min(detail.len().saturating_sub(detail_rows.max(1)));
            rows.extend(
                detail
                    .into_iter()
                    .skip(start)
                    .take(detail_rows)
                    .map(|line| StyledLine::new(line, ThemeColor::Dim)),
            );
        }
        rows.into_iter()
            .take(height)
            .map(|line| line.fit(width))
            .collect()
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dependencies(ids: &[String]) -> String {
    if ids.is_empty() {
        "—".into()
    } else {
        ids.iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(in crate::interactive) async fn open_owner(
    id: String,
    subagents: &mut crate::interactive::agent::SubagentProjection,
    attachment: &AgentAttachment,
    state: &mut UiState,
) {
    let result = if id == attachment.id().as_str() {
        subagents.switch_to_main();
        Ok(true)
    } else {
        subagents.open(id, attachment).await
    };
    let error = match result {
        Ok(true) => {
            close_tasks(state);
            return;
        }
        Ok(false) => "The responsible agent's session is unavailable.".into(),
        Err(error) => format!("Cannot open the responsible agent: {error}"),
    };
    retain_tasks(state);
    state.tasks.notice = Some(error);
    sync_tasks(state);
}

type TaskRead = tokio::task::JoinHandle<(
    Option<phi_sdk::tasks::TaskSubscription>,
    anyhow::Result<Option<TaskListView>>,
)>;

#[derive(Default)]
pub(in crate::interactive) struct TaskRefresh {
    registration: Option<AgentRegistrationId>,
    pending: Option<TaskRead>,
    read_requested: bool,
    panel_open: bool,
    task_changes: Option<phi_sdk::tasks::TaskSubscription>,
    changes: Option<AgentChanges>,
    children: Vec<(AgentAttachment, AgentChanges)>,
    observed_children: Vec<SubagentView>,
}

impl Drop for TaskRefresh {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}

impl TaskRefresh {
    pub(in crate::interactive) async fn poll(
        &mut self,
        attachment: &AgentAttachment,
        state: &mut UiState,
        visibility: TaskVisibility,
    ) -> bool {
        let registration = attachment.registration_id().ok().flatten();
        let mut changed = false;
        if self.registration != registration {
            if let Some(task) = self.pending.take() {
                task.abort();
            }
            self.registration = registration;
            self.read_requested = true;
            self.task_changes = None;
            self.changes = None;
            self.children.clear();
            state.close_bash_selector();
            close_tasks(state);
            state.tasks = TaskPanel::default();
            state.bash_views.clear();
            state.activity_agents.clear();
            state.activity_selection = None;
            changed = true;
        }
        let initial = self.changes.is_none();
        let changes = self
            .changes
            .get_or_insert_with(|| attachment.subscribe_changes());
        let mut activity_changed = initial | changes.changed();
        for (_, changes) in &mut self.children {
            activity_changed |= changes.changed();
        }
        activity_changed |= self.observed_children != state.subagent_views;
        if activity_changed {
            self.observed_children.clone_from(&state.subagent_views);
            match attachment.activities().await {
                Ok(activities) => {
                    let agents = &activities.agents;
                    self.children.retain(|(owner, _)| {
                        owner.snapshot().status
                            != phi_sdk::agent_instance::AgentInstanceStatus::Stopped
                            && agents
                                .iter()
                                .any(|agent| agent.child_agent_id == owner.id().as_str())
                    });
                    for agent in agents {
                        if !self
                            .children
                            .iter()
                            .any(|(owner, _)| owner.id().as_str() == agent.child_agent_id)
                            && let Ok(owner) =
                                attachment.child_attachment(&agent.child_agent_id).await
                        {
                            self.children
                                .push((owner.clone(), owner.subscribe_changes()));
                        }
                    }
                    changed |= refresh_views(attachment, state, activities).await;
                }
                Err(error) => {
                    state.notify(
                        NotifyKind::Warning,
                        format!("Cannot observe Agent activities: {error}"),
                    );
                    changed = true;
                }
            }
        }
        if let Some(result) = self
            .task_changes
            .as_mut()
            .and_then(|changes| changes.changed().now_or_never())
        {
            self.read_requested = true;
            if let Err(error) = result {
                self.task_changes = None;
                state.notify(
                    NotifyKind::Warning,
                    format!("Task observation failed: {error}"),
                );
                changed = true;
            }
        }
        if self.pending.as_ref().is_some_and(|task| task.is_finished()) {
            let result = match self.pending.take().unwrap().await {
                Ok((subscription, result)) => {
                    self.task_changes = subscription;
                    result
                }
                Err(error) => Err(anyhow::Error::new(error)),
            };
            retain_tasks(state);
            let updated = match result {
                Ok(view) => state.tasks.replace(view),
                Err(error) => {
                    let error = format!("Tasks not refreshed: {error}");
                    let updated = state.tasks.error.as_ref() != Some(&error);
                    state.tasks.error = Some(error);
                    updated
                }
            };
            if updated {
                sync_tasks(state);
            }
            changed |= updated;
        }
        let owners = state
            .activity_agents
            .iter()
            .map(|agent| {
                (
                    agent.id.clone(),
                    agent.alias.clone().unwrap_or_else(|| agent.id.clone()),
                )
            })
            .collect();
        if state.tasks.owners != owners {
            retain_tasks(state);
            state.tasks.owners = owners;
            sync_tasks(state);
            changed = true;
        }
        let panel_open = tasks_open(state);
        if panel_open && !self.panel_open {
            self.read_requested = true;
        }
        self.panel_open = panel_open;
        if visibility == TaskVisibility::Visible && self.pending.is_none() && self.read_requested {
            self.read_requested = false;
            let attachment = attachment.clone();
            let subscription = self.task_changes.take();
            self.pending = Some(tokio::spawn(async move {
                let subscription = match subscription {
                    Some(subscription) => Some(subscription),
                    None => match attachment.subscribe_tasks().await {
                        Ok(subscription) => subscription,
                        Err(error) => return (None, Err(error)),
                    },
                };
                let view = attachment.observe_tasks().await;
                (subscription, view)
            }));
        }
        changed
    }
}

fn sync_activities(state: &mut UiState) {
    let Some(handle) = state.active_overlay else {
        return;
    };
    let Some(ActiveOverlay::Tasks(panel)) = state.runtime.widget(handle) else {
        return;
    };
    if panel
        .activities
        .matches(&state.activity_agents, &state.bash_views)
    {
        return;
    }
    let agents = state.activity_agents.clone();
    let commands = state.bash_views.clone();
    state.runtime.update_widget(handle, |overlay| {
        if let ActiveOverlay::Tasks(panel) = overlay {
            panel.activities.sync(agents, commands);
        }
    });
}

pub(in crate::interactive) fn toggle_tasks(state: &mut UiState) {
    state.tasks.expanded = !state.tasks.expanded;
}

pub(in crate::interactive) fn retain_tasks(state: &mut UiState) {
    if let Some(ActiveOverlay::Tasks(panel)) = state
        .active_overlay
        .and_then(|handle| state.runtime.widget(handle))
    {
        let checklist = panel.checklist.clone();
        state.tasks = checklist;
    }
}

pub(in crate::interactive) fn sync_tasks(state: &mut UiState) {
    let Some(handle) = state.active_overlay else {
        return;
    };
    if matches!(state.runtime.widget(handle), Some(ActiveOverlay::Tasks(_))) {
        let checklist = state.tasks.clone();
        state.runtime.update_widget(handle, |overlay| {
            if let ActiveOverlay::Tasks(panel) = overlay {
                panel.checklist = checklist;
            }
        });
    }
}

pub(in crate::interactive) fn close_tasks(state: &mut UiState) {
    if tasks_open(state) {
        crate::interactive::overlays::close_active(state);
    }
}

pub(in crate::interactive) fn tasks_open(state: &UiState) -> bool {
    matches!(
        state
            .active_overlay
            .and_then(|handle| state.runtime.widget(handle)),
        Some(ActiveOverlay::Tasks(_))
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::interactive) struct BashView {
    pub(in crate::interactive) owner: String,
    pub(in crate::interactive) execution: BashExecution,
}

pub(in crate::interactive) struct BashSelector {
    owner: String,
    commands: Vec<BashExecution>,
    menu: SelectMenuState,
}

impl Widget for BashSelector {
    type Action = UiAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let width = context.area.width;
        if width == 0 || context.area.height == Some(0) {
            return Vec::new();
        }
        let child = context.with_area(RenderArea {
            width: width.saturating_sub(4).min(BASH_SELECTOR_MAX_WIDTH),
            ..context.area
        });
        let mut lines = vec![StyledLine::new("Background Bash", ThemeColor::Title)];
        let start = self
            .menu
            .selected
            .saturating_sub(BASH_SELECTOR_VISIBLE_ROWS - 1);
        lines.extend(
            self.commands
                .iter()
                .enumerate()
                .skip(start)
                .take(BASH_SELECTOR_VISIBLE_ROWS)
                .map(|(index, execution)| {
                    StyledLine::segments([
                        (
                            if index == self.menu.selected {
                                "> "
                            } else {
                                "  "
                            }
                            .to_owned(),
                            ThemeColor::Accent,
                        ),
                        (single_line(&execution.command), ThemeColor::Default),
                    ])
                }),
        );
        lines.push(StyledLine::new(
            "↑↓ select · enter background · esc cancel",
            ThemeColor::Dim,
        ));
        let max_height = lines.len() + 2;
        Panel {
            title: String::new(),
            lines,
            max_height,
            border_color: ThemeColor::Dim,
        }
        .render(&child)
        .into_iter()
        .take(context.area.height.unwrap_or(usize::MAX))
        .map(|line| line.prepend_segment("   ", ThemeColor::Dim).fit(width))
        .collect()
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<UiAction> {
        match key {
            KeyInput::Up => self.menu.move_up(self.commands.len()),
            KeyInput::Down => self.menu.move_down(self.commands.len()),
            KeyInput::Enter => {
                if let Some(execution) = self.commands.get(self.menu.selected) {
                    return InputOutcome::Action(UiAction::BackgroundBash {
                        owner: self.owner.clone(),
                        id: execution.id.clone(),
                    });
                }
            }
            KeyInput::Esc | KeyInput::Ctrl('c') => {
                return InputOutcome::Action(UiAction::CloseBashSelector);
            }
            _ => {}
        }
        InputOutcome::Consumed
    }
}

impl UiState {
    pub(in crate::interactive) fn close_bash_selector(&mut self) {
        if let Some(handle) = self.bash_selector.take() {
            self.runtime.remove_widget(handle);
        }
    }

    pub(in crate::interactive) fn bash_selector_lines(
        &self,
        context: &RenderContext<'_>,
    ) -> Option<Vec<StyledLine>> {
        self.bash_selector
            .and_then(|handle| self.runtime.widget(handle))
            .map(|selector| selector.render(context))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::interactive) enum ActivitySelection {
    Agent(String),
    Bash { owner: String, id: String },
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::interactive) struct ActivityPanel {
    agents: Vec<SubagentView>,
    commands: Vec<BashView>,
    selected: usize,
}

impl ActivityPanel {
    pub(in crate::interactive) fn new(agents: Vec<SubagentView>, commands: Vec<BashView>) -> Self {
        Self {
            agents,
            commands,
            selected: 0,
        }
    }

    fn selection_at(&self, index: usize) -> Option<ActivitySelection> {
        if let Some(agent) = self.agents.get(index) {
            Some(ActivitySelection::Agent(agent.id.clone()))
        } else {
            self.commands
                .get(index.saturating_sub(self.agents.len()))
                .map(|command| ActivitySelection::Bash {
                    owner: command.owner.clone(),
                    id: command.execution.id.clone(),
                })
        }
    }

    pub(in crate::interactive) fn selection(&self) -> Option<ActivitySelection> {
        self.selection_at(self.selected)
    }

    pub(in crate::interactive) fn restore_selection(
        &mut self,
        selected: Option<&ActivitySelection>,
    ) {
        let count = self.agents.len() + self.commands.len();
        self.selected = (0..count)
            .find(|index| self.selection_at(*index).as_ref() == selected)
            .unwrap_or_else(|| self.selected.min(count.saturating_sub(1)));
    }

    pub(in crate::interactive) fn matches(
        &self,
        agents: &[SubagentView],
        commands: &[BashView],
    ) -> bool {
        self.agents == agents && self.commands == commands
    }

    pub(in crate::interactive) fn sync(
        &mut self,
        agents: Vec<SubagentView>,
        commands: Vec<BashView>,
    ) {
        let selected = self.selection();
        self.agents = agents;
        self.commands = commands;
        self.restore_selection(selected.as_ref());
    }

    pub(in crate::interactive) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        let count = self.agents.len() + self.commands.len();
        match key {
            KeyInput::Up => self.selected = self.selected.saturating_sub(1),
            KeyInput::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyInput::Enter => {
                if let Some(agent) = self.agents.get(self.selected) {
                    return UiAction::EnterSubagent(agent.id.clone());
                }
                if let Some(command) = self
                    .commands
                    .get(self.selected.saturating_sub(self.agents.len()))
                {
                    return UiAction::OpenBashOutput {
                        owner: command.owner.clone(),
                        id: command.execution.id.clone(),
                    };
                }
            }
            KeyInput::Ctrl('d') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    if agent.status == SubagentStatus::Running {
                        return UiAction::CancelSubagent(agent.id.clone());
                    }
                } else if let Some(command) = self
                    .commands
                    .get(self.selected.saturating_sub(self.agents.len()))
                    && command.execution.status == BashStatus::Running
                {
                    return UiAction::StopBash {
                        owner: command.owner.clone(),
                        id: command.execution.id.clone(),
                    };
                }
            }
            _ => {}
        }
        UiAction::None
    }

    pub(in crate::interactive) fn render_lines(
        &self,
        width: u16,
        height: usize,
    ) -> Vec<StyledLine> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let mut rows: Vec<StyledLine> = self
            .agents
            .iter()
            .map(|agent| {
                use phi_sdk::agent_manager::DelegationMode;

                StyledLine::segments([
                    (format!("[agent] {}", agent.name), ThemeColor::Default),
                    (
                        match agent.mode {
                            Some(DelegationMode::Foreground) => " · foreground",
                            Some(DelegationMode::Background) => " · background",
                            None => "",
                        }
                        .to_owned(),
                        ThemeColor::Dim,
                    ),
                    (
                        format!(
                            " · {}",
                            match agent.status {
                                SubagentStatus::Running => "running",
                                SubagentStatus::Success => "completed",
                                SubagentStatus::Failed => "failed",
                                SubagentStatus::Cancelled => "cancelled",
                                SubagentStatus::Interrupted => "interrupted",
                            }
                        ),
                        ThemeColor::Default,
                    ),
                ])
            })
            .chain(self.commands.iter().map(|command| {
                let (marker, color) = match command.execution.status {
                    BashStatus::Running => ("[-]", ThemeColor::Warning),
                    BashStatus::Completed => ("[x]", ThemeColor::Success),
                    BashStatus::Failed => ("[!]", ThemeColor::Error),
                    BashStatus::Stopped => ("[ ]", ThemeColor::Dim),
                };
                StyledLine::segments([
                    (marker.to_owned(), color),
                    (
                        format!(
                            " [bash] {} · {} · {} · {}",
                            single_line(&command.execution.command),
                            command.owner,
                            match command.execution.mode {
                                BashMode::Foreground => "foreground",
                                BashMode::Background => "background",
                            },
                            status_label(command.execution.status)
                        ),
                        ThemeColor::Default,
                    ),
                ])
            }))
            .collect();
        let budget = height;
        let start = self.selected.saturating_sub(budget.saturating_sub(1));
        rows = rows
            .into_iter()
            .enumerate()
            .skip(start)
            .take(budget)
            .map(|(index, row)| {
                row.prepend_segment(
                    if index == self.selected { "> " } else { "  " },
                    ThemeColor::Accent,
                )
            })
            .collect();
        rows.into_iter().map(|line| line.fit(width)).collect()
    }
}

#[derive(Clone)]
pub(in crate::interactive) struct BashDetails {
    pub(in crate::interactive) command: BashView,
    pub(in crate::interactive) output: String,
    offset: usize,
}

impl BashDetails {
    pub(in crate::interactive) fn new(command: BashView, output: String) -> Self {
        Self {
            command,
            output,
            offset: 0,
        }
    }

    pub(in crate::interactive) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        match key {
            KeyInput::Esc | KeyInput::Ctrl('c') | KeyInput::Ctrl('t') => {
                return UiAction::CloseActiveOverlay;
            }
            KeyInput::Up => self.offset = self.offset.saturating_sub(1),
            KeyInput::Down => self.offset = self.offset.saturating_add(1),
            KeyInput::Ctrl('b')
                if self.command.execution.status == BashStatus::Running
                    && self.command.execution.mode == BashMode::Foreground =>
            {
                return UiAction::BackgroundBash {
                    owner: self.command.owner.clone(),
                    id: self.command.execution.id.clone(),
                };
            }
            KeyInput::Ctrl('d') if self.command.execution.status == BashStatus::Running => {
                return UiAction::StopBash {
                    owner: self.command.owner.clone(),
                    id: self.command.execution.id.clone(),
                };
            }
            _ => {}
        }
        UiAction::None
    }

    pub(in crate::interactive) fn render_lines(
        &self,
        width: u16,
        height: usize,
        theme: &Theme,
    ) -> Vec<StyledLine> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let execution = &self.command.execution;
        let mut lines = vec![
            StyledLine::new(
                format!("{} · {}", status_label(execution.status), execution.command),
                ThemeColor::Title,
            ),
            StyledLine::new(execution.output_path.display().to_string(), ThemeColor::Dim),
        ];
        let output = phi_tui::text::wrap(&self.output, usize::from(width.saturating_sub(4)));
        let budget = height.saturating_sub(6);
        let offset = self.offset.min(output.len().saturating_sub(budget));
        lines.extend(
            output
                .into_iter()
                .skip(offset)
                .take(budget)
                .map(|line| StyledLine::new(line, ThemeColor::Default)),
        );
        let controls = match (execution.mode, execution.status) {
            (BashMode::Foreground, BashStatus::Running) => " · ctrl+b background · ctrl+d stop",
            (BashMode::Background, BashStatus::Running) => " · ctrl+d stop",
            _ => "",
        };
        lines.push(StyledLine::new(
            format!("latest 50 KiB · ↑↓ scroll{controls} · esc close"),
            ThemeColor::Dim,
        ));
        Panel {
            title: "Bash output".into(),
            lines,
            max_height: height,
            border_color: ThemeColor::BorderAccent,
        }
        .render(&RenderContext::width(width, theme))
        .into_iter()
        .take(height)
        .map(|line| line.fit(width))
        .collect()
    }
}

pub(in crate::interactive) fn status_label(status: BashStatus) -> &'static str {
    match status {
        BashStatus::Running => "running",
        BashStatus::Completed => "completed",
        BashStatus::Failed => "failed",
        BashStatus::Stopped => "stopped",
    }
}

async fn owner_attachment(root: &AgentAttachment, owner: &str) -> anyhow::Result<AgentAttachment> {
    if owner == root.id().as_str() {
        Ok(root.clone())
    } else {
        root.child_attachment(owner).await
    }
}

fn project_agents(
    state: &UiState,
    links: &[crate::session::SubagentChildLink],
) -> Vec<SubagentView> {
    let agent_ids: HashSet<_> = links
        .iter()
        .map(|link| link.child_agent_id.as_str())
        .collect();
    state
        .subagent_views
        .iter()
        .filter(|agent| agent_ids.contains(agent.id.as_str()))
        .cloned()
        .collect()
}

pub(in crate::interactive) async fn refresh(root: &AgentAttachment, state: &mut UiState) -> bool {
    match root.activities().await {
        Ok(activities) => refresh_views(root, state, activities).await,
        Err(error) => {
            state.notify(
                NotifyKind::Warning,
                format!("Cannot refresh Agent activities: {error}"),
            );
            true
        }
    }
}

async fn refresh_views(
    root: &AgentAttachment,
    state: &mut UiState,
    activities: AgentActivities,
) -> bool {
    let agents = project_agents(state, &activities.agents);
    let commands = activities
        .bash_executions
        .into_iter()
        .map(|(owner, execution)| BashView {
            owner: owner.to_string(),
            execution,
        })
        .collect();
    let mut changed = commands != state.bash_views || agents != state.activity_agents;
    if changed {
        state.bash_views = commands;
        state.activity_agents = agents;
        sync_activities(state);
    }
    changed |= refresh_bash_selector(root, state).await;
    let detail = state
        .active_overlay
        .and_then(|handle| match state.runtime.widget(handle) {
            Some(ActiveOverlay::BashDetails(panel)) => Some((
                handle,
                panel.command.owner.clone(),
                panel.command.execution.id.clone(),
            )),
            _ => None,
        });
    if let Some((handle, owner, id)) = detail
        && let Ok(attachment) = owner_attachment(root, &owner).await
        && let Ok(output) = attachment.read_bash_output(&id).await
    {
        let view = attachment
            .snapshot()
            .bash_executions
            .into_iter()
            .find(|execution| execution.id == id)
            .map(|execution| BashView { owner, execution });
        let detail_changed = matches!(state.runtime.widget(handle),
            Some(ActiveOverlay::BashDetails(panel)) if panel.output != output
                || view.as_ref().is_some_and(|view| panel.command != *view));
        if !detail_changed {
            return changed;
        }
        state.runtime.update_widget(handle, |overlay| {
            if let ActiveOverlay::BashDetails(panel) = overlay {
                panel.output = output;
                if let Some(view) = view {
                    panel.command = view;
                }
            }
        });
        return true;
    }
    changed
}

pub(in crate::interactive) async fn background_bash(
    root: &AgentAttachment,
    owner: &str,
    id: &str,
    state: &mut UiState,
) {
    state.close_bash_selector();
    let result = async {
        owner_attachment(root, owner)
            .await?
            .background_bash(id)
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            refresh(root, state).await;
        }
        Err(error) => state.notify(
            NotifyKind::Warning,
            format!("Cannot background Bash: {error}"),
        ),
    }
}

pub(in crate::interactive) async fn stop_bash(
    root: &AgentAttachment,
    owner: &str,
    id: &str,
    state: &mut UiState,
) {
    let result = async {
        owner_attachment(root, owner).await?.stop_bash(id).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        state.notify(NotifyKind::Warning, format!("Cannot stop Bash: {error}"));
    }
}

pub(in crate::interactive) async fn open_bash_output(
    root: &AgentAttachment,
    owner: String,
    id: &str,
    state: &mut UiState,
) {
    let result = async {
        let attachment = owner_attachment(root, &owner).await?;
        let execution = attachment
            .snapshot()
            .bash_executions
            .into_iter()
            .find(|execution| execution.id == id)
            .ok_or_else(|| anyhow::anyhow!("Bash execution is unavailable"))?;
        let output = attachment.read_bash_output(id).await?;
        Ok::<_, anyhow::Error>(BashDetails::new(BashView { owner, execution }, output))
    }
    .await;
    match result {
        Ok(panel) => crate::interactive::overlays::open_bash_output(state, panel),
        Err(error) => state.notify(
            NotifyKind::Warning,
            format!("Cannot read Bash output: {error}"),
        ),
    }
}

pub(in crate::interactive) async fn open_current_bash(
    root: &AgentAttachment,
    owner: &str,
    state: &mut UiState,
) {
    if state.interaction.is_some() {
        return;
    }
    state.close_bash_selector();
    let result = async {
        let attachment = owner_attachment(root, owner).await?;
        refresh(root, state).await;
        let commands = foreground_commands(&attachment);
        if !commands.is_empty() {
            crate::interactive::overlays::close_active(state);
            state.bash_selector = Some(state.runtime.mount_widget(
                BashSelector {
                    owner: owner.to_owned(),
                    commands,
                    menu: SelectMenuState::default(),
                },
                MountOptions::input(InputContext::custom("bash_selector")).focused(),
            ));
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        state.notify(
            NotifyKind::Warning,
            format!("Cannot open Bash activities: {error}"),
        );
    }
}

fn foreground_commands(attachment: &AgentAttachment) -> Vec<BashExecution> {
    attachment
        .snapshot()
        .bash_executions
        .into_iter()
        .filter(|execution| {
            execution.mode == BashMode::Foreground && execution.status == BashStatus::Running
        })
        .collect()
}

async fn refresh_bash_selector(root: &AgentAttachment, state: &mut UiState) -> bool {
    let Some(handle) = state.bash_selector else {
        return false;
    };
    let Some(selector) = state.runtime.widget(handle) else {
        state.close_bash_selector();
        return true;
    };
    let owner = selector.owner.clone();
    let selected = selector
        .commands
        .get(selector.menu.selected)
        .map(|execution| execution.id.clone());
    let commands = match owner_attachment(root, &owner).await {
        Ok(attachment) => foreground_commands(&attachment),
        Err(_) => Vec::new(),
    };
    let Some(index) = commands
        .iter()
        .position(|execution| Some(&execution.id) == selected.as_ref())
    else {
        state.close_bash_selector();
        return true;
    };
    if state
        .runtime
        .widget(handle)
        .is_some_and(|selector| selector.commands == commands)
    {
        return false;
    }
    state.runtime.update_widget(handle, |selector| {
        selector.commands = commands;
        selector.menu.selected = index;
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{
        WORK_PANEL_MAX_HEIGHT_PERCENT as OVERLAY_MAX_HEIGHT_PERCENT,
        WORK_PANEL_MAX_WIDTH as PANEL_MAX_WIDTH,
        WORK_PANEL_MIN_HEIGHT_PERCENT as OVERLAY_MIN_HEIGHT_PERCENT,
    };
    use phi_sdk::agent_instance::AgentInstanceId;
    use phi_tui::InputOutcome;

    fn command(id: &str, mode: BashMode) -> BashView {
        BashView {
            owner: "master".into(),
            execution: BashExecution {
                id: id.into(),
                turn_id: "turn".into(),
                tool_call_id: format!("call-{id}"),
                command: format!("command {id}"),
                mode,
                status: BashStatus::Running,
                output_path: std::path::PathBuf::from("/tmp").join(id),
                exit_code: None,
                error: None,
            },
        }
    }

    fn mount_selector(state: &mut UiState, commands: Vec<BashExecution>) {
        state.bash_selector = Some(state.runtime.mount_widget(
            BashSelector {
                owner: "master".into(),
                commands,
                menu: SelectMenuState::default(),
            },
            MountOptions::input(InputContext::custom("bash_selector")).focused(),
        ));
    }

    #[test]
    fn inline_selector_routes_selection_without_touching_drafts_or_child_view() {
        let mut state = UiState::default();
        state.editor.replace_text("retained draft");
        mount_selector(
            &mut state,
            ["first", "second"]
                .map(|id| command(id, BashMode::Foreground).execution)
                .into(),
        );
        assert!(state.active_overlay.is_none());
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Char('x')),
            InputOutcome::Consumed
        );
        state.runtime.dispatch_key(KeyInput::Down);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::BackgroundBash {
                owner: "master".into(),
                id: "second".into(),
            })
        );
        let theme = Theme::default();
        for width in 0..=100 {
            for height in 0..=12 {
                let lines = state
                    .bash_selector_lines(&RenderContext::new(
                        RenderArea {
                            width,
                            height: Some(height),
                        },
                        &theme,
                    ))
                    .unwrap();
                assert!(lines.len() <= height);
                assert!(
                    lines
                        .iter()
                        .all(|line| text::display_width(&line.text) <= usize::from(width))
                );
            }
        }
        let mut session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("child transcript");
        let mut conversation =
            crate::interactive::components::conversation::ConversationView::child_from_session(
                &session, None,
            );
        let built = crate::interactive::render::build_ui_frame(
            &mut crate::interactive::render::RenderTarget::Subagent {
                conversation: &mut conversation,
                session: &session,
                compact_enabled: None,
                terminal: &mut state,
            },
            RenderArea {
                width: 100,
                height: Some(24),
            },
            &theme,
        );
        assert!(
            built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("child transcript"))
        );
        assert!(
            built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("Background Bash"))
        );
        assert!(
            !built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("retained draft"))
        );
        for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
            assert_eq!(
                state.runtime.dispatch_key(key),
                InputOutcome::Action(UiAction::CloseBashSelector)
            );
        }
        state.close_bash_selector();
        assert!(state.bash_selector.is_none());
        assert_eq!(state.editor.text(), "retained draft");
    }

    #[test]
    fn permission_prompts_preempt_without_replacing_the_inline_selector() {
        use crate::interactive::components::permission::{ConfirmState, InteractionPrompt};
        let mut state = UiState::default();
        mount_selector(
            &mut state,
            vec![command("first", BashMode::Foreground).execution],
        );
        let handle = state.bash_selector.unwrap();
        state.set_interaction(InteractionPrompt::Permission(ConfirmState {
            title: "Permission".into(),
            message: "Run command".into(),
            choice: phi_tui::Choice::new(["once", "always", "deny"]),
            help: "enter confirm".into(),
        }));
        state.activate_interaction();
        let theme = Theme::default();
        let lines = crate::interactive::components::status::render(
            &state,
            &RenderContext::width(100, &theme),
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.text.contains("Background Bash"))
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::PermissionAllowOnce)
        );
        assert_eq!(
            state.bash_selector.map(|handle| handle.id()),
            Some(handle.id())
        );
        state.clear_interaction();
        state.runtime.focus_widget(handle);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::BackgroundBash {
                owner: "master".into(),
                id: "first".into(),
            })
        );
    }

    #[test]
    fn agent_rows_show_known_delegation_modes() {
        use phi_sdk::agent_manager::DelegationMode;

        for (mode, label) in [
            (Some(DelegationMode::Foreground), " · foreground"),
            (Some(DelegationMode::Background), " · background"),
            (None, ""),
        ] {
            for (outcome, status) in [
                (crate::session::SubagentOutcome::Running, "running"),
                (
                    crate::session::SubagentOutcome::Finished {
                        result: String::new(),
                        is_error: false,
                    },
                    "completed",
                ),
            ] {
                let link = crate::session::SubagentChildLink {
                    child_session_id: "child".into(),
                    child_agent_id: "child".into(),
                    child_working_dir: "/tmp".into(),
                    definition: Some("reviewer".into()),
                    alias: None,
                    parent_tool_call_id: None,
                    outcome,
                    admission: phi_sdk::agents::SubagentAdmission::Accepted,
                    mode,
                };
                let panel = ActivityPanel::new(vec![SubagentView::from(&link)], Vec::new());
                let row = panel.render_lines(100, 12).remove(0);
                assert_eq!(
                    row.text.trim_end(),
                    format!("> [agent] reviewer{label} · {status}")
                );
                if mode.is_some() {
                    assert!(row.segments.iter().any(|segment| {
                        segment.text() == label && segment.color() == ThemeColor::Dim
                    }));
                }
            }
        }
    }

    #[test]
    fn bash_rows_use_checklist_status_markers() {
        for (status, marker, color) in [
            (BashStatus::Running, "[-]", ThemeColor::Warning),
            (BashStatus::Completed, "[x]", ThemeColor::Success),
            (BashStatus::Failed, "[!]", ThemeColor::Error),
            (BashStatus::Stopped, "[ ]", ThemeColor::Dim),
        ] {
            let mut bash = command("status", BashMode::Background);
            bash.execution.status = status;
            let panel = ActivityPanel::new(Vec::new(), vec![bash]);
            let lines = panel.render_lines(80, 12);
            let row = lines
                .iter()
                .find(|line| line.text.contains("[bash]"))
                .unwrap();
            assert!(row.text.contains(&format!("{marker} [bash]")));
            assert!(
                row.segments
                    .iter()
                    .any(|segment| segment.text() == marker && segment.color() == color)
            );
        }
    }

    #[test]
    fn bash_detail_controls_follow_execution_state() {
        let theme = Theme::default();
        for mode in [BashMode::Foreground, BashMode::Background] {
            for status in [
                BashStatus::Running,
                BashStatus::Completed,
                BashStatus::Failed,
                BashStatus::Stopped,
            ] {
                let mut bash = command("controls", mode);
                bash.execution.status = status;
                let mut details = BashDetails::new(bash, String::new());
                let running = status == BashStatus::Running;
                let can_background = running && mode == BashMode::Foreground;
                assert_eq!(
                    details.handle_key(KeyInput::Ctrl('b')),
                    if can_background {
                        UiAction::BackgroundBash {
                            owner: "master".into(),
                            id: "controls".into(),
                        }
                    } else {
                        UiAction::None
                    }
                );
                assert_eq!(
                    details.handle_key(KeyInput::Ctrl('d')),
                    if running {
                        UiAction::StopBash {
                            owner: "master".into(),
                            id: "controls".into(),
                        }
                    } else {
                        UiAction::None
                    }
                );
                let lines = details.render_lines(120, 12, &theme);
                assert_eq!(
                    lines.iter().any(|line| line.text.contains("ctrl+b")),
                    can_background
                );
                assert_eq!(
                    lines.iter().any(|line| line.text.contains("ctrl+d")),
                    running
                );
            }
        }
    }

    #[test]
    fn command_selection_survives_reordering() {
        let commands = vec![
            command("first", BashMode::Background),
            command("second", BashMode::Background),
        ];
        let mut panel = ActivityPanel::new(Vec::new(), commands.clone());
        panel.handle_key(KeyInput::Down);
        panel.sync(Vec::new(), vec![commands[1].clone(), commands[0].clone()]);
        assert_eq!(
            panel.handle_key(KeyInput::Enter),
            UiAction::OpenBashOutput {
                owner: "master".into(),
                id: "second".into()
            }
        );
    }

    #[test]
    fn mixed_activity_rows_route_type_specific_details_and_stop() {
        let agent = SubagentView {
            id: "child".into(),
            name: "reviewer".into(),
            alias: None,
            mode: None,
            status: SubagentStatus::Running,
        };
        let mut panel = ActivityPanel::new(
            vec![agent.clone()],
            vec![command("bash", BashMode::Background)],
        );
        assert_eq!(panel.handle_key(KeyInput::Ctrl('b')), UiAction::None);
        assert_eq!(
            panel.handle_key(KeyInput::Enter),
            UiAction::EnterSubagent("child".into())
        );
        assert_eq!(
            panel.handle_key(KeyInput::Ctrl('d')),
            UiAction::CancelSubagent("child".into())
        );
        panel.handle_key(KeyInput::Down);
        assert_eq!(
            panel.handle_key(KeyInput::Enter),
            UiAction::OpenBashOutput {
                owner: "master".into(),
                id: "bash".into()
            }
        );
        assert_eq!(
            panel.handle_key(KeyInput::Ctrl('d')),
            UiAction::StopBash {
                owner: "master".into(),
                id: "bash".into()
            }
        );
        let mut finished = command("bash", BashMode::Background);
        finished.execution.status = BashStatus::Completed;
        panel.sync(Vec::new(), vec![finished]);
        assert_eq!(panel.handle_key(KeyInput::Ctrl('b')), UiAction::None);
        assert_eq!(panel.handle_key(KeyInput::Ctrl('d')), UiAction::None);
        let mut finished_agent = agent;
        finished_agent.status = SubagentStatus::Success;
        panel.sync(vec![finished_agent], Vec::new());
        assert_eq!(panel.handle_key(KeyInput::Ctrl('d')), UiAction::None);
        panel.sync(Vec::new(), Vec::new());
        assert_eq!(panel.handle_key(KeyInput::Enter), UiAction::None);
        panel.sync(Vec::new(), vec![command("bash", BashMode::Background)]);
        let theme = Theme::default();
        let details = BashDetails::new(
            command("bash", BashMode::Background),
            "output 界\n".repeat(20),
        );
        for width in 0..=100 {
            for height in 0..=24 {
                for lines in [
                    panel.render_lines(width, height),
                    details.render_lines(width, height, &theme),
                ] {
                    assert!(lines.len() <= height);
                    assert!(
                        lines
                            .iter()
                            .all(|line| phi_tui::text::display_width(&line.text)
                                <= usize::from(width))
                    );
                }
            }
        }
    }

    #[test]
    fn activity_filter_preserves_projected_status() {
        let links = [crate::session::SubagentChildLink {
            child_session_id: "ordinary".into(),
            child_agent_id: "ordinary".into(),
            child_working_dir: "/tmp".into(),
            definition: Some("probe".into()),
            alias: None,
            parent_tool_call_id: None,
            outcome: crate::session::SubagentOutcome::Interrupted,
            admission: phi_sdk::agents::SubagentAdmission::Accepted,
            mode: None,
        }];
        for status in [
            SubagentStatus::Running,
            SubagentStatus::Success,
            SubagentStatus::Failed,
            SubagentStatus::Cancelled,
            SubagentStatus::Interrupted,
        ] {
            let mut state = UiState::default();
            state.subagent_views = ["ordinary", "workflow-worker"]
                .map(|id| SubagentView {
                    id: id.into(),
                    name: "probe".into(),
                    alias: None,
                    mode: None,
                    status,
                })
                .into();
            assert_eq!(
                project_agents(&state, &links),
                [state.subagent_views[0].clone()]
            );
        }
    }

    #[tokio::test]
    async fn activities_respect_agent_execution_boundaries() {
        use std::sync::Arc;
        use std::time::Duration;

        use clap::Parser;
        use phi_sdk::lifecycle::SessionEndReason;
        use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const HTTP_OK: &str = "HTTP/1.1 200 OK";
        const BACKGROUND_COMMAND: &str = "sleep 60";
        const BACKGROUND_CHILD_PROMPT: &str = "ordinary-job";

        tokio::time::timeout(Duration::from_secs(30), async {
            let root = std::path::PathBuf::from("/tmp")
                .join(format!("phi-activity-scope-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
            std::fs::write(root.join(".phi/agents/probe.md"), "---\nname: probe\ndescription: Probe\n---\nFollow the request.").unwrap();
            std::fs::write(root.join(".phi/settings.json"), r#"{"defaultProvider":"local","defaultModel":"test","permission":{"mode":"allow"},"extensions":["!*.lua"],"memory":{"enabled":false}}"#).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let models = root.join("models.json");
            std::fs::write(&models, serde_json::json!({"providers":{"local":{
                "api":"openai-completions", "baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
                "models":{"test":{"contextWindow":128000}}
            }}}).to_string()).unwrap();
            let provider = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (mut connection, _) = listener.accept().await.unwrap();
                    connections.spawn(async move {
                        let mut bytes = Vec::new();
                        let request = loop {
                            let mut buffer = [0; 4096];
                            assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
                            bytes.extend_from_slice(&buffer);
                            if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                                let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                                if bytes.len() >= start + 4 + length {
                                    break serde_json::from_slice::<serde_json::Value>(&bytes[start + 4..start + 4 + length]).unwrap();
                                }
                            }
                        };
                        if request["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool") {
                            std::future::pending::<()>().await;
                        }
                        let mut tool_calls = vec![serde_json::json!({"index":0,"id":"background-bash","type":"function",
                            "function":{"name":"Bash","arguments":serde_json::json!({"command":BACKGROUND_COMMAND,"run_in_background":true}).to_string()}})];
                        if request.to_string().contains(BACKGROUND_CHILD_PROMPT) {
                            tool_calls.push(serde_json::json!({"index":1,"id":"foreground-bash","type":"function",
                                "function":{"name":"Bash","arguments":serde_json::json!({"command":BACKGROUND_COMMAND}).to_string()}}));
                        }
                        if request.to_string().contains("root-job") || request.to_string().contains("foreground-job") {
                            for (index, command, mode) in [
                                (1, "printf completed", BashMode::Foreground),
                                (2, "exit 7", BashMode::Foreground),
                                (3, "printf background-completed", BashMode::Background),
                                (4, "exit 8", BashMode::Background),
                            ] {
                                tool_calls.push(serde_json::json!({"index":index,"id":format!("terminal-bash-{index}"),"type":"function",
                                    "function":{"name":"Bash","arguments":serde_json::json!({"command":command,"run_in_background":mode == BashMode::Background}).to_string()}}));
                            }
                        }
                        let reply = serde_json::json!({"id":"completion","model":"test","choices":[{
                            "delta":{"tool_calls":tool_calls},
                            "finish_reason":"tool_calls"
                        }]});
                        let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
                        connection.write_all(format!("{HTTP_OK}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    });
                }
            });
            let catalog = Arc::new(phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("catalog.json"), user_paths: vec![models], offline: true,
            }).unwrap());
            let permission = phi_sdk::permission::PermissionConfig::try_from_value(serde_json::json!({"mode":"allow"})).unwrap();
            let resources = crate::runtime::RuntimeResources::new_for(
                phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()).permission_policy(permission, Some(root.clone())),
                Arc::new(phi_sdk::extension::ExtensionRuntime::load_with_paths(Vec::new())), catalog.clone(),
                phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(), &root,
            );
            let runtime = crate::runtime::TestRuntime::new(crate::runtime::AgentRuntimeFactory::new(resources));
            let cli = Arc::new(crate::cli::Cli::parse_from(["phi"]));
            let mut configuration = crate::config::Settings::default();
            configuration.default_provider = Some("local".into());
            configuration.default_model = Some("test".into());
            configuration.permission = Some(serde_json::json!({"mode":"allow"}));
            configuration.memory = Some(phi_sdk::extension::memory::MemoryConfig { enabled: false, ..Default::default() });
            let policy = crate::runtime::preparation::tests::TestConfiguration::new(
                cli.clone(), configuration, crate::AgentBuildOptions::default(), Vec::new(), catalog.clone(),
            );
            policy.install(&runtime.preparation);
            runtime.preparation.enable_subagents(&runtime.sessions);
            let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let session_id = session.id.clone();
            let sessions = crate::runtime::preparation::SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
            let binding = sessions.start_session(session, true).await.unwrap();
            let attachment = AgentAttachment::new(&binding, runtime.sessions.session_access(binding.instance().id()), runtime.preparation.clone());
            let run = binding.launch_workflow(runtime.workflows.clone(), WorkflowRequest::parse(
                "export const meta = {name: 'activity-scope', description: 'worker Bash isolation'}; return await agent('workflow-job', {role: 'probe'});", None,
            ).await.unwrap(), None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.unwrap();
            let ordinary = binding.instance().spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new("probe", BACKGROUND_CHILD_PROMPT)).await.unwrap();
            let foreground = binding.instance().spawn_subagent(
                phi_sdk::agent_manager::AgentSpawnRequest::new("probe", "foreground-job")
                    .mode(phi_sdk::agent_manager::DelegationMode::Foreground),
            ).await.unwrap();
            attachment.submit(phi_sdk::turn::TurnPrompt {
                prompt: "root-job".into(), source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                images: Vec::new(), injected_context: Vec::new(), tool_policy: Default::default(),
            }).await.unwrap();
            let command_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let worker = loop {
                let snapshot = run.snapshot();
                assert!(tokio::time::Instant::now() < command_deadline,
                    "Bash startup stalled: root {:?}, workflow {:?}, ordinary {:?}",
                    attachment.snapshot(), snapshot, runtime.manager.get(ordinary.instance_id()).map(|instance| instance.snapshot()));
                if let Some(worker) = snapshot.invocations().first().and_then(|item| item.worker())
                    && worker.registration().is_some()
                    && [attachment.id(), ordinary.instance_id(), foreground.instance_id(), worker.instance_id()].iter().all(|id| {
                        runtime.manager.get(id).is_ok_and(|instance| instance.snapshot().bash_executions.iter().any(|execution| {
                            execution.mode == BashMode::Background && execution.status == BashStatus::Running
                        }))
                    })
                    && runtime.manager.get(ordinary.instance_id()).unwrap().snapshot().bash_executions.len() == 2
                    && [attachment.id(), foreground.instance_id()].iter().all(|id| {
                        let executions = runtime.manager.get(id).unwrap().snapshot().bash_executions;
                        [BashMode::Foreground, BashMode::Background].iter().all(|mode| {
                            [BashStatus::Completed, BashStatus::Failed].iter().all(|status| executions.iter().any(|execution| execution.mode == *mode && execution.status == *status))
                        })
                    }) {
                    break worker.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let mut state = UiState::default();
            state.subagent_views = attachment.subagents().await.unwrap().iter().map(|child| SubagentView {
                id: child.child_agent_id.clone(), name: child.display_name(), alias: child.alias.clone(), mode: child.mode, status: SubagentStatus::Running,
            }).collect();
            refresh(&attachment, &mut state).await;
            let mut agent_ids: Vec<_> = state.activity_agents.iter().map(|agent| agent.id.as_str()).collect();
            agent_ids.sort();
            let mut expected_agents = vec![ordinary.instance_id().as_str(), foreground.instance_id().as_str()];
            expected_agents.sort();
            assert_eq!(agent_ids, expected_agents);
            let mut owners: Vec<_> = state.bash_views.iter().map(|view| view.owner.clone()).collect();
            owners.sort();
            let mut expected: Vec<_> = [attachment.id(), foreground.instance_id()].into_iter()
                .flat_map(|id| std::iter::repeat_n(id.to_string(), 3)).collect();
            expected.sort();
            let mut observed_owners: Vec<_> = attachment.activities().await.unwrap().bash_executions.into_iter().map(|(owner, _)| owner.to_string()).collect();
            observed_owners.sort();
            let worker_commands = runtime.manager.get(worker.instance_id()).unwrap().snapshot().bash_executions;
            let child_attachment = attachment.child_attachment(ordinary.instance_id().as_str()).await.unwrap();
            let child_commands = child_attachment.activities().await.unwrap().bash_executions;
            let child_owned_commands = child_attachment.snapshot().bash_executions;
            let background_link = attachment.activities().await.unwrap().agents.into_iter()
                .find(|agent| agent.child_agent_id == ordinary.instance_id().as_str()).unwrap();
            let history = attachment.subagent_view(worker.instance_id().as_str()).await.unwrap();
            let history = serde_json::to_string(&history.session.transcript).unwrap();
            let mut task_refresh = TaskRefresh::default();
            task_refresh.poll(&attachment, &mut state, TaskVisibility::Hidden).await;
            state.subagent_views.iter_mut().find(|agent| agent.id == ordinary.instance_id().as_str()).unwrap().status = SubagentStatus::Failed;
            assert!(task_refresh.poll(&attachment, &mut state, TaskVisibility::Hidden).await);
            assert_eq!(state.activity_agents.iter().find(|agent| agent.id == ordinary.instance_id().as_str()).unwrap().status, SubagentStatus::Failed);
            let command = state.bash_views.iter().find(|command| command.owner == attachment.id().as_str()).unwrap().clone();
            let output = attachment.read_bash_output(&command.execution.id).await.unwrap();
            crate::interactive::overlays::open_bash_output(&mut state, BashDetails::new(command, output));
            assert!(!refresh(&attachment, &mut state).await);
            crate::interactive::overlays::close_active(&mut state);
            open_current_bash(&attachment, ordinary.instance_id().as_str(), &mut state).await;
            let scoped_selector = state.active_overlay.is_none() && state.bash_selector.is_some();
            let scoped_handoff = state.runtime.dispatch_key(KeyInput::Enter);
            let foreground_command = child_owned_commands.iter().find(|execution| execution.mode == BashMode::Foreground).unwrap();
            background_bash(&attachment, ordinary.instance_id().as_str(), &foreground_command.id, &mut state).await;
            let scoped_backgrounded = state.bash_selector.is_none()
                && child_attachment.snapshot().bash_executions.iter().any(|execution|
                    execution.id == foreground_command.id && execution.mode == BashMode::Background);
            crate::interactive::overlays::close_active(&mut state);
            state.subagent_views.clear();
            refresh(&attachment, &mut state).await;
            let mut without_ui_children: Vec<_> = state.bash_views.iter().map(|view| view.owner.clone()).collect();
            without_ui_children.sort();
            let master_command = attachment.snapshot().bash_executions.into_iter().find(|execution| execution.status == BashStatus::Running).unwrap();
            open_bash_output(&attachment, attachment.id().to_string(), &master_command.id, &mut state).await;
            attachment.stop_bash(&master_command.id).await.unwrap();
            let foreground_attachment = attachment.child_attachment(foreground.instance_id().as_str()).await.unwrap();
            let foreground_execution = foreground_attachment.snapshot().bash_executions.into_iter().find(|execution| execution.status == BashStatus::Running).unwrap();
            foreground_attachment.stop_bash(&foreground_execution.id).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while [attachment.clone(), foreground_attachment.clone()].iter().any(|owner| {
                    owner.snapshot().bash_executions.iter().any(|execution| execution.status == BashStatus::Running)
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            task_refresh.poll(&attachment, &mut state, TaskVisibility::Hidden).await;
            let ended_commands = attachment.activities().await.unwrap().bash_executions;
            let ended_views = state.bash_views.clone();
            let stopped_details = state.active_overlay.is_some_and(|handle| {
                matches!(state.runtime.widget(handle), Some(ActiveOverlay::BashDetails(panel))
                    if panel.command.execution.id == master_command.id && panel.command.execution.status == BashStatus::Stopped)
            });
            crate::interactive::overlays::close_active(&mut state);
            run.stop().await.unwrap();
            let outgoing = sessions.new_session(binding.instance().id()).await.unwrap();
            let resumed = sessions.resume_session(outgoing.attachment.id(), crate::runtime::SavedSessionLocator::Id(&session_id)).await.unwrap();
            let mut restored_state = UiState::default();
            let mut projection = crate::interactive::agent::SubagentProjection::default();
            projection.poll(&resumed.attachment, &mut restored_state).await.unwrap();
            refresh(&resumed.attachment, &mut restored_state).await;
            assert!(restored_state.activity_agents.is_empty());
            assert_eq!(resumed.attachment.subagent_view(ordinary.instance_id().as_str()).await.unwrap().session.id.as_str(), ordinary.instance_id().as_str());
            assert!(restored_state.bash_views.is_empty());
            assert!(resumed.attachment.activities().await.unwrap().bash_executions.is_empty());
            assert!(resumed.attachment.workflow_views(resumed.attachment.registration_id().unwrap().unwrap()).unwrap().is_empty());
            runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
            provider.abort();
            let _ = provider.await;
            assert!(runtime.manager.discover().unwrap().is_empty());
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(owners, expected);
            assert_eq!(observed_owners, expected);
            assert_eq!(without_ui_children, expected);
            assert_eq!(ended_commands.len(), expected.len());
            assert_eq!(ended_views.len(), expected.len());
            for owner in [attachment.id(), foreground.instance_id()] {
                for status in [BashStatus::Completed, BashStatus::Failed, BashStatus::Stopped] {
                    assert!(ended_commands.iter().any(|(id, execution)| id == owner && execution.mode == BashMode::Background && execution.status == status));
                    assert!(ended_views.iter().any(|command| command.owner == owner.as_str() && command.execution.mode == BashMode::Background && command.execution.status == status));
                }
            }
            assert!(stopped_details);
            assert_eq!(background_link.mode, Some(phi_sdk::agent_manager::DelegationMode::Background));
            assert_eq!(child_commands.len(), 2);
            assert_eq!(child_owned_commands.len(), 2);
            assert!(child_commands.iter().all(|(owner, execution)| owner == ordinary.instance_id() && execution.status == BashStatus::Running));
            assert!(scoped_selector);
            assert!(scoped_backgrounded);
            assert_eq!(scoped_handoff, phi_tui::InputOutcome::Action(UiAction::BackgroundBash {
                owner: ordinary.instance_id().to_string(), id: foreground_command.id.clone(),
            }));
            assert!(worker_commands.iter().any(|execution| execution.mode == BashMode::Background && execution.status == BashStatus::Running));
            assert!(history.contains(BACKGROUND_COMMAND));
        }).await.expect("activity Bash scope timed out");
    }

    fn list() -> TaskListView {
        TaskListView {
            list_id: "shared".into(),
            tasks: [
                TaskStatusView::Pending,
                TaskStatusView::InProgress,
                TaskStatusView::Completed,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, status)| TaskItemView {
                id: (index + 1).to_string(),
                subject: "Review authentication 安全".into(),
                description: "Acceptance criteria\nInspect permissions and report findings.\n"
                    .repeat(20),
                active_form: Some("Reviewing authentication".into()),
                status,
                owner: (index != 0).then(|| AgentInstanceId::new("captured-instance")),
                blocked_by: if index == 0 { vec!["2".into()] } else { vec![] },
                unresolved: if index == 0 { vec!["2".into()] } else { vec![] },
                blocks: vec![],
                eligible: index == 1,
            })
            .collect(),
        }
    }

    #[test]
    fn markers_selection_and_geometry_are_independent() {
        let mut panel = TaskPanel::default();
        panel.replace(Some(list()));
        panel
            .owners
            .insert("captured-instance".into(), "security".into());
        for (index, marker, color) in [
            (0, "[ ]", ThemeColor::Default),
            (1, "[-]", ThemeColor::Warning),
            (2, "[x]", ThemeColor::Success),
        ] {
            panel.selected = index;
            let row = panel.task_row(&panel.items()[index], TaskRowMode::Selected, 80);
            assert!(row.text.starts_with(&format!("> {marker} ")));
            let expected_suffix = if index == 0 {
                "安全   —"
            } else if index == 1 {
                "authentication   security"
            } else {
                "安全   security"
            };
            assert!(row.text.trim_end().ends_with(expected_suffix));
            assert_eq!(row.bg, Some(ThemeColor::SelectedBg));
            assert!(
                row.segments
                    .iter()
                    .any(|segment| segment.text() == marker && segment.color() == color)
            );
            assert!(
                row.segments
                    .iter()
                    .filter(|segment| segment.text() != marker)
                    .all(|segment| !matches!(
                        segment.color(),
                        ThemeColor::Warning | ThemeColor::Success | ThemeColor::Error
                    ))
            );
        }
        panel.selected = 1;
        assert!(
            panel
                .render_lines(80, 24)
                .iter()
                .any(|line| line.bg == Some(ThemeColor::SelectedBg) && line.text.contains("> [-]"))
        );
        let collapsed = panel.progress_lines(100);
        assert_eq!(collapsed.len(), 2);
        assert!(collapsed[1].text.starts_with("Tasks ·"));
        panel.expanded = true;
        let progress = panel.progress_lines(100);
        assert!(
            progress
                .iter()
                .all(|line| line.bg.is_none() && !line.text.starts_with('>'))
        );
        assert!(
            progress
                .iter()
                .any(|line| line.text.contains("Reviewing authentication"))
        );
        assert!(
            progress
                .iter()
                .any(|line| line.text.contains("blocked by #2"))
        );
        assert!(progress[1].text.contains("1/3 completed"));
        let active = &progress[2];
        assert!(active.text.starts_with("  [-] #2 "));
        assert!(
            active.segments.iter().any(|segment| {
                segment.text() == "[-]" && segment.color() == ThemeColor::Warning
            })
        );
        assert!(
            active
                .segments
                .iter()
                .filter(|segment| { segment.text() != "[-]" && !segment.text().trim().is_empty() })
                .all(|segment| segment.color() == ThemeColor::Dim)
        );
        let unowned = &progress[3];
        assert!(unowned.text.starts_with("  [ ] #1 "));
        assert!(unowned.text.trim_end().ends_with("安全   —"));

        let mut view = list();
        for task in &mut view.tasks {
            task.status = TaskStatusView::Completed;
            task.unresolved.clear();
        }
        panel.replace(Some(view));
        let lines = panel.progress_lines(100);
        assert!(lines[1].text.contains("3/3 completed"));
        assert_eq!(lines.len(), 2 + MAX_PROGRESS_ROWS);
        for (index, line) in lines[2..].iter().enumerate() {
            assert!(line.text.starts_with(&format!("  [x] #{} ", index + 1)));
        }
    }

    #[test]
    fn tabs_render_only_the_selected_panel() {
        let mut state = UiState::default();
        state.tasks.replace(Some(list()));
        let mut overlay = TasksOverlay::new(&state);
        let lines = overlay.render_lines(80, 24);
        assert!(lines[1].text.contains("[checklist]"));
        assert!(lines.iter().any(|line| line.text.contains("in progress")));
        assert!(
            !lines
                .iter()
                .any(|line| line.text.contains("No agents or commands."))
        );
        overlay.handle_key(KeyInput::Tab);
        let lines = overlay.render_lines(80, 24);
        assert!(lines[1].text.contains("[agents & commands]"));
        assert!(
            !lines
                .iter()
                .any(|line| line.text.contains("No agents or commands."))
        );
        assert!(!lines.iter().any(|line| line.text.contains("in progress")));
        overlay.handle_key(KeyInput::Tab);
        let lines = overlay.render_lines(80, 24);
        assert!(lines[1].text.contains("[checklist]"));
        assert!(!lines[1].text.contains("workflow"));
        overlay.handle_key(KeyInput::Tab);
        overlay.handle_key(KeyInput::Right);
        assert!(overlay.render_lines(80, 24)[1].text.contains("[checklist]"));
        overlay.handle_key(KeyInput::Left);
        assert!(
            overlay.render_lines(80, 24)[1]
                .text
                .contains("[agents & commands]")
        );
        overlay.handle_key(KeyInput::Right);
        for key in [KeyInput::Tab, KeyInput::Tab] {
            overlay.handle_key(key);
            let lines = overlay.render_lines(80, 24);
            assert!(lines[0].text.starts_with('╭'));
            assert!(lines[2].text.contains("switch tab"));
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.text.contains("switch tab"))
                    .count(),
                1
            );
            assert!(lines[3].text.starts_with('├'));
            assert!(lines.last().unwrap().text.starts_with('╰'));
            for index in [0, 2, 3, lines.len() - 1] {
                assert_eq!(lines[index].fg, ThemeColor::Dim);
            }
            assert_eq!(
                lines.iter().filter(|line| line.text.contains('╭')).count(),
                1
            );
            assert_eq!(
                lines.iter().filter(|line| line.text.contains('╰')).count(),
                1
            );
            assert!(
                lines
                    .iter()
                    .all(|line| text::display_width(&line.text) == 80)
            );
        }
        for width in 0..=100 {
            for height in 0..=28 {
                let lines = overlay.render_lines(width, height);
                assert!(lines.len() <= height);
                assert!(
                    lines
                        .iter()
                        .all(|line| text::display_width(&line.text) <= usize::from(width))
                );
            }
        }
    }

    #[test]
    fn managed_commands_preserve_selection_and_hints() {
        let mut state = UiState::default();
        state.tasks.replace(Some(list()));
        state.bash_views = ["first", "second"]
            .into_iter()
            .map(|id| command(id, BashMode::Background))
            .collect();
        let selection = ActivitySelection::Bash {
            owner: "master".into(),
            id: "second".into(),
        };
        state.activity_selection = Some(selection.clone());
        crate::interactive::overlays::open_tasks(&mut state);
        state.runtime.dispatch_key(KeyInput::Tab);
        let handle = state.active_overlay.unwrap();
        let theme = Theme::default();
        let context = RenderContext::width(120, &theme);
        let lines = state.runtime.widget(handle).unwrap().render(&context);
        assert!(lines[1].text.contains("[agents & commands]"));
        assert!(lines[2].text.contains(
            "tab / ← → switch tab · ↑↓ select · enter details · ctrl+d stop · esc close"
        ));
        assert!(lines.iter().any(|line| line.text.contains("background")));
        state.bash_views[1].execution.status = BashStatus::Completed;
        super::sync_activities(&mut state);
        assert_eq!(
            state.active_overlay.map(|handle| handle.id()),
            Some(handle.id())
        );
        let ActiveOverlay::Tasks(panel) = state.runtime.widget(handle).unwrap() else {
            panic!("expected tasks overlay");
        };
        assert!(panel.activities.selection() == Some(selection));
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('b')),
            InputOutcome::Action(UiAction::None)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Up),
            InputOutcome::Action(UiAction::None)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('d')),
            InputOutcome::Action(UiAction::StopBash {
                owner: "master".into(),
                id: "first".into(),
            })
        );
    }

    #[test]
    fn reopen_preserves_selection() {
        let mut state = UiState::default();
        state.bash_views = ["first", "second"]
            .map(|id| command(id, BashMode::Background))
            .into();
        crate::interactive::overlays::open_tasks(&mut state);
        state.runtime.dispatch_key(KeyInput::Down);
        crate::interactive::overlays::open_tasks(&mut state);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::OpenBashOutput {
                owner: "master".into(),
                id: "second".into(),
            })
        );
    }

    #[test]
    fn both_tabs_handle_dismissal() {
        let mut state = UiState::default();
        state.tasks.replace(Some(list()));
        let mut overlay = TasksOverlay::new(&state);
        for _ in 0..2 {
            for key in [KeyInput::Esc, KeyInput::Ctrl('c'), KeyInput::Ctrl('t')] {
                assert_eq!(overlay.handle_key(key), UiAction::CloseActiveOverlay);
            }
            overlay.handle_key(KeyInput::Tab);
        }
    }

    #[test]
    fn mounted_panel_geometry_respects_terminal_size() {
        for tasks in [None, Some(list())] {
            let mut state = UiState::default();
            state.tasks.replace(tasks);
            crate::interactive::overlays::open_tasks(&mut state);
            for _ in 0..2 {
                for width in [40, 80, 160, 240] {
                    for height in [10, 24, 60, 120] {
                        let composed = state.runtime.compose(
                            phi_tui::RenderArea {
                                width,
                                height: Some(height),
                            },
                            &phi_tui::Frame::default(),
                            &Theme::default(),
                        );
                        let lines = &composed.overlays[0].lines;
                        assert!(lines.len() >= height * OVERLAY_MIN_HEIGHT_PERCENT / 100);
                        assert!(
                            lines.len() <= height * usize::from(OVERLAY_MAX_HEIGHT_PERCENT) / 100
                        );
                        assert!(lines.last().unwrap().text.starts_with('╰'));
                        assert!(lines.iter().all(|line| {
                            text::display_width(&line.text)
                                <= usize::from(width.min(PANEL_MAX_WIDTH))
                        }));
                    }
                }
                state.runtime.dispatch_key(KeyInput::Tab);
            }
        }
    }

    #[test]
    fn overlay_refresh_and_navigation_preserve_identity_and_draft() {
        let mut state = UiState::default();
        state.editor.replace_text("retained editor draft");
        state.tasks.replace(Some(list()));
        crate::interactive::overlays::open_tasks(&mut state);
        assert!(matches!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::None)
        ));
        let theme = Theme::default();
        let handle = state.active_overlay.unwrap();
        assert!(
            state
                .runtime
                .widget(handle)
                .unwrap()
                .render(&RenderContext::width(80, &theme))
                .iter()
                .any(|line| line.text.contains("no assigned agent"))
        );
        state.runtime.dispatch_key(KeyInput::Down);
        assert!(
            matches!(state.runtime.dispatch_key(KeyInput::Enter), InputOutcome::Action(UiAction::OpenTaskOwner(id)) if id == "captured-instance")
        );
        assert!(matches!(
            state.runtime.dispatch_key(KeyInput::Ctrl('d')),
            InputOutcome::Action(UiAction::None)
        ));
        super::retain_tasks(&mut state);
        let mut updated = list();
        updated.tasks.remove(0);
        assert!(state.tasks.replace(Some(updated.clone())));
        assert_eq!(state.tasks.items()[state.tasks.selected].id, "2");
        state.tasks.notice = Some("History access denied".into());
        super::sync_tasks(&mut state);
        crate::interactive::overlays::close_active(&mut state);
        crate::interactive::overlays::open_tasks(&mut state);
        super::retain_tasks(&mut state);
        assert_eq!(state.tasks.items()[state.tasks.selected].id, "2");
        assert_eq!(state.tasks.notice.as_deref(), Some("History access denied"));
        assert_eq!(state.editor.text(), "retained editor draft");
        assert!(!state.tasks.replace(Some(updated)));
    }
}
