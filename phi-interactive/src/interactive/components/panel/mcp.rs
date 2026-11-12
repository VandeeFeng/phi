//! MCP panel, event projection and elicitation interaction for the terminal attachment.
//! SDK owns connections and tool authority; drafts and notices are transient UI state.

use std::collections::{HashMap, HashSet};

use phi_sdk::extension::mcp::McpElicitationRequest;
use phi_sdk::tools::ToolExposure;
use phi_tui::{
    Choice, ChoiceAction, EditorState, InputOutcome, KeyInput, NotifyKind, Panel, RenderArea,
    RenderContext, SelectMenuState, StyledLine, ThemeColor, Widget,
    text::{display_width, truncate as truncate_to_width},
};
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::task::JoinHandle;

use crate::interactive::components::input::UiAction;
use crate::interactive::{overlays, state::UiState};
use crate::runtime::McpReconnectResult;
use crate::runtime::attachment::AgentAttachment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpPanelTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) exposure: ToolExposure,
    pub(crate) was_exposure: ToolExposure,
    pub(crate) tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpPanelServer {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) connected: bool,
    pub(crate) cached: bool,
    pub(crate) expanded: bool,
    pub(crate) notice: Option<String>,
    pub(crate) tools: Vec<McpPanelTool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct McpPanelState {
    pub(crate) servers: Vec<McpPanelServer>,
    pub(crate) cursor: usize,
    pub(crate) dirty: bool,
    pub(crate) confirming_discard: bool,
    pub(crate) discard_selected: usize,
    pub(crate) auth_notice: Option<String>,
    pub(crate) read_only: bool,
}

impl McpPanelState {
    pub(crate) fn refresh_servers(&mut self, mut servers: Vec<McpPanelServer>) {
        for server in &mut servers {
            let Some(previous) = self.servers.iter().find(|entry| entry.name == server.name) else {
                continue;
            };
            server.expanded = previous.expanded;
            server.notice = previous.notice.clone();
            if self.dirty {
                for tool in &mut server.tools {
                    if let Some(old) = previous.tools.iter().find(|item| item.name == tool.name) {
                        tool.exposure = old.exposure;
                        tool.was_exposure = old.was_exposure;
                    }
                }
            }
        }
        self.servers = servers;
        let count = self.servers.len()
            + self
                .servers
                .iter()
                .filter(|server| server.expanded)
                .map(|server| server.tools.len())
                .sum::<usize>();
        self.cursor = self.cursor.min(count.saturating_sub(1));
    }

    pub(crate) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        let visible = visible_items(self);
        clear_notices(self);
        if self.confirming_discard {
            return handle_discard_key(self, key);
        }
        if matches!(key, KeyInput::Esc)
            && let Some((index, None)) = visible.get(self.cursor).copied()
            && self.servers[index].status == "connecting"
        {
            let server = &mut self.servers[index];
            server.status = String::from("failed");
            server.connected = false;
            server.notice = Some(String::from("(cancelled)"));
            return UiAction::Pick(format!("mcp:cancel:{}", server.name));
        }
        if self.read_only {
            match key {
                KeyInput::Up => self.cursor = self.cursor.saturating_sub(1),
                KeyInput::Down => {
                    self.cursor = (self.cursor + 1).min(visible.len().saturating_sub(1))
                }
                KeyInput::Enter => {
                    if let Some((index, None)) = visible.get(self.cursor) {
                        self.servers[*index].expanded = !self.servers[*index].expanded;
                    }
                }
                KeyInput::Esc | KeyInput::Ctrl('c') => return UiAction::CloseActiveOverlay,
                KeyInput::Ctrl('r') => {
                    if let Some((index, None)) = visible.get(self.cursor).copied() {
                        let server = &mut self.servers[index];
                        if !server.connected
                            && !matches!(server.status.as_str(), "connecting" | "blocked")
                        {
                            server.status = String::from("connecting");
                            return UiAction::Pick(format!("mcp:reconnect:{}", server.name));
                        }
                    }
                }
                _ => {}
            }
            return UiAction::None;
        }
        match key {
            KeyInput::Ctrl('c') => return UiAction::CloseActiveOverlay,
            KeyInput::Char('s') | KeyInput::Char('S') | KeyInput::Ctrl('s') => {
                return UiAction::Pick(String::from("mcp:save"));
            }
            KeyInput::Esc => {
                if self.dirty {
                    self.confirming_discard = true;
                    self.discard_selected = 1;
                } else {
                    return UiAction::CloseActiveOverlay;
                }
            }
            KeyInput::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyInput::Down => self.cursor = (self.cursor + 1).min(visible.len().saturating_sub(1)),
            KeyInput::Enter => toggle_expand_or_tool(self, visible.get(self.cursor).copied()),
            KeyInput::Char(' ') => toggle_direct(self, visible.get(self.cursor).copied()),
            KeyInput::Char('d') | KeyInput::Char('D') => {
                toggle_disabled(self, visible.get(self.cursor).copied())
            }
            KeyInput::Ctrl('r') => {
                if let Some((server_index, _)) = visible.get(self.cursor).copied() {
                    let server = &mut self.servers[server_index];
                    if matches!(server.status.as_str(), "connecting" | "blocked") {
                        return UiAction::None;
                    }
                    if server.connected {
                        server.notice = Some(String::from("(already connected)"));
                        return UiAction::None;
                    }
                    server.status = String::from("connecting");
                    return UiAction::Pick(format!("mcp:reconnect:{}", server.name));
                }
            }
            _ => {}
        }
        UiAction::None
    }

    pub(crate) fn render_lines(&self, width: u16) -> Vec<StyledLine> {
        render_panel(self, width)
    }
}

fn render_panel(panel: &McpPanelState, width: u16) -> Vec<StyledLine> {
    let width = width.min(96).max(48) as usize;
    let inner = width.saturating_sub(2);
    let mut lines = Vec::new();
    let title = " MCP Servers ";
    let border_len = inner.saturating_sub(display_width(title));
    let left = border_len / 2;
    let right = border_len - left;
    lines.push(StyledLine::segments([
        (format!("╭{}", "─".repeat(left)), ThemeColor::Dim),
        (title.to_string(), ThemeColor::Title),
        (format!("{}╮", "─".repeat(right)), ThemeColor::Dim),
    ]));

    let visible = visible_items(panel);
    lines.push(row("", inner, ThemeColor::Default));
    if panel.servers.is_empty() {
        lines.push(row("No MCP servers configured.", inner, ThemeColor::Dim));
        lines.push(row("", inner, ThemeColor::Default));
    } else {
        let start = panel
            .cursor
            .saturating_sub(6)
            .min(visible.len().saturating_sub(12));
        for (row_index, item) in visible.iter().enumerate().skip(start).take(12) {
            lines.push(item_row(panel, *item, row_index == panel.cursor, inner));
        }
        lines.push(row("", inner, ThemeColor::Default));
        if visible.len() > 12 {
            lines.push(row(
                &progress(panel.cursor, visible.len()),
                inner,
                ThemeColor::Default,
            ));
            lines.push(row("", inner, ThemeColor::Default));
        }
    }

    lines.push(line(format!("├{}┤", "─".repeat(inner)), ThemeColor::Dim));
    if let Some(notice) = &panel.auth_notice {
        lines.push(row(notice, inner, ThemeColor::Warning));
    } else {
        lines.push(row("", inner, ThemeColor::Default));
    }
    if panel.confirming_discard {
        lines.push(row(&discard_prompt(panel), inner, ThemeColor::Warning));
    } else {
        lines.push(row(&stats(panel), inner, ThemeColor::Dim));
    }
    lines.push(row("", inner, ThemeColor::Default));
    if panel.read_only {
        lines.push(row(
            "↑↓ navigate  ⏎ expand  ctrl+r reconnect  esc close",
            inner,
            ThemeColor::Dim,
        ));
        lines.push(row(
            "MCP tool configuration is read-only",
            inner,
            ThemeColor::Dim,
        ));
    } else {
        lines.push(row(
            "↑↓ navigate  space direct  d disable  ⏎  expand  ctrl+r reconnect",
            inner,
            ThemeColor::Dim,
        ));
        lines.push(row(
            "s save  esc cancel/close  ctrl+c quit",
            inner,
            ThemeColor::Dim,
        ));
    }
    lines.push(line(format!("╰{}╯", "─".repeat(inner)), ThemeColor::Dim));
    lines
}

type VisibleItem = (usize, Option<usize>);

fn visible_items(panel: &McpPanelState) -> Vec<VisibleItem> {
    let mut items = Vec::new();
    for (server_index, server) in panel.servers.iter().enumerate() {
        items.push((server_index, None));
        if server.expanded {
            for (tool_index, _) in server.tools.iter().enumerate() {
                items.push((server_index, Some(tool_index)));
            }
        }
    }
    items
}

fn toggle_expand_or_tool(panel: &mut McpPanelState, item: Option<VisibleItem>) {
    let Some((server_index, tool_index)) = item else {
        return;
    };
    if let Some(tool_index) = tool_index {
        toggle_tool_direct(&mut panel.servers[server_index].tools[tool_index]);
        update_dirty(panel);
    } else if panel.servers[server_index].status == "needs-auth" {
        panel.auth_notice = Some(format!(
            "OAuth required — run /mcp-auth {} after closing this panel",
            panel.servers[server_index].name
        ));
    } else {
        panel.servers[server_index].expanded = !panel.servers[server_index].expanded;
    }
}

fn toggle_direct(panel: &mut McpPanelState, item: Option<VisibleItem>) {
    let Some((server_index, tool_index)) = item else {
        return;
    };
    if let Some(tool_index) = tool_index {
        toggle_tool_direct(&mut panel.servers[server_index].tools[tool_index]);
        update_dirty(panel);
        return;
    }
    let all_direct = panel.servers[server_index]
        .tools
        .iter()
        .all(|tool| tool.exposure == ToolExposure::Direct);
    for tool in &mut panel.servers[server_index].tools {
        tool.exposure = if all_direct {
            ToolExposure::Deferred
        } else {
            ToolExposure::Direct
        };
    }
    update_dirty(panel);
}

fn toggle_disabled(panel: &mut McpPanelState, item: Option<VisibleItem>) {
    if let Some((server_index, Some(tool_index))) = item {
        let tool = &mut panel.servers[server_index].tools[tool_index];
        tool.exposure = if tool.exposure == ToolExposure::Hidden {
            ToolExposure::Deferred
        } else {
            ToolExposure::Hidden
        };
        update_dirty(panel);
    }
}

fn toggle_tool_direct(tool: &mut McpPanelTool) {
    tool.exposure = if tool.exposure == ToolExposure::Direct {
        ToolExposure::Deferred
    } else {
        ToolExposure::Direct
    };
}

fn stats(panel: &McpPanelState) -> String {
    let direct = panel
        .servers
        .iter()
        .flat_map(|s| &s.tools)
        .filter(|t| t.exposure == ToolExposure::Direct)
        .count();
    let disabled = panel
        .servers
        .iter()
        .flat_map(|s| &s.tools)
        .filter(|t| t.exposure == ToolExposure::Hidden)
        .count();
    let tokens: usize = panel
        .servers
        .iter()
        .flat_map(|s| &s.tools)
        .filter(|t| t.exposure == ToolExposure::Direct)
        .map(|t| t.tokens)
        .sum();
    if direct == 0 {
        format!(
            "no direct tools{}",
            if disabled > 0 {
                format!("  {disabled} disabled")
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "{direct} direct  ~{tokens} tokens{}",
            if disabled > 0 {
                format!("  {disabled} disabled")
            } else {
                String::new()
            }
        )
    }
}

fn row(content: &str, inner: usize, color: ThemeColor) -> StyledLine {
    let text = truncate(content, inner.saturating_sub(1));
    row_segments([(text, color)], inner)
}

fn item_row(panel: &McpPanelState, item: VisibleItem, selected: bool, inner: usize) -> StyledLine {
    let server = &panel.servers[item.0];
    if let Some(tool_index) = item.1 {
        return row_segments(
            render_tool_segments(&server.tools[tool_index], selected, inner),
            inner,
        );
    }
    row_segments_with_min_padding(render_server_segments(server, selected), inner, 0)
}

fn row_segments(
    segments: impl IntoIterator<Item = (String, ThemeColor)>,
    inner: usize,
) -> StyledLine {
    let segments = segments.into_iter().collect::<Vec<_>>();
    row_segments_with_min_padding(segments, inner, 1)
}

fn row_segments_with_min_padding(
    segments: Vec<(String, ThemeColor)>,
    inner: usize,
    min_left_padding: usize,
) -> StyledLine {
    let text_width = segments
        .iter()
        .map(|(text, _)| display_width(text))
        .sum::<usize>();
    let left_padding = usize::from(text_width + min_left_padding + 1 <= inner) * min_left_padding;
    let content_width = inner.saturating_sub(left_padding + 1);
    let segments = truncate_segments(segments, content_width);
    let text_width = segments
        .iter()
        .map(|(text, _)| display_width(text))
        .sum::<usize>();

    let mut row_segments = vec![(format!("│{}", " ".repeat(left_padding)), ThemeColor::Dim)];
    row_segments.extend(segments);
    row_segments.push((
        format!(
            "{}│",
            " ".repeat(inner.saturating_sub(left_padding + text_width))
        ),
        ThemeColor::Dim,
    ));
    StyledLine::segments(row_segments)
}

fn render_tool_segments(
    tool: &McpPanelTool,
    selected: bool,
    inner: usize,
) -> Vec<(String, ThemeColor)> {
    let cursor = if selected { "▸" } else { " " };
    let (toggle, toggle_color) = match tool.exposure {
        ToolExposure::Hidden => ("✕", ThemeColor::Error),
        ToolExposure::Direct => ("●", ThemeColor::Success),
        ToolExposure::Deferred => ("○", ThemeColor::Dim),
    };
    let max_desc = inner.saturating_sub(display_width(&tool.name) + 10);
    let desc = if max_desc > 5 && !tool.description.is_empty() {
        Some(format!(" — {}", truncate(&tool.description, max_desc)))
    } else {
        None
    };
    let mut segments = vec![
        (
            format!("  {cursor} "),
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Default
            },
        ),
        (format!("{toggle} "), toggle_color),
        (
            tool.name.clone(),
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Default
            },
        ),
    ];
    if let Some(desc) = desc {
        segments.push((desc, ThemeColor::Dim));
    }
    segments
}

fn render_server_segments(server: &McpPanelServer, selected: bool) -> Vec<(String, ThemeColor)> {
    let expand = if server.expanded { "▾" } else { "▸" };
    let cursor = if selected { expand } else { " " };
    let direct = server
        .tools
        .iter()
        .filter(|tool| tool.exposure == ToolExposure::Direct)
        .count();
    let total = server.tools.len();
    let (toggle, toggle_color) = if total > 0 && direct == total {
        ("●", ThemeColor::Success)
    } else if direct > 0 {
        ("◐", ThemeColor::Warning)
    } else {
        ("○", ThemeColor::Dim)
    };
    let (info, info_color) = server.notice.clone().map_or_else(
        || {
            if server.status == "connecting" {
                ("(connecting...)".to_string(), ThemeColor::Warning)
            } else if !server.cached {
                ("(not cached)".to_string(), ThemeColor::Dim)
            } else {
                (format!("{direct}/{total}"), ThemeColor::Dim)
            }
        },
        |notice| (notice, ThemeColor::Dim),
    );
    let status = if server.connected {
        Some((" ✔".to_string(), ThemeColor::Success))
    } else if server.status == "blocked" {
        Some((" (URL blocked)".to_string(), ThemeColor::Warning))
    } else if server.status == "failed" {
        Some((" ✕".to_string(), ThemeColor::Error))
    } else {
        None
    };
    let mut segments = vec![
        (
            format!("{cursor} "),
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Dim
            },
        ),
        (format!("{toggle} "), toggle_color),
        (
            server.name.clone(),
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Default
            },
        ),
    ];
    if let Some(status) = status {
        segments.push(status);
    }
    segments.push((format!("  {info}"), info_color));
    segments
}

fn line(text: String, color: ThemeColor) -> StyledLine {
    StyledLine::new(text, color)
}

fn truncate_segments(
    segments: Vec<(String, ThemeColor)>,
    max_width: usize,
) -> Vec<(String, ThemeColor)> {
    if segments
        .iter()
        .map(|(text, _)| display_width(text))
        .sum::<usize>()
        <= max_width
    {
        return segments;
    }
    let mut out = Vec::new();
    let mut used = 0;
    let text_width = max_width.saturating_sub(1);
    for (text, color) in segments {
        if used >= text_width {
            out.push(("…".to_string(), color));
            return out;
        }
        let clipped = truncate_to_width(&text, text_width - used);
        used += display_width(&clipped);
        out.push((clipped, color));
        if used >= text_width {
            out.push(("…".to_string(), color));
            return out;
        }
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    truncate_to_width(text, max.saturating_sub(1)) + "…"
}

fn clear_notices(panel: &mut McpPanelState) {
    panel.auth_notice = None;
    for server in &mut panel.servers {
        server.notice = None;
    }
}

fn update_dirty(panel: &mut McpPanelState) {
    panel.dirty = panel.servers.iter().any(|server| {
        server
            .tools
            .iter()
            .any(|tool| tool.exposure != tool.was_exposure)
    });
}

fn progress(cursor: usize, total: usize) -> String {
    let filled = ((cursor + 1) * 10).div_ceil(total.max(1));
    let dots = (0..10)
        .map(|index| if index < filled { "●" } else { "○" })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{dots}  {}/{}", cursor + 1, total)
}

fn discard_prompt(panel: &McpPanelState) -> String {
    let discard = if panel.discard_selected == 0 {
        "[ Discard ]"
    } else {
        "  Discard  "
    };
    let keep = if panel.discard_selected == 1 {
        "[ Keep ]"
    } else {
        "  Keep  "
    };
    format!("Discard unsaved changes?  {discard}   {keep}")
}

fn handle_discard_key(panel: &mut McpPanelState, key: KeyInput) -> UiAction {
    match key {
        KeyInput::Ctrl('c') | KeyInput::Char('y') | KeyInput::Char('Y') => {
            return UiAction::CloseActiveOverlay;
        }
        KeyInput::Esc | KeyInput::Char('n') | KeyInput::Char('N') => {
            panel.confirming_discard = false
        }
        KeyInput::Left | KeyInput::Right | KeyInput::Tab => {
            panel.discard_selected = 1 - panel.discard_selected
        }
        KeyInput::Enter if panel.discard_selected == 0 => return UiAction::CloseActiveOverlay,
        KeyInput::Enter => panel.confirming_discard = false,
        _ => {}
    }
    UiAction::None
}

pub(crate) fn tool_selections(
    panel: &McpPanelState,
) -> Vec<crate::runtime::preparation::McpServerToolSelection> {
    panel
        .servers
        .iter()
        .filter(|server| {
            server
                .tools
                .iter()
                .any(|tool| tool.exposure != tool.was_exposure)
        })
        .map(
            |server| crate::runtime::preparation::McpServerToolSelection {
                name: server.name.clone(),
                tools: server
                    .tools
                    .iter()
                    .map(|tool| crate::runtime::preparation::McpToolSelection {
                        name: tool.name.clone(),
                        exposure: tool.exposure,
                        was_exposure: tool.was_exposure,
                    })
                    .collect(),
            },
        )
        .collect()
}

#[derive(Default)]
pub(in crate::interactive) struct PanelActions {
    tasks: HashMap<String, JoinHandle<Result<(), String>>>,
}

impl Drop for PanelActions {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.abort();
        }
    }
}

impl PanelActions {
    pub(in crate::interactive) async fn handle_pick(
        &mut self,
        value: &str,
        attachment: &AgentAttachment,
        state: &mut UiState,
    ) -> bool {
        if !state.features.mcp {
            return value.starts_with("mcp:");
        }
        if value == "mcp:save" {
            save_tools(attachment, state).await;
        } else if let Some(server) = value.strip_prefix("mcp:cancel:") {
            if let Some(task) = self.tasks.remove(server) {
                task.abort();
            }
            if let Err(error) = attachment.cancel_mcp_connect(server).await {
                state.notify(NotifyKind::Error, format!("MCP cancel failed: {error}"));
            }
        } else if let Some(server) = value.strip_prefix("mcp:reconnect:") {
            self.reconnect(server.to_string(), attachment.clone());
        } else {
            return false;
        }
        true
    }

    fn reconnect(&mut self, server: String, attachment: AgentAttachment) {
        if let Some(task) = self.tasks.remove(&server) {
            task.abort();
        }
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            match attachment.reconnect_mcp(&task_server).await {
                Ok(McpReconnectResult::Refreshed { .. }) => Ok(()),
                Ok(McpReconnectResult::RetryAfter { seconds }) => {
                    Err(format!("retry after {seconds}s"))
                }
                Err(error) => Err(error.to_string()),
            }
        });
        self.tasks.insert(server, task);
    }

    pub(in crate::interactive) async fn poll(
        &mut self,
        attachment: &AgentAttachment,
        state: &mut UiState,
    ) -> bool {
        if !state.features.mcp {
            return false;
        }
        let finished: Vec<_> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(server, _)| server.clone())
            .collect();
        let changed = !finished.is_empty();
        for server in finished {
            let task = self.tasks.remove(&server).expect("finished MCP refresh");
            let result = task.await.unwrap_or_else(|error| Err(error.to_string()));
            refresh_panel(state, attachment).await;
            if let Err(error) = result {
                state.notify(
                    NotifyKind::Warning,
                    format!("mcp {server} refresh failed: {error}"),
                );
            }
        }
        changed
    }
}

async fn save_tools(attachment: &AgentAttachment, state: &mut UiState) {
    let Some(mut panel) = overlays::take_mcp(state) else {
        return;
    };
    let selections = tool_selections(&panel);
    let result = if panel.read_only {
        Err(anyhow::anyhow!("MCP configuration is read-only"))
    } else if selections.is_empty() {
        Ok(())
    } else {
        attachment.save_mcp_tools(selections).await.map(|()| {
            for tool in panel
                .servers
                .iter_mut()
                .flat_map(|server| &mut server.tools)
            {
                tool.was_exposure = tool.exposure;
            }
            panel.dirty = false;
        })
    };
    match result {
        Ok(()) => {
            panel.auth_notice = Some(String::from("MCP config saved; /reload to apply"));
        }
        Err(error) => {
            let message = format!("failed to save mcp config: {error}");
            state.notify(NotifyKind::Error, message.clone());
            panel.auth_notice = Some(message);
        }
    }
    overlays::open_mcp(state, panel);
}

pub(in crate::interactive) fn panel_servers(
    servers: Vec<crate::runtime::McpServerView>,
) -> Vec<McpPanelServer> {
    servers
        .into_iter()
        .map(|server| McpPanelServer {
            name: server.name,
            status: server.status,
            connected: server.connected,
            cached: server.cached,
            expanded: false,
            notice: None,
            tools: server
                .tools
                .into_iter()
                .map(|tool| {
                    let tokens = (tool.name.len() + tool.description.len() + tool.schema_length)
                        .div_ceil(4)
                        + 10;
                    McpPanelTool {
                        tokens,
                        name: tool.name,
                        description: tool.description,
                        exposure: tool.exposure,
                        was_exposure: tool.exposure,
                    }
                })
                .collect(),
        })
        .collect()
}

pub(in crate::interactive) async fn refresh_panel(
    state: &mut UiState,
    attachment: &crate::runtime::attachment::AgentAttachment,
) -> bool {
    if !state.features.mcp || overlays::update_mcp(state, |_| ()).is_none() {
        return false;
    }
    match attachment.mcp_servers().await {
        Ok(servers) => {
            overlays::update_mcp(state, |panel| panel.refresh_servers(panel_servers(servers)));
        }
        Err(error) => state.notify(
            phi_tui::NotifyKind::Warning,
            format!("failed to refresh MCP status: {error}"),
        ),
    }
    true
}

pub(in crate::interactive) async fn drain_mcp_events(
    event_rx: &mut Option<
        tokio::sync::broadcast::Receiver<phi_sdk::extension::mcp::McpRuntimeEvent>,
    >,
    attachment: &crate::runtime::attachment::AgentAttachment,
    state: &mut UiState,
    show_status: bool,
    notify: bool,
) -> bool {
    if !state.features.mcp {
        return false;
    }
    let Some(event_rx) = event_rx else {
        return false;
    };
    let mut changed = false;
    let mut refresh_tool_names = false;
    loop {
        match event_rx.try_recv() {
            Ok(event) => {
                refresh_tool_names |= matches!(
                    &event,
                    phi_sdk::extension::mcp::McpRuntimeEvent::Connected { .. }
                        | phi_sdk::extension::mcp::McpRuntimeEvent::CacheUpdated { .. }
                        | phi_sdk::extension::mcp::McpRuntimeEvent::MetadataUpdated { .. }
                );
                changed = true;
                let message = format_mcp_event(&event);
                if show_status {
                    state.mcp_status = Some(message.clone());
                }
                if notify {
                    state.notify(phi_tui::NotifyKind::Info, message);
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Lagged(_)) => {
                changed = true;
                refresh_tool_names = true;
            }
            Err(TryRecvError::Closed) => break,
        }
    }
    if refresh_tool_names {
        changed |= refresh_tool_display_projection(state, attachment).await;
    }
    changed
}

pub(in crate::interactive) async fn refresh_tool_display_projection(
    state: &mut UiState,
    attachment: &crate::runtime::attachment::AgentAttachment,
) -> bool {
    if !state.features.mcp {
        return false;
    }
    let tool_names = state.conversation.tool_names();
    match attachment.mcp_tool_names(tool_names).await {
        Ok(names) => state.conversation.install_tool_titles(names),
        Err(error) => {
            state.notify(
                phi_tui::NotifyKind::Warning,
                format!("failed to refresh MCP tool titles: {error}"),
            );
            true
        }
    }
}

pub(in crate::interactive) async fn tool_display_name_projection_for_session(
    session: &crate::session::Session,
    attachment: &crate::runtime::attachment::AgentAttachment,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut tool_names = Vec::new();
    for entry in session.current_branch() {
        let crate::session::TranscriptEntryKind::Message { message } = &entry.kind else {
            continue;
        };
        let crate::session::AgentMessage::Assistant { content, .. } = message else {
            continue;
        };
        for block in content {
            if let crate::session::ContentBlock::ToolCall { name, .. } = block {
                tool_names.push(name.to_string());
            }
        }
    }
    attachment.mcp_tool_names(tool_names).await
}

fn format_mcp_event(event: &phi_sdk::extension::mcp::McpRuntimeEvent) -> String {
    match event {
        phi_sdk::extension::mcp::McpRuntimeEvent::Connected { server } => {
            format!("MCP: {server} connected")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::Disconnected { server, reason } => {
            format!("MCP: {server} disconnected ({reason})")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::CacheUpdated { server, .. } => {
            format!("MCP: {server} metadata updated")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::MetadataUpdated { server, kind } => {
            format!("MCP: {server} {kind:?} metadata updated")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::ToolCatalogChanged { server } => {
            format!("MCP: tool catalog changed for {server}")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::ConnectionFailed { server, error } => {
            format!("MCP: {server} failed: {error}")
        }
        phi_sdk::extension::mcp::McpRuntimeEvent::MetadataRefreshFailed {
            server,
            kind,
            error,
        } => format!("MCP: {server} {kind:?} refresh failed: {error}"),
        phi_sdk::extension::mcp::McpRuntimeEvent::SubscriptionFailed { server, error } => {
            format!("MCP: {server} subscription failed: {error}")
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ElicitationState {
    pub(crate) server: String,
    pub(crate) message: String,
    pub(crate) kind: ElicitationKind,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum ElicitationKind {
    Form {
        fields: Vec<FormField>,
        selected: usize,
    },
    Url {
        url: String,
        choice: Choice,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct FormField {
    name: String,
    label: String,
    required: bool,
    value: FieldValue,
}

#[derive(Debug, Clone)]
enum FieldValue {
    Text {
        value: EditorState,
        integer: bool,
        number: bool,
    },
    Bool(bool),
    Single {
        options: Vec<OptionValue>,
        selected: usize,
    },
    Multi {
        options: Vec<OptionValue>,
        cursor: usize,
        selected: HashSet<usize>,
        min: usize,
        max: usize,
    },
}

#[derive(Debug, Clone)]
struct OptionValue {
    value: String,
    label: String,
}

pub(crate) enum ElicitationAction {
    None,
    Submit(Option<Value>),
    Decline,
    Cancel,
    OpenUrl(String),
}

impl ElicitationState {
    pub(crate) fn from_request(request: &McpElicitationRequest) -> Result<Self, String> {
        match request {
            McpElicitationRequest::Form {
                server,
                message,
                requested_schema,
                ..
            } => Ok(Self {
                server: server.clone(),
                message: message.clone(),
                kind: ElicitationKind::Form {
                    fields: parse_fields(requested_schema)?,
                    selected: 0,
                },
                error: None,
            }),
            McpElicitationRequest::Url {
                server,
                message,
                url,
                ..
            } => Ok(Self {
                server: server.clone(),
                message: message.clone(),
                kind: ElicitationKind::Url {
                    url: url.clone(),
                    choice: Choice::new(["Open URL", "Completed", "Decline"]),
                },
                error: None,
            }),
        }
    }

    fn handle_key(&mut self, key: KeyInput) -> ElicitationAction {
        self.error = None;
        if let ElicitationKind::Form { fields, selected } = &mut self.kind {
            return handle_form_key(fields, selected, key).unwrap_or_else(|error| {
                self.error = Some(error);
                ElicitationAction::None
            });
        }
        let ElicitationKind::Url { url, choice } = &mut self.kind else {
            return ElicitationAction::None;
        };
        match choice.handle_input(&key) {
            InputOutcome::Action(ChoiceAction::Selected(0)) => {
                ElicitationAction::OpenUrl(url.clone())
            }
            InputOutcome::Action(ChoiceAction::Selected(1)) => ElicitationAction::Submit(None),
            InputOutcome::Action(ChoiceAction::Selected(_)) => ElicitationAction::Decline,
            InputOutcome::Action(ChoiceAction::Cancelled) => ElicitationAction::Cancel,
            _ => ElicitationAction::None,
        }
    }
}

impl Widget for ElicitationState {
    type Action = ElicitationAction;

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match self.handle_key(key.clone()) {
            ElicitationAction::None => InputOutcome::Consumed,
            action => InputOutcome::Action(action),
        }
    }

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let width = context.area.width;
        let box_width = width.saturating_sub(4).min(68);
        let child = context.with_area(RenderArea {
            width: box_width,
            ..context.area
        });
        let inner_width = box_width.saturating_sub(4) as usize;
        let mut lines = vec![
            StyledLine::new(
                format!("MCP input from {}", self.server),
                ThemeColor::Warning,
            )
            .bold(),
            StyledLine::new(
                phi_tui::text::truncate(&self.message, inner_width),
                ThemeColor::Text,
            ),
        ];
        match &self.kind {
            ElicitationKind::Form { fields, selected } => {
                let start = selected.saturating_sub(3);
                for (index, field) in fields.iter().enumerate().skip(start).take(7) {
                    let marker = if index == *selected { ">" } else { " " };
                    let required = if field.required { "*" } else { "" };
                    lines.push(StyledLine::new(
                        phi_tui::text::truncate(
                            &format!(
                                "{marker} {}{required}: {}",
                                field.label,
                                display_value(&field.value)
                            ),
                            inner_width,
                        ),
                        if index == *selected {
                            ThemeColor::Text
                        } else {
                            ThemeColor::Dim
                        },
                    ));
                }
                lines.push(StyledLine::new(
                    "tab/↑↓ field  ←→ choice  enter next/submit  esc cancel",
                    ThemeColor::Dim,
                ));
            }
            ElicitationKind::Url { url, choice } => {
                lines.push(StyledLine::new(
                    phi_tui::text::truncate(url, inner_width),
                    ThemeColor::Title,
                ));
                lines
                    .extend(choice.render(&child.with_area(RenderArea::width(inner_width as u16))));
                lines.push(StyledLine::new(
                    "Opening uses your system browser; credentials never pass through Phi",
                    ThemeColor::Dim,
                ));
            }
        }
        if let Some(error) = &self.error {
            lines.push(StyledLine::new(
                phi_tui::text::truncate(error, inner_width),
                ThemeColor::Error,
            ));
        }
        Panel {
            title: String::new(),
            lines,
            max_height: 13,
            border_color: ThemeColor::Dim,
        }
        .render(&child)
        .into_iter()
        .take(context.area.height.unwrap_or(usize::MAX))
        .map(|line| line.prepend_segment("   ", ThemeColor::Dim).fit(width))
        .collect()
    }
}

fn parse_fields(schema: &Value) -> Result<Vec<FormField>, String> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("MCP form schema has no properties")?;
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let mut fields = properties
        .iter()
        .map(|(name, schema)| {
            let label = schema
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string();
            Ok(FormField {
                name: name.clone(),
                label,
                required: required.contains(name.as_str()),
                value: parse_value(schema)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(fields)
}

fn parse_value(schema: &Value) -> Result<FieldValue, String> {
    if let Some(options) = options(schema.get("oneOf").and_then(Value::as_array)) {
        return Ok(FieldValue::Single {
            selected: default_index(schema, &options),
            options,
        });
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("array") => {
            let items = schema.get("items").ok_or("enum array has no items")?;
            let options = options(
                items
                    .get("anyOf")
                    .or_else(|| items.get("oneOf"))
                    .and_then(Value::as_array),
            )
            .or_else(|| string_options(items.get("enum").and_then(Value::as_array)))
            .ok_or("enum array has no options")?;
            let selected = schema
                .get("default")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|value| options.iter().position(|option| option.value == value))
                .collect();
            Ok(FieldValue::Multi {
                options,
                cursor: 0,
                selected,
                min: schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize,
                max: schema
                    .get("maxItems")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX) as usize,
            })
        }
        Some("string") if schema.get("enum").is_some() => {
            let options = string_options(schema.get("enum").and_then(Value::as_array))
                .ok_or("enum has no options")?;
            Ok(FieldValue::Single {
                selected: default_index(schema, &options),
                options,
            })
        }
        Some("boolean") => Ok(FieldValue::Bool(
            schema
                .get("default")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )),
        Some("number") | Some("integer") | Some("string") => {
            let mut value = EditorState::default();
            value.set_text(schema.get("default").map(value_text).unwrap_or_default());
            Ok(FieldValue::Text {
                value,
                integer: schema.get("type").and_then(Value::as_str) == Some("integer"),
                number: matches!(
                    schema.get("type").and_then(Value::as_str),
                    Some("number" | "integer")
                ),
            })
        }
        other => Err(format!(
            "unsupported MCP form field type: {}",
            other.unwrap_or("missing")
        )),
    }
}

fn options(values: Option<&Vec<Value>>) -> Option<Vec<OptionValue>> {
    values
        .map(|values| {
            values
                .iter()
                .filter_map(|item| {
                    Some(OptionValue {
                        value: item.get("const")?.as_str()?.to_string(),
                        label: item
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or(item.get("const")?.as_str()?)
                            .to_string(),
                    })
                })
                .collect()
        })
        .filter(|values: &Vec<_>| !values.is_empty())
}

fn string_options(values: Option<&Vec<Value>>) -> Option<Vec<OptionValue>> {
    values
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| OptionValue {
                    value: value.to_string(),
                    label: value.to_string(),
                })
                .collect()
        })
        .filter(|values: &Vec<_>| !values.is_empty())
}

fn default_index(schema: &Value, options: &[OptionValue]) -> usize {
    schema
        .get("default")
        .and_then(Value::as_str)
        .and_then(|value| options.iter().position(|option| option.value == value))
        .unwrap_or(0)
}

fn handle_form_key(
    fields: &mut [FormField],
    selected: &mut usize,
    key: KeyInput,
) -> Result<ElicitationAction, String> {
    if fields.is_empty() {
        return match key {
            KeyInput::Enter => Ok(ElicitationAction::Submit(Some(Value::Object(Map::new())))),
            KeyInput::Esc | KeyInput::Ctrl('c') => Ok(ElicitationAction::Cancel),
            _ => Ok(ElicitationAction::None),
        };
    }
    match key {
        KeyInput::Up => *selected = selected.saturating_sub(1),
        KeyInput::Down | KeyInput::Tab => *selected = (*selected + 1).min(fields.len() - 1),
        KeyInput::Left => cycle_value(&mut fields[*selected].value, false),
        KeyInput::Right => cycle_value(&mut fields[*selected].value, true),
        KeyInput::Char(' ') if matches!(fields[*selected].value, FieldValue::Multi { .. }) => {
            toggle_multi(&mut fields[*selected].value)
        }
        KeyInput::Char(ch) => edit_value(&mut fields[*selected].value, Some(ch), false),
        KeyInput::Paste(text) => {
            for ch in text.chars() {
                edit_value(&mut fields[*selected].value, Some(ch), false);
            }
        }
        KeyInput::Backspace => edit_value(&mut fields[*selected].value, None, true),
        KeyInput::Enter if *selected + 1 < fields.len() => *selected += 1,
        KeyInput::Enter | KeyInput::ShiftEnter => {
            return Ok(ElicitationAction::Submit(Some(build_content(fields)?)));
        }
        KeyInput::Esc | KeyInput::Ctrl('c') => return Ok(ElicitationAction::Cancel),
        _ => {}
    }
    Ok(ElicitationAction::None)
}

fn cycle_value(value: &mut FieldValue, forward: bool) {
    match value {
        FieldValue::Bool(current) => *current = !*current,
        FieldValue::Single { options, selected }
        | FieldValue::Multi {
            options,
            cursor: selected,
            ..
        } if !options.is_empty() => {
            let mut menu = SelectMenuState {
                selected: *selected,
                ..Default::default()
            };
            if forward {
                menu.move_down(options.len());
            } else {
                menu.move_up(options.len());
            }
            *selected = menu.selected;
        }
        _ => {}
    }
}

fn toggle_multi(value: &mut FieldValue) {
    if let FieldValue::Multi {
        cursor, selected, ..
    } = value
    {
        if !selected.remove(cursor) {
            selected.insert(*cursor);
        }
    }
}

fn edit_value(value: &mut FieldValue, ch: Option<char>, backspace: bool) {
    let FieldValue::Text { value, number, .. } = value else {
        return;
    };
    if backspace {
        value.handle_input(&KeyInput::Backspace);
    } else if let Some(ch) = ch
        && (!*number || ch.is_ascii_digit() || matches!(ch, '-' | '.' | 'e' | 'E' | '+'))
    {
        value.handle_input(&KeyInput::Char(ch));
    }
}

fn build_content(fields: &[FormField]) -> Result<Value, String> {
    let mut content = Map::new();
    for field in fields {
        let value = match &field.value {
            FieldValue::Text {
                value,
                integer,
                number,
            } if value.text().trim().is_empty() => {
                if field.required {
                    return Err(format!("{} is required", field.label));
                }
                continue;
            }
            FieldValue::Text {
                value,
                integer: true,
                ..
            } => Value::Number(
                value
                    .text()
                    .parse::<i64>()
                    .map_err(|_| format!("{} must be an integer", field.label))?
                    .into(),
            ),
            FieldValue::Text {
                value,
                number: true,
                ..
            } => serde_json::Number::from_f64(
                value
                    .text()
                    .parse::<f64>()
                    .map_err(|_| format!("{} must be a number", field.label))?,
            )
            .map(Value::Number)
            .ok_or_else(|| format!("{} must be finite", field.label))?,
            FieldValue::Text { value, .. } => Value::String(value.text().to_string()),
            FieldValue::Bool(value) => Value::Bool(*value),
            FieldValue::Single { options, selected } => {
                Value::String(options[*selected].value.clone())
            }
            FieldValue::Multi {
                options,
                selected,
                min,
                max,
                ..
            } => {
                if selected.len() < *min || selected.len() > *max {
                    return Err(format!(
                        "{} requires {}..={} selections",
                        field.label, min, max
                    ));
                }
                Value::Array(
                    (0..options.len())
                        .filter(|index| selected.contains(index))
                        .map(|index| Value::String(options[index].value.clone()))
                        .collect(),
                )
            }
        };
        content.insert(field.name.clone(), value);
    }
    Ok(Value::Object(content))
}

fn display_value(value: &FieldValue) -> String {
    match value {
        FieldValue::Text { value, .. } => {
            if value.text().is_empty() {
                "<enter value>".to_string()
            } else {
                value.text().to_string()
            }
        }
        FieldValue::Bool(value) => value.to_string(),
        FieldValue::Single { options, selected } => options[*selected].label.clone(),
        FieldValue::Multi {
            options,
            cursor,
            selected,
            ..
        } => format!(
            "{} [{}]",
            options[*cursor].label,
            if selected.contains(cursor) {
                "selected"
            } else {
                " "
            }
        ),
    }
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

pub(crate) fn open_url(url: &str) -> anyhow::Result<()> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        anyhow::bail!("refusing to open non-HTTP MCP URL");
    }
    open::that_detached(url)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn readonly_panel_cannot_modify_tools_or_save() {
        let mut panel = McpPanelState {
            read_only: true,
            servers: vec![McpPanelServer {
                name: "remote".into(),
                status: "connected".into(),
                connected: true,
                cached: false,
                expanded: true,
                notice: None,
                tools: vec![McpPanelTool {
                    name: "search".into(),
                    description: String::new(),
                    exposure: ToolExposure::Deferred,
                    was_exposure: ToolExposure::Deferred,
                    tokens: 0,
                }],
            }],
            cursor: 1,
            ..Default::default()
        };
        assert!(matches!(
            panel.handle_key(KeyInput::Char('s')),
            UiAction::None
        ));
        assert!(matches!(
            panel.handle_key(KeyInput::Char('d')),
            UiAction::None
        ));
        assert!(matches!(
            panel.handle_key(KeyInput::Char(' ')),
            UiAction::None
        ));
        assert!(!panel.dirty);
        assert!(panel.servers[0].tools[0].exposure != ToolExposure::Direct);
        assert!(panel.servers[0].tools[0].exposure != ToolExposure::Hidden);
    }

    #[test]
    fn editable_panel_keeps_changes_until_save() {
        let mut panel = McpPanelState {
            servers: vec![McpPanelServer {
                name: "remote".into(),
                status: "connected".into(),
                connected: true,
                cached: true,
                expanded: true,
                notice: None,
                tools: vec![McpPanelTool {
                    name: "search".into(),
                    description: String::new(),
                    exposure: ToolExposure::Deferred,
                    was_exposure: ToolExposure::Deferred,
                    tokens: 0,
                }],
            }],
            cursor: 1,
            ..Default::default()
        };
        assert!(matches!(
            panel.handle_key(KeyInput::Char(' ')),
            UiAction::None
        ));
        assert!(panel.dirty);
        assert!(
            matches!(panel.handle_key(KeyInput::Char('s')), UiAction::Pick(value) if value == "mcp:save")
        );
        let updates = super::tool_selections(&panel);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].tools[0].exposure == ToolExposure::Direct);
        assert!(updates[0].tools[0].was_exposure != ToolExposure::Direct);
        panel.servers[0].notice = Some("retry pending".into());
        panel.refresh_servers(vec![McpPanelServer {
            status: "failed".into(),
            connected: false,
            tools: vec![McpPanelTool {
                exposure: ToolExposure::Deferred,
                was_exposure: ToolExposure::Deferred,
                ..panel.servers[0].tools[0].clone()
            }],
            ..panel.servers[0].clone()
        }]);
        assert!(panel.servers[0].expanded);
        assert_eq!(panel.servers[0].notice.as_deref(), Some("retry pending"));
        assert_eq!(panel.servers[0].status, "failed");
        assert!(panel.servers[0].tools[0].exposure == ToolExposure::Direct);
        assert!(panel.servers[0].tools[0].was_exposure != ToolExposure::Direct);
        assert!(panel.dirty);
    }

    #[test]
    fn readonly_panel_can_reconnect_without_editing_config() {
        let mut panel = McpPanelState {
            read_only: true,
            servers: vec![McpPanelServer {
                name: "remote".into(),
                status: "failed".into(),
                connected: false,
                cached: false,
                expanded: false,
                notice: None,
                tools: Vec::new(),
            }],
            ..Default::default()
        };
        assert!(matches!(panel.handle_key(KeyInput::Ctrl('r')),
            UiAction::Pick(value) if value == "mcp:reconnect:remote"));
        assert!(!panel.dirty);
        assert_eq!(panel.servers[0].status, "connecting");
        assert!(matches!(
            panel.handle_key(KeyInput::Ctrl('r')),
            UiAction::None
        ));
        assert!(matches!(panel.handle_key(KeyInput::Esc),
            UiAction::Pick(value) if value == "mcp:cancel:remote"));
        assert_eq!(panel.servers[0].status, "failed");
        assert!(!panel.dirty);
        assert!(matches!(
            panel.handle_key(KeyInput::Esc),
            UiAction::CloseActiveOverlay
        ));
        panel.servers[0].status = "blocked".into();
        assert!(matches!(
            panel.handle_key(KeyInput::Ctrl('r')),
            UiAction::None
        ));
        assert_eq!(panel.servers[0].status, "blocked");
        assert!(!panel.dirty);
    }

    #[tokio::test]
    async fn dropping_panel_actions_cancels_pending_refreshes() {
        let mut actions = PanelActions::default();
        let task = tokio::spawn(std::future::pending());
        let abort = task.abort_handle();
        actions.tasks.insert("fixture".into(), task);
        drop(actions);
        let cancelled = tokio::time::timeout(Duration::from_secs(1), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        abort.abort();
        assert!(cancelled.is_ok(), "refresh task survived its UI owner");
    }

    async fn start_panel_session() -> (
        std::path::PathBuf,
        std::sync::Arc<crate::runtime::TestRuntime>,
        AgentAttachment,
    ) {
        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            runtime.sessions.clone(),
            runtime.preparation.clone(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let started = Box::pin(workflows.start_session(session, false))
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &started,
            runtime.sessions.session_access(started.instance().id()),
            runtime.preparation.clone(),
        );
        (root, runtime, attachment)
    }

    #[tokio::test]
    async fn replaced_refresh_cannot_retire_the_current_task() {
        let (root, runtime, attachment) = start_panel_session().await;
        let mut actions = PanelActions::default();
        let previous = tokio::spawn(async { Err("stale refresh".into()) });
        while !previous.is_finished() {
            tokio::task::yield_now().await;
        }
        actions.tasks.insert("fixture".into(), previous);
        actions
            .tasks
            .insert("fixture".into(), tokio::spawn(std::future::pending()));
        let mut state = UiState::default();
        let changed = actions.poll(&attachment, &mut state).await;
        let current_tracked = actions.tasks.contains_key("fixture");
        drop(actions);
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(!changed, "replaced refresh changed the current UI");
        assert!(current_tracked, "replaced refresh retired the current task");
    }

    #[tokio::test]
    async fn panel_actions_preserve_rejected_drafts() {
        let (root, runtime, attachment) = start_panel_session().await;
        let generation = attachment
            .recovery()
            .await
            .unwrap()
            .snapshot
            .runtime_generation_id;
        let mut state = UiState::default();
        state.features.mcp = true;
        let mut actions = PanelActions::default();
        for read_only in [true, false] {
            let panel = McpPanelState {
                read_only,
                dirty: true,
                servers: vec![McpPanelServer {
                    name: "unavailable".into(),
                    status: "disconnected".into(),
                    connected: false,
                    cached: true,
                    expanded: true,
                    notice: None,
                    tools: vec![McpPanelTool {
                        name: "search".into(),
                        description: String::new(),
                        exposure: ToolExposure::Direct,
                        was_exposure: ToolExposure::Deferred,
                        tokens: 0,
                    }],
                }],
                ..Default::default()
            };
            overlays::open_mcp(&mut state, panel.clone());
            assert!(
                actions
                    .handle_pick("mcp:save", &attachment, &mut state,)
                    .await
            );
            let rejected = overlays::take_mcp(&mut state).unwrap();
            assert_eq!(rejected.servers, panel.servers);
            assert!(rejected.dirty);
            assert!(
                rejected
                    .auth_notice
                    .unwrap()
                    .starts_with("failed to save mcp config:")
            );
            assert_eq!(
                attachment
                    .recovery()
                    .await
                    .unwrap()
                    .snapshot
                    .runtime_generation_id,
                generation
            );
        }
        overlays::open_mcp(&mut state, McpPanelState::default());
        assert!(
            actions
                .handle_pick("mcp:save", &attachment, &mut state,)
                .await
        );
        let saved = overlays::take_mcp(&mut state).unwrap();
        assert_eq!(
            saved.auth_notice.as_deref(),
            Some("MCP config saved; /reload to apply")
        );
        assert!(
            !actions
                .handle_pick("model:local:test", &attachment, &mut state,)
                .await
        );
        assert!(!actions.poll(&attachment, &mut state).await);
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn form(schema: Value) -> ElicitationState {
        ElicitationState::from_request(&McpElicitationRequest::Form {
            server: "server".to_string(),
            protocol_request_id: serde_json::json!(1),
            parent_tool_call_id: Some("tool-call".to_string()),
            message: "Provide input".to_string(),
            requested_schema: schema,
        })
        .unwrap()
    }

    #[test]
    fn validates_and_submits_form_values() {
        let mut state = form(serde_json::json!({
            "type": "object",
            "properties": {
                "age": { "type": "integer" },
                "choice": { "type": "string", "enum": ["a", "b"] }
            },
            "required": ["age", "choice"]
        }));

        assert!(matches!(
            state.handle_key(KeyInput::ShiftEnter),
            ElicitationAction::None
        ));
        assert_eq!(state.error.as_deref(), Some("age is required"));
        state.handle_key(KeyInput::Char('4'));
        state.handle_key(KeyInput::Char('2'));
        state.handle_key(KeyInput::Tab);
        state.handle_key(KeyInput::Right);
        let ElicitationAction::Submit(Some(content)) = state.handle_key(KeyInput::Enter) else {
            panic!("expected submitted form");
        };
        assert_eq!(content, serde_json::json!({ "age": 42, "choice": "b" }));
    }

    #[test]
    fn form_multi_bounds_and_text_editing_preserve_schema_values() {
        let mut state = form(serde_json::json!({
            "properties": {
                "choices": { "type": "array", "items": { "enum": ["a", "b"] }, "minItems": 1, "maxItems": 1 },
                "text": { "type": "string", "default": "default" }
            }
        }));
        state.handle_key(KeyInput::ShiftEnter);
        assert_eq!(
            state.error.as_deref(),
            Some("choices requires 1..=1 selections")
        );
        state.handle_key(KeyInput::Char(' '));
        state.handle_key(KeyInput::Left);
        state.handle_key(KeyInput::Char(' '));
        state.handle_key(KeyInput::ShiftEnter);
        assert_eq!(
            state.error.as_deref(),
            Some("choices requires 1..=1 selections")
        );
        state.handle_key(KeyInput::Char(' '));
        state.handle_key(KeyInput::Tab);
        state.handle_key(KeyInput::Left);
        state.handle_key(KeyInput::Paste(" e\u{301}👩‍💻".into()));
        state.handle_key(KeyInput::Backspace);
        state.handle_key(KeyInput::Backspace);
        let ElicitationAction::Submit(Some(content)) = state.handle_key(KeyInput::ShiftEnter)
        else {
            panic!("expected form content");
        };
        assert_eq!(
            content,
            serde_json::json!({ "choices": ["a"], "text": "default " })
        );
        let theme = phi_tui::Theme::default();
        for width in [0, 1, 24, 80] {
            let lines = state.render(&RenderContext::new(
                RenderArea {
                    width,
                    height: Some(3),
                },
                &theme,
            ));
            assert!(lines.len() <= 3);
            assert!(
                lines
                    .iter()
                    .all(|line| phi_tui::text::display_width(&line.text) <= width as usize)
            );
        }
        assert!(matches!(
            state.handle_input(&KeyInput::Esc),
            InputOutcome::Action(ElicitationAction::Cancel)
        ));
    }
}
