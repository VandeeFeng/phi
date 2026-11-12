use phi_sdk::tools::{
    NestedCallStatus, NestedToolCall, ToolDisplay, ToolDisplayFormatter, ToolResultDetails,
};
use phi_tui::{
    BlockBody, BlockExpansion, BlockRole, BlockStatus, BlockText, DiffView, ImageBlock, NotifyKind,
    NotifyMessage, StyledLine, ThemeColor, TranscriptBlock,
};

use super::conversation::{
    BlockId, ConversationView, LiveRunView, TOOL_TIMER_INTERVAL, ToolBlockMeta, ToolTimer, UiBlock,
};
use super::format::{
    codemode_output_body, format_tool_display, format_tool_title, read_input_from_output,
    tool_diff_view,
};

const TOOL_BODY_INDENT: &str = "   ";

fn subagent_header(name: &str, args: &serde_json::Value) -> TranscriptBlock {
    let display = format_tool_display(name, &args.to_string());
    TranscriptBlock {
        title: display.title.unwrap_or_else(|| name.into()),
        input: display
            .input
            .map(|input| phi_tui::abbreviate_home_paths(&input)),
        body: BlockBody::Output("".into()),
        status: BlockStatus::Running,
        expansion: BlockExpansion::Fixed,
        ..TranscriptBlock::text("")
    }
}

impl ConversationView {
    pub(in crate::interactive) fn start_live_run(&mut self) {
        self.live_run = Some(LiveRunView::default());
    }

    pub(in crate::interactive) fn clear_live_run(&mut self) {
        if let Some(run) = self.live_run.take() {
            for block_id in run.tool_blocks.into_values() {
                self.finish_tool_timing(block_id);
                self.finish_nested_calls(block_id);
            }
        }
    }

    pub(in crate::interactive) fn live_assistant_block(&self) -> Option<BlockId> {
        self.live_run.as_ref()?.assistant_block
    }

    pub(in crate::interactive) fn append_live_assistant(&mut self, text: &str) -> Option<BlockId> {
        let (block_id, group_id) = {
            let run = self.live_run.as_ref()?;
            (run.assistant_block, run.assistant_group)
        };
        let block_id = match block_id {
            Some(block_id) => block_id,
            None => {
                let block_id = self.push_text_block(BlockRole::Assistant, "");
                if let Some(group_id) = group_id {
                    self.set_block_group(block_id, group_id);
                }
                if let Some(run) = self.live_run.as_mut() {
                    run.assistant_block = Some(block_id);
                    run.assistant_group.get_or_insert(block_id);
                }
                block_id
            }
        };
        self.append_text_block(block_id, text);
        Some(block_id)
    }

    pub(in crate::interactive) fn push_live_assistant_image(
        &mut self,
        image: ImageBlock,
    ) -> Option<BlockId> {
        let run = self.live_run.as_ref()?;
        let group_id = run.assistant_group.or(run.assistant_block);
        let block_id = self.push_image_block(BlockRole::Assistant, image);
        if let Some(group_id) = group_id {
            self.set_block_group(block_id, group_id);
        }
        if let Some(run) = self.live_run.as_mut() {
            run.assistant_group = Some(group_id.unwrap_or(block_id));
            run.assistant_block = None;
        }
        Some(block_id)
    }

    pub(in crate::interactive) fn reset_live_assistant(&mut self) {
        if let Some(run) = self.live_run.as_mut() {
            run.assistant_block = None;
            run.assistant_group = None;
        }
    }

    pub(in crate::interactive) fn set_live_reasoning(&mut self, text: &str) -> Option<BlockId> {
        let block_id = self.live_run.as_ref()?.reasoning_block;
        let block_id = match block_id {
            Some(block_id) => {
                if let Some(block) = self.touch_block(block_id) {
                    block.block.set_body(BlockBody::Thinking(text.into()));
                    block.block.set_status(BlockStatus::Running);
                }
                block_id
            }
            None => {
                let block_id = self.push_thinking_block_with_status(text, BlockStatus::Running, 0);
                if let Some(run) = self.live_run.as_mut() {
                    run.reasoning_block = Some(block_id);
                }
                block_id
            }
        };
        Some(block_id)
    }

    pub(in crate::interactive) fn complete_live_reasoning(&mut self) -> Option<()> {
        let block_id = self.live_run.as_ref()?.reasoning_block?;
        let has_text = self.block(block_id).is_some_and(|block| {
            matches!(&block.block.body, BlockBody::Thinking(text) if !text.trim().is_empty())
        });
        self.complete_thinking_block(block_id);
        has_text.then_some(())
    }

    pub(in crate::interactive) fn reset_live_reasoning(&mut self) {
        if let Some(run) = self.live_run.as_mut() {
            run.reasoning_block = None;
        }
    }

    pub(in crate::interactive) fn live_reasoning_is_visible(&self) -> bool {
        self.live_run
            .as_ref()
            .and_then(|run| run.reasoning_block)
            .is_none_or(|block_id| self.block_is_expanded(block_id))
    }

    pub(in crate::interactive) fn project_tool_call_started(
        &mut self,
        id: &str,
        name: &str,
        args: &str,
        display: ToolDisplay,
    ) -> BlockId {
        let run = self.live_run.get_or_insert_default();
        if let Some(block_id) = run.tool_blocks.get(id) {
            return *block_id;
        }
        let block_id = self.project_tool_block_with_display(name, args, display);
        if let Some(block) = self.touch_block(block_id) {
            block.tool_call_id = Some(id.to_string());
        }
        if name.eq_ignore_ascii_case("bash") {
            self.start_tool_timing(block_id);
        }
        let run = self.live_run.get_or_insert_default();
        run.tool_blocks.insert(id.to_string(), block_id);
        run.active_tool_block = Some(block_id);
        block_id
    }

    pub(in crate::interactive) fn project_tool_call_finished(
        &mut self,
        id: &str,
        output: &str,
        is_error: bool,
        details: Option<&ToolResultDetails>,
    ) -> bool {
        let Some(run) = self.live_run.as_mut() else {
            return false;
        };
        let Some(block_id) = run.tool_blocks.remove(id) else {
            return false;
        };
        if run.active_tool_block == Some(block_id) {
            run.active_tool_block = None;
        }
        self.project_tool_result(block_id, output, is_error, details);
        true
    }

    pub(in crate::interactive) fn project_nested_tool_call(
        &mut self,
        call: &NestedToolCall,
    ) -> bool {
        let Some(parent) = self
            .blocks
            .iter()
            .find(|block| block.tool_call_id.as_deref() == Some(call.parent_tool_call_id.as_str()))
            .map(|block| block.id)
        else {
            return false;
        };
        let id = if let Some(id) = self.nested_tool_block(parent, &call.id) {
            id
        } else {
            let start = self.blocks.len();
            let args = call
                .arguments
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let id = self.project_tool_block(&call.name, args, None);
            if let Some(block) = self.touch_block(id) {
                block.tool_call_id = Some(call.id.clone());
                if call.arguments.is_none() {
                    block
                        .block
                        .set_input(Some("[arguments unavailable]".into()));
                }
            }
            self.nest_agent_blocks(parent, start);
            id
        };
        let status = match call.status {
            NestedCallStatus::Running => BlockStatus::Running,
            NestedCallStatus::Ok => BlockStatus::Success,
            NestedCallStatus::Error | NestedCallStatus::Cancelled => BlockStatus::Error,
        };
        self.set_block_status(id, status);
        if !matches!(call.status, NestedCallStatus::Running) {
            let prefix = if matches!(call.status, NestedCallStatus::Cancelled) {
                "Cancelled · Took"
            } else {
                "Took"
            };
            self.set_tool_timing_footer(
                id,
                format!(
                    "{prefix} {:.1}s",
                    std::time::Duration::from_millis(call.duration_ms).as_secs_f64()
                ),
            );
        }
        true
    }

    fn finish_nested_calls(&mut self, parent: BlockId) {
        let pending = self
            .blocks
            .iter()
            .filter(|block| {
                block.parent_id == Some(parent) && block.block.status == BlockStatus::Running
            })
            .map(|block| block.id)
            .collect::<Vec<_>>();
        for id in pending {
            self.set_block_status(id, BlockStatus::Idle);
            self.set_tool_timing_footer(id, "Status unavailable".into());
        }
    }

    pub(in crate::interactive) fn live_active_tool_block(&self) -> Option<BlockId> {
        self.live_run.as_ref()?.active_tool_block
    }

    pub(in crate::interactive) fn push_text_block(
        &mut self,
        role: BlockRole,
        text: impl Into<BlockText>,
    ) -> BlockId {
        let block = if role == BlockRole::User {
            TranscriptBlock::collapsible_text(text)
        } else {
            TranscriptBlock::text(text)
        };
        self.push_block(role, block)
    }

    pub(in crate::interactive) fn push_shared_text_block(
        &mut self,
        role: BlockRole,
        text: &phi_sdk::transcript::SharedText,
    ) -> BlockId {
        self.push_text_block(role, BlockText::shared(text.shared()))
    }

    pub(in crate::interactive) fn push_image_block(
        &mut self,
        role: BlockRole,
        image: ImageBlock,
    ) -> BlockId {
        self.push_block(role, TranscriptBlock::image(image))
    }

    pub(in crate::interactive) fn push_prompt_disclosure_block(
        &mut self,
        role: BlockRole,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> BlockId {
        self.push_block(
            role,
            TranscriptBlock {
                title: title.into(),
                title_suffix: None,
                input: None,
                body: BlockBody::Prompt(text.into()),
                footer: None,
                expansion: BlockExpansion::Collapsed,
                status: BlockStatus::Idle,
                indent: 0,
            },
        )
    }

    pub(super) fn project_failure(&mut self, notice: NotifyMessage) {
        self.push_block(
            BlockRole::Error,
            TranscriptBlock {
                body: BlockBody::Notify(notice),
                ..TranscriptBlock::text("")
            },
        );
    }

    pub(in crate::interactive) fn notify(&mut self, kind: NotifyKind, message: impl Into<String>) {
        self.push_notify_body(BlockBody::Notify(NotifyMessage::new(kind, message)));
    }

    pub(in crate::interactive) fn notify_operation_canceled(&mut self) {
        self.push_notify_body(BlockBody::Styled(vec![StyledLine::new(
            "Operation canceled",
            ThemeColor::Warning,
        )]));
    }

    fn push_notify_body(&mut self, body: BlockBody) {
        self.push_block(
            BlockRole::Event,
            TranscriptBlock {
                title: String::new(),
                title_suffix: None,
                input: None,
                body,
                footer: None,
                expansion: BlockExpansion::Fixed,
                status: BlockStatus::Idle,
                indent: 0,
            },
        );
        self.blocks.last_mut().unwrap().local_notification = true;
    }

    pub(in crate::interactive) fn push_thinking_block(
        &mut self,
        text: impl Into<BlockText>,
    ) -> BlockId {
        self.push_thinking_block_with_status(text, BlockStatus::Success, 0)
    }

    pub(in crate::interactive) fn push_shared_thinking_block(
        &mut self,
        text: &phi_sdk::transcript::SharedText,
    ) -> BlockId {
        self.push_thinking_block_with_status(
            BlockText::shared(text.shared()),
            BlockStatus::Success,
            0,
        )
    }

    fn push_thinking_block_with_status(
        &mut self,
        text: impl Into<BlockText>,
        status: BlockStatus,
        indent: usize,
    ) -> BlockId {
        self.push_block(
            BlockRole::Event,
            TranscriptBlock {
                title: String::new(),
                title_suffix: None,
                input: None,
                body: BlockBody::Thinking(text.into()),
                footer: None,
                expansion: BlockExpansion::Collapsed,
                status,
                indent,
            },
        )
    }

    pub(in crate::interactive) fn push_agent_result_block(
        &mut self,
        title: impl Into<String>,
        output: impl Into<String>,
        status: BlockStatus,
    ) -> BlockId {
        self.push_block(
            BlockRole::Event,
            TranscriptBlock {
                title: title.into(),
                title_suffix: None,
                input: None,
                body: BlockBody::Output(BlockText::from(output.into())),
                footer: None,
                expansion: BlockExpansion::Collapsed,
                status,
                indent: 0,
            },
        )
    }

    pub(in crate::interactive) fn project_subagent_container(
        &mut self,
        call_id: Option<&str>,
        title: String,
    ) -> BlockId {
        let id = self.push_agent_result_block(title, "", BlockStatus::Running);
        if let Some(block) = self.touch_block(id) {
            block.agent_container = true;
            block.block.set_body(BlockBody::Group("".into()));
        }
        self.place_subagent_container(call_id);
        id
    }

    pub(in crate::interactive) fn restore_subagent_position(&mut self, id: BlockId) {
        let Some(notice) = self
            .blocks
            .iter()
            .position(|block| block.local_notification)
        else {
            return;
        };
        let Some(position) = self.blocks.iter().position(|block| block.id == id) else {
            return;
        };
        if position > notice {
            let block = self.blocks.remove(position);
            self.blocks.insert(notice, block);
            self.reindex_blocks();
        }
    }

    fn place_subagent_container(&mut self, call_id: Option<&str>) {
        let position = call_id.and_then(|id| {
            self.blocks.iter().position(|block| {
                block.tool_call_id.as_deref() == Some(id) && block.parent_id.is_none()
            })
        });
        if let Some(position) = position {
            let block = self
                .blocks
                .pop()
                .expect("subagent container was just appended");
            let mut insertion = position + 1;
            while self
                .blocks
                .get(insertion)
                .is_some_and(|block| block.agent_container || block.parent_id.is_some())
            {
                insertion += 1;
            }
            self.blocks.insert(insertion, block);
            self.reindex_blocks();
        }
    }

    pub(in crate::interactive) fn remove_subagent_trace(&mut self, parent: BlockId) {
        self.blocks.retain(|block| {
            let retained = block.id != parent && block.parent_id != Some(parent);
            if !retained && self.focused_block == Some(block.id) {
                self.focused_block = None;
            }
            retained
        });
        self.reindex_blocks();
        self.projection_revision = self.projection_revision.saturating_add(1);
    }

    pub(in crate::interactive) fn project_subagent_status(
        &mut self,
        parent: BlockId,
        title: impl Into<String>,
        metadata: &str,
        status: BlockStatus,
    ) {
        let title = title.into();
        let output = metadata.to_string();
        if self.block(parent).is_some_and(|block| {
            block.block.title == title
                && block.block.status == status
                && matches!(&block.block.body, BlockBody::Group(text) | BlockBody::Output(text)
                    if &**text == output)
        }) {
            return;
        }
        self.set_output_block(parent, title, output, status);
    }

    pub(in crate::interactive) fn project_subagent_tool_finished(
        &mut self,
        parent_id: BlockId,
        call_id: &str,
        is_error: bool,
    ) -> bool {
        let status = if is_error {
            BlockStatus::Error
        } else {
            BlockStatus::Success
        };
        let Some(id) = self.nested_tool_block(parent_id, call_id) else {
            return false;
        };
        if self
            .block(id)
            .is_some_and(|block| block.block.status == status)
        {
            return false;
        }
        self.set_block_status(id, status);
        true
    }

    pub(in crate::interactive) fn project_subagent_thinking(
        &mut self,
        parent: BlockId,
        thinking: bool,
        observed: bool,
    ) -> bool {
        let status = if thinking {
            BlockStatus::Running
        } else {
            BlockStatus::Success
        };
        let existing = self
            .blocks
            .iter()
            .find(|block| {
                block.parent_id == Some(parent)
                    && matches!(block.block.body, BlockBody::Thinking(_))
            })
            .map(|block| (block.id, block.block.status));
        if let Some((id, previous)) = existing {
            if previous == status {
                return false;
            }
            if thinking {
                self.set_block_status(id, status);
            } else {
                self.complete_thinking_block(id);
            }
        } else if observed {
            let start = self.blocks.len();
            let id = self.push_thinking_block_with_status("", status, 0);
            if let Some(block) = self.touch_block(id) {
                block.block.set_expansion(BlockExpansion::Fixed);
            }
            self.nest_agent_blocks(parent, start);
        } else {
            return false;
        }
        true
    }

    fn nested_tool_block(&self, parent_id: BlockId, call_id: &str) -> Option<BlockId> {
        self.blocks
            .iter()
            .find(|block| {
                block.parent_id == Some(parent_id) && block.tool_call_id.as_deref() == Some(call_id)
            })
            .map(|block| block.id)
    }

    pub(in crate::interactive) fn project_subagent_tool_started(
        &mut self,
        parent_id: BlockId,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> bool {
        if self.nested_tool_block(parent_id, call_id).is_some() {
            return false;
        }
        let start = self.blocks.len();
        let id = self.push_block(BlockRole::Event, subagent_header(name, args));
        if let Some(block) = self.touch_block(id) {
            block.tool_call_id = Some(call_id.into());
        }
        self.nest_agent_blocks(parent_id, start);
        true
    }

    pub(in crate::interactive) fn nest_agent_blocks(&mut self, parent_id: BlockId, start: usize) {
        let Some(position) = self.block_positions.get(&parent_id).copied() else {
            return;
        };
        if start == self.blocks.len() {
            return;
        }
        let mut children = self.blocks.split_off(start);
        for child in &mut children {
            child.parent_id = Some(parent_id);
            child.group_id = parent_id;
            child.block.set_indent(child.block.indent + 2);
        }
        let end = self.blocks[position + 1..]
            .iter()
            .position(|block| block.parent_id != Some(parent_id))
            .map_or(self.blocks.len(), |offset| position + 1 + offset);
        self.blocks.splice(end..end, children);
        self.reindex_blocks();
        self.projection_revision = self.projection_revision.saturating_add(1);
    }

    pub(in crate::interactive) fn reindex_blocks(&mut self) {
        self.block_positions.clear();
        self.block_positions.extend(
            self.blocks
                .iter()
                .enumerate()
                .map(|(position, block)| (block.id, position)),
        );
    }

    pub(in crate::interactive) fn project_tool_block(
        &mut self,
        name: impl Into<String>,
        args: impl Into<String>,
        tool_display: Option<&dyn ToolDisplayFormatter>,
    ) -> BlockId {
        let name = name.into();
        let args = args.into();
        let display = resolve_tool_display(self, &name, &args, tool_display);
        self.project_tool_block_with_display(name, args, display)
    }

    fn project_tool_block_with_display(
        &mut self,
        name: impl Into<String>,
        args: impl Into<String>,
        mut display: ToolDisplay,
    ) -> BlockId {
        let name = name.into();
        let args = args.into();
        let mcp_display_name = self
            .tool_display_name_projection
            .get(&name)
            .map(String::as_str);
        if display.title.is_none()
            || (name.eq_ignore_ascii_case("bash") && display.title_suffix.is_none())
        {
            let fallback = format_tool_display(&name, &args);
            if display.title.is_none() {
                display.title = fallback.title;
                display.input = display.input.or(fallback.input);
            }
            display.title_suffix = display.title_suffix.or(fallback.title_suffix);
        }
        let title = format_tool_title(display.title.as_deref().unwrap_or(&name), mcp_display_name);
        let input = if name == "Codemode" {
            display.input.clone()
        } else {
            display.input.as_deref().map(phi_tui::abbreviate_home_paths)
        };
        let title_suffix = display.title_suffix.clone();
        let view = ToolBlockMeta {
            name: name.clone(),
            args: args.clone(),
            display,
            tool_timer: None,
        };
        let mut block = TranscriptBlock::output(title);
        block.input = input;
        block.title_suffix = title_suffix;
        block.set_expanded(view.display.expand || name == "Edit");
        let id = self.push_block(BlockRole::Event, block);
        self.tool_block_meta.insert(id, view);
        id
    }

    pub(in crate::interactive) fn refresh_mcp_tool_titles(&mut self) -> bool {
        let mut changed = false;
        for block in &mut self.blocks {
            let Some(view) = self.tool_block_meta.get(&block.id) else {
                continue;
            };
            let name = &view.name;
            let mcp_display_name = self
                .tool_display_name_projection
                .get(name)
                .map(String::as_str);
            let title = format_tool_title(
                view.display.title.as_deref().unwrap_or(name),
                mcp_display_name,
            );
            if block.block.title != title {
                block.block.set_title(title);
                changed = true;
            }
        }
        if changed {
            self.projection_revision = self.projection_revision.saturating_add(1);
        }
        changed
    }

    pub(in crate::interactive) fn project_tool_result(
        &mut self,
        block_id: BlockId,
        output: &str,
        is_error: bool,
        details: Option<&ToolResultDetails>,
    ) {
        if let Some(diff) = details.and_then(|details| details.diff.as_ref()) {
            self.set_tool_diff(block_id, tool_diff_view(diff));
        } else {
            self.set_tool_output(block_id, output);
        }
        if is_error {
            self.set_tool_failed(block_id);
        }
        if let Some(calls) = details.and_then(|details| details.nested_calls.as_ref()) {
            for call in &calls.calls {
                self.project_nested_tool_call(call);
            }
        }
        self.finish_nested_calls(block_id);
    }

    pub(in crate::interactive) fn refresh_tool_timing(&mut self, now: std::time::Instant) -> bool {
        let updates: Vec<_> = self
            .tool_block_meta
            .iter_mut()
            .filter_map(|(&id, meta)| {
                let timer = meta.tool_timer.as_mut()?;
                if timer.finished_at.is_some() || now < timer.last_observed_at + TOOL_TIMER_INTERVAL
                {
                    return None;
                }
                timer.last_observed_at = now;
                Some((id, timer.label(now)))
            })
            .collect();
        let mut changed = false;
        for (id, label) in updates {
            changed |= self.set_tool_timing_footer(id, label);
        }
        changed
    }

    pub(in crate::interactive) fn start_tool_timing(&mut self, id: BlockId) {
        let Some(meta) = self.tool_block_meta.get_mut(&id) else {
            return;
        };
        if meta.tool_timer.is_some() {
            return;
        }
        meta.tool_timer = Some(ToolTimer::start(std::time::Instant::now()));
        self.set_tool_timing_footer(id, "Elapsed 0.0s".to_string());
    }

    fn finish_tool_timing(&mut self, id: BlockId) {
        self.finish_tool_timing_at(id, std::time::Instant::now());
    }

    fn finish_tool_timing_at(&mut self, id: BlockId, now: std::time::Instant) {
        let Some(timer) = self
            .tool_block_meta
            .get_mut(&id)
            .and_then(|meta| meta.tool_timer.as_mut())
        else {
            return;
        };
        timer.finished_at.get_or_insert(now);
        let label = timer.label(now);
        self.set_tool_timing_footer(id, label);
    }

    fn set_tool_timing_footer(&mut self, id: BlockId, label: String) -> bool {
        let text = format!("{TOOL_BODY_INDENT}{label}");
        if self
            .block(id)
            .and_then(|block| block.block.footer.as_ref())
            .is_some_and(|footer| footer.text == text)
        {
            return false;
        }
        let Some(block) = self.touch_block(id) else {
            return false;
        };
        block
            .block
            .set_footer(Some(StyledLine::new(text, ThemeColor::Dim)));
        true
    }

    pub(in crate::interactive) fn set_block_group(&mut self, id: BlockId, group_id: BlockId) {
        if let Some(block) = self.touch_block(id) {
            block.group_id = group_id;
        }
    }

    pub(in crate::interactive) fn set_block_status(&mut self, id: BlockId, status: BlockStatus) {
        if let Some(block) = self.touch_block(id) {
            block.block.set_status(status);
        }
    }

    pub(in crate::interactive) fn set_output_block(
        &mut self,
        id: BlockId,
        title: impl Into<String>,
        output: impl Into<String>,
        status: BlockStatus,
    ) {
        if let Some(block) = self.touch_block(id) {
            block.block.set_title(title.into());
            let output = BlockText::from(output.into());
            let body = if matches!(block.block.body, BlockBody::Group(_)) {
                BlockBody::Group(output)
            } else {
                BlockBody::Output(output)
            };
            block.block.set_body(body);
            block.block.set_status(status);
        }
    }

    pub(in crate::interactive) fn set_text_block(
        &mut self,
        id: BlockId,
        text: impl Into<BlockText>,
    ) {
        self.replace_text_block(id, text.into());
    }

    pub(in crate::interactive) fn set_shared_text_block(
        &mut self,
        id: BlockId,
        text: &phi_sdk::transcript::SharedText,
    ) {
        self.set_text_block(id, BlockText::shared(text.shared()));
    }

    fn replace_text_block(&mut self, id: BlockId, text: BlockText) {
        if let Some(block) = self.touch_block(id) {
            block.block.set_body(BlockBody::Text(text));
        }
    }

    fn append_text_block(&mut self, id: BlockId, text: &str) {
        if let Some(block) = self.touch_block(id) {
            block.block.append_text(text);
        }
    }

    pub(in crate::interactive) fn set_tool_output(
        &mut self,
        id: BlockId,
        output: impl Into<String>,
    ) {
        self.finish_tool_timing(id);
        let output = output.into();
        let view = self.tool_block_meta.get(&id);
        let is_codemode = view.is_some_and(|view| view.name == "Codemode");
        let expand = self.blocks_expanded.unwrap_or_else(|| {
            if is_codemode {
                self.block_is_expanded(id)
            } else {
                view.is_some_and(|view| view.display.expand)
            }
        });
        let read_input = view
            .filter(|view| view.name.eq_ignore_ascii_case("read"))
            .and_then(|view| {
                read_input_from_output(&view.args, view.display.input.as_deref()?, &output)
            });
        let content = view.and_then(|view| view.display.content.as_deref());
        let output = content
            .map(|content| format!("{content}\n{output}"))
            .unwrap_or(output);
        if let Some(block) = self.touch_block(id) {
            if let Some(input) = read_input {
                block
                    .block
                    .set_input(Some(phi_tui::abbreviate_home_paths(&input)));
            }
            let body = if is_codemode {
                codemode_output_body(&output)
            } else {
                BlockBody::Output(output.into())
            };
            block.block.set_body(body);
            block.block.set_expanded(expand);
            block.block.set_status(BlockStatus::Success);
        }
    }

    pub(in crate::interactive) fn set_tool_diff(&mut self, id: BlockId, diff: DiffView) {
        self.set_tool_diff_view_status(id, diff, BlockStatus::Success);
    }

    pub(in crate::interactive) fn set_tool_diff_status(
        &mut self,
        id: BlockId,
        diff: impl AsRef<str>,
        status: BlockStatus,
    ) {
        self.set_tool_diff_view_status(id, DiffView::from_prefixed_text(diff.as_ref()), status);
    }

    fn set_tool_diff_view_status(&mut self, id: BlockId, diff: DiffView, status: BlockStatus) {
        self.finish_tool_timing(id);
        let expanded = self.blocks_expanded.unwrap_or(true);
        if let Some(block) = self.touch_block(id) {
            if matches!(&block.block.body, BlockBody::Diff(current) if current == &diff) {
                block.block.set_status(status);
                return;
            }
            block.block.set_body(BlockBody::Diff(diff));
            block.block.set_expanded(expanded);
            block.block.set_status(status);
        }
    }

    pub(in crate::interactive) fn block_is_expanded(&self, id: BlockId) -> bool {
        self.block(id)
            .is_some_and(|block| block.block.is_expanded())
    }

    pub(in crate::interactive) fn complete_thinking_block(&mut self, id: BlockId) {
        if let Some(block) = self.touch_block(id) {
            block.block.set_status(BlockStatus::Success);
        }
    }

    pub(in crate::interactive) fn set_tool_failed(&mut self, id: BlockId) {
        self.finish_tool_timing(id);
        if let Some(block) = self.touch_block(id) {
            block.block.set_status(BlockStatus::Error);
        }
    }

    pub(in crate::interactive) fn retire_transcript_before(&mut self, first_retained: BlockId) {
        let Some(mut drain_end) = self.block_positions.get(&first_retained).copied() else {
            return;
        };
        let retained_group = self.blocks[drain_end].group_id;
        while drain_end > 0 && self.blocks[drain_end - 1].group_id == retained_group {
            drain_end -= 1;
        }
        if let Some(pinned) = self.first_pinned_position(drain_end) {
            drain_end = pinned;
        }
        if drain_end == 0 {
            return;
        }
        self.retired_history_blocks = self.retired_history_blocks.saturating_add(
            self.blocks[..drain_end]
                .iter()
                .filter(|block| !block.local_notification)
                .count(),
        );
        let retired = self
            .blocks
            .drain(..drain_end)
            .map(|block| block.id)
            .collect::<std::collections::HashSet<_>>();
        self.block_positions.clear();
        self.block_positions.extend(
            self.blocks
                .iter()
                .enumerate()
                .map(|(position, block)| (block.id, position)),
        );
        self.tool_block_meta
            .retain(|block_id, _| !retired.contains(block_id));
    }

    fn first_pinned_position(&self, before: usize) -> Option<usize> {
        self.blocks[..before]
            .iter()
            .position(|block| self.block_is_pinned(block.id))
    }

    fn block_is_pinned(&self, id: BlockId) -> bool {
        self.is_live_block(id)
            || self
                .blocks
                .iter()
                .any(|block| block.parent_id == Some(id) && self.is_live_block(block.id))
    }

    fn is_live_block(&self, id: BlockId) -> bool {
        self.focused_block == Some(id)
            || self.live_run.as_ref().is_some_and(|run| {
                run.assistant_block == Some(id)
                    || run.reasoning_block == Some(id)
                    || run.active_tool_block == Some(id)
                    || run.tool_blocks.values().any(|block_id| *block_id == id)
            })
    }

    pub(in crate::interactive) fn toggle_all_collapsible_blocks(&mut self) -> bool {
        if self.buffer_initialized && self.buffer_first_block.is_none() {
            return false;
        }
        let first = self
            .buffer_first_block
            .and_then(|id| self.block_positions.get(&id))
            .copied()
            .unwrap_or(0);
        let blocks = &mut self.blocks[first..];
        let expanded = self.blocks_expanded.map_or_else(
            || {
                blocks
                    .iter()
                    .any(|block| block.block.is_collapsible() && !block.block.is_expanded())
            },
            |expanded| !expanded,
        );
        let mut changed = false;
        for block in blocks {
            changed |= block.block.set_expanded(expanded);
        }
        if changed {
            self.blocks_expanded = Some(expanded);
            self.projection_revision = self.projection_revision.saturating_add(1);
        }
        changed
    }

    fn push_block(&mut self, role: BlockRole, mut block: TranscriptBlock) -> BlockId {
        if let Some(expanded) = self.blocks_expanded {
            block.set_expanded(expanded);
        }
        let id = self.allocate_block_id();
        let position = self.blocks.len();
        self.blocks.push(UiBlock {
            id,
            group_id: id,
            parent_id: None,
            agent_container: false,
            local_notification: false,
            tool_call_id: None,
            role,
            block: block.into(),
        });
        self.block_positions.insert(id, position);
        self.focused_block = Some(id);
        self.projection_revision = self.projection_revision.saturating_add(1);
        id
    }

    fn block(&self, id: BlockId) -> Option<&UiBlock> {
        self.block_positions
            .get(&id)
            .and_then(|position| self.blocks.get(*position))
    }

    fn touch_block(&mut self, id: BlockId) -> Option<&mut UiBlock> {
        let position = *self.block_positions.get(&id)?;
        self.projection_revision = self.projection_revision.saturating_add(1);
        self.blocks.get_mut(position)
    }

    fn allocate_block_id(&mut self) -> BlockId {
        let id = self.next_block_id;
        self.next_block_id = self.next_block_id.saturating_add(1);
        id
    }
}

fn resolve_tool_display(
    state: &mut ConversationView,
    name: &str,
    args: &str,
    formatter: Option<&dyn ToolDisplayFormatter>,
) -> ToolDisplay {
    formatter
        .and_then(
            |formatter| match formatter.format_tool_display(name, args) {
                Ok(display) => display,
                Err(error) => {
                    state.notify(
                        NotifyKind::Warning,
                        format!("tool display formatter failed for {name}: {error}"),
                    );
                    None
                }
            },
        )
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn subagent_headers_are_ordered_scoped_complete_and_never_expand() {
        let mut state = ConversationView::default();
        let parent = state.project_subagent_container(None, "[agent] probe".into());
        assert_eq!(
            state.block(parent).unwrap().block.status,
            phi_tui::BlockStatus::Running
        );
        let command = format!("{}\npwd", "x".repeat(300));
        assert!(state.project_subagent_tool_started(
            parent,
            "same",
            "Bash",
            &serde_json::json!({"command": command}),
        ));
        assert!(state.project_subagent_tool_started(
            parent,
            "read",
            "Read",
            &serde_json::json!({"path": "AGENTS.md", "offset": 1, "limit": 53}),
        ));
        assert!(!state.project_subagent_tool_started(
            parent,
            "same",
            "Bash",
            &serde_json::json!({"command": "duplicate"}),
        ));
        let other = state.project_subagent_container(None, "[agent] other".into());
        assert!(state.project_subagent_tool_started(
            other,
            "same",
            "Bash",
            &serde_json::json!({"command": "other"}),
        ));
        assert!(state.project_subagent_tool_finished(parent, "same", false));
        let revision = state.projection_revision;
        assert!(!state.project_subagent_tool_finished(parent, "same", false));
        assert!(!state.project_subagent_tool_finished(parent, "missing", true));
        assert!(!state.project_subagent_tool_started(
            parent,
            "same",
            "Bash",
            &serde_json::json!({})
        ));
        assert_eq!(state.projection_revision, revision);
        assert!(state.project_subagent_tool_finished(parent, "read", true));
        let children: Vec<_> = state
            .blocks
            .iter()
            .filter(|block| block.parent_id == Some(parent))
            .collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].block.title, "Bash");
        assert_eq!(
            children[0].block.input.as_deref(),
            Some(format!("{{{command}}}").as_str())
        );
        assert_eq!(children[0].block.status, phi_tui::BlockStatus::Success);
        assert!(children[1].block.title.starts_with("Read"));
        assert_eq!(children[1].block.status, phi_tui::BlockStatus::Error);
        assert!(children.iter().all(
            |block| matches!(&block.block.body, phi_tui::BlockBody::Output(text) if text.is_empty())
        ));
        assert_eq!(
            state
                .block(state.nested_tool_block(other, "same").unwrap())
                .unwrap()
                .block
                .status,
            phi_tui::BlockStatus::Running
        );
        assert!(
            children
                .iter()
                .all(|block| block.block.expansion == phi_tui::BlockExpansion::Fixed)
        );
        assert!(state.toggle_all_collapsible_blocks());
        state.project_subagent_status(
            parent,
            "[agent] probe",
            "taskId: task",
            BlockStatus::Success,
        );
        let revision = state.projection_revision;
        state.project_subagent_status(
            parent,
            "[agent] probe",
            "taskId: task",
            BlockStatus::Success,
        );
        assert_eq!(state.projection_revision, revision);
    }

    use super::super::format::tool_display_formatter;
    use super::*;

    #[test]
    fn codemode_completion_preserves_expansion_and_previews_only_text_regions() {
        let source = "const value = 1;\n".repeat(20) + "return value;";
        let args = serde_json::json!({"code": source}).to_string();
        let first = format!("first\n{}hidden-first", "a".repeat(200));
        let second = format!("second\n{}hidden-second", "界".repeat(200));
        let console = format!("console\n{}hidden-console", "c".repeat(200));
        let diagnostic = format!("Script error: {}diagnostic-end", "error ".repeat(30));
        let path = "[Full output: /tmp/phi-codemode-preview.txt]";
        for (include_console, expanded) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let console_output = if include_console {
                format!("<console_output>\n{console}\n</console_output>\n")
            } else {
                String::new()
            };
            let output = format!(
                "Script failed\nOutput:\n==> text 1/2 <==\n{first}\n==> text 2/2 <==\n{second}\n{console_output}<script_error>\n{diagnostic}\nTool effects are not undone.\n</script_error>\n{path}"
            );
            let mut state = ConversationView::default();
            let parent = state.project_tool_call_started(
                "script",
                "Codemode",
                &args,
                ToolDisplay::default(),
            );
            state
                .touch_block(parent)
                .unwrap()
                .block
                .set_expanded(expanded);
            assert_eq!(
                state.block(parent).unwrap().block.input.as_deref(),
                Some(format!("{{{source}}}").as_str())
            );
            assert!(state.project_tool_call_finished("script", &output, true, None));
            assert_eq!(state.block_is_expanded(parent), expanded);
            assert_eq!(
                state.block(parent).unwrap().block.status,
                BlockStatus::Error
            );
            let rendered = state.materialize(phi_tui::TranscriptRenderRequest {
                width: 100,
                row_budget: 256,
                projection_revision: state.projection_revision,
                theme_revision: 0,
                expansion_key_hint: Some("ctrl+o"),
                text_expansion_row_limit: None,
            });
            let text = rendered
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if expanded {
                assert!(text.contains("==> text 1/2 <=="));
                assert!(text.contains("==> text 2/2 <=="));
                assert_eq!(text.contains("<console_output>"), include_console);
                assert_eq!(text.contains("</console_output>"), include_console);
                assert!(text.contains("<script_error>"));
                assert!(text.contains("</script_error>"));
                let compact = text.split_whitespace().collect::<String>();
                assert_eq!(
                    compact.matches("...").count(),
                    if include_console { 3 } else { 2 }
                );
                for hidden in ["hidden-first", "hidden-second", "hidden-console"] {
                    assert!(!text.contains(hidden));
                }
                assert!(text.contains("diagnostic-end"));
                for segment in rendered.lines.iter().flat_map(|line| &line.segments) {
                    if segment.text().contains("...") {
                        assert_eq!(segment.color(), ThemeColor::Dim);
                    }
                }
                assert!(text.contains(path));
            } else {
                let line_count = if include_console { 17 } else { 13 };
                assert!(text.contains(&format!("+{line_count} lines")));
                assert!(!text.contains(path));
                assert!(!text.contains("<script_error>"));
                assert!(!text.contains("Script error:"));
            }
        }
    }

    #[test]
    fn nested_calls_keep_identity_status_and_visibility_under_collapsed_script() {
        let mut state = ConversationView::default();
        let parent = state.project_tool_call_started(
            "script",
            "Codemode",
            r#"{"code":"return 1;"}"#,
            ToolDisplay::default(),
        );
        let mut call = NestedToolCall {
            id: "script/1".into(),
            parent_tool_call_id: "script".into(),
            name: "Read".into(),
            status: NestedCallStatus::Running,
            arguments: Some(serde_json::json!({"path": "README.md"})),
            duration_ms: 0,
            error: None,
            details: None,
        };
        assert!(state.project_event(phi_sdk::PhiEvent::NestedToolCall { call: call.clone() }));
        let child = state.nested_tool_block(parent, &call.id).unwrap();
        assert_eq!(
            state.block(child).unwrap().block.status,
            BlockStatus::Running
        );
        assert!(!state.block_is_expanded(parent));
        let mut pending = call.clone();
        pending.id = "script/2".into();
        assert!(state.project_nested_tool_call(&pending));
        let pending_id = state.nested_tool_block(parent, &pending.id).unwrap();
        let rendered = state.materialize(phi_tui::TranscriptRenderRequest {
            width: 80,
            row_budget: 128,
            projection_revision: state.projection_revision,
            theme_revision: 0,
            expansion_key_hint: Some("ctrl+o"),
            text_expansion_row_limit: None,
        });
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.text.contains("Read {README.md}"))
        );
        call.status = NestedCallStatus::Error;
        call.error = Some("read denied".into());
        call.duration_ms = 1500;
        assert!(state.project_event(phi_sdk::PhiEvent::NestedToolCall { call: call.clone() }));
        let mut completed = pending.clone();
        completed.id = "script/3".into();
        completed.status = NestedCallStatus::Ok;
        assert!(state.project_tool_call_finished(
            "script",
            "caught error",
            false,
            Some(&ToolResultDetails {
                nested_calls: Some(phi_sdk::tools::NestedCalls {
                    calls: vec![call, completed.clone()],
                    complete: true
                }),
                ..ToolResultDetails::default()
            })
        ));
        assert_eq!(state.blocks.len(), 4);
        let completed_id = state.nested_tool_block(parent, &completed.id).unwrap();
        assert_eq!(
            state.block(completed_id).unwrap().block.status,
            BlockStatus::Success
        );
        assert_eq!(
            state.block(pending_id).unwrap().block.status,
            BlockStatus::Idle
        );
        assert!(
            state
                .block(pending_id)
                .unwrap()
                .block
                .footer
                .as_ref()
                .unwrap()
                .text
                .contains("Status unavailable")
        );
        assert_eq!(
            state.block(parent).unwrap().block.status,
            BlockStatus::Success
        );
        let block = &state.block(child).unwrap().block;
        assert_eq!(block.status, BlockStatus::Error);
        assert!(matches!(&block.body, BlockBody::Output(text) if text.is_empty()));
        assert!(block.footer.as_ref().unwrap().text.contains("Took 1.5s"));
    }

    #[test]
    fn live_and_history_keep_tool_input_separate_from_results() {
        let mut state = ConversationView::default();
        let args = r#"{"path":"README.md","offset":2,"limit":3}"#;
        let formatter = tool_display_formatter;
        let history = state.project_tool_block("Read", args, Some(&formatter));
        let display = formatter
            .format_tool_display("Read", args)
            .unwrap()
            .unwrap();
        let live = state.project_tool_call_started("read-live", "Read", args, display);
        for id in [history, live] {
            assert_eq!(state.block(id).unwrap().block.title, "Read");
            assert_eq!(
                state.block(id).unwrap().block.input.as_deref(),
                Some("{README.md:2-4}")
            );
            state.set_tool_output(id, "(100 lines, showing lines 2-3)\ntext");
            assert_eq!(state.block(id).unwrap().block.title, "Read");
            assert_eq!(
                state.block(id).unwrap().block.input.as_deref(),
                Some("{README.md:2-3}")
            );
        }
    }

    #[test]
    fn read_ranges_preserve_custom_input_and_colons_in_paths() {
        let mut state = ConversationView::default();
        let output = "(100 lines, showing lines 2-3)\ntext";
        for input in ["custom summary", "{custom summary}"] {
            let display = ToolDisplay {
                title: Some("Read".into()),
                input: Some(input.into()),
                ..ToolDisplay::default()
            };
            let id =
                state.project_tool_call_started(input, "Read", r#"{"path":"README.md"}"#, display);
            state.set_tool_output(id, output);
            assert_eq!(state.block(id).unwrap().block.input.as_deref(), Some(input));
        }
        let id = state.project_tool_block("Read", r#"{"path":"notes:1-4"}"#, None);
        state.set_tool_output(id, output);
        assert_eq!(
            state.block(id).unwrap().block.input.as_deref(),
            Some("{notes:1-4:2-3}")
        );
    }

    #[test]
    fn sdk_title_suffix_is_projected_in_live_and_history_views() {
        let mut state = ConversationView::default();
        let args = r#"{"command":"sleep 1","timeout":20}"#;
        let mut display = format_tool_display("Bash", args);
        assert_eq!(display.title_suffix.as_deref(), Some(" (timeout 20s)"));
        display.title_suffix = Some(" (custom metadata)".into());
        let formatter = |_: &str, _: &str| Ok(Some(display.clone()));
        let history = state.project_tool_block("Bash", args, Some(&formatter));
        let live = state.project_tool_call_started("bash-live", "Bash", args, display.clone());
        for id in [history, live] {
            assert_eq!(
                state.block(id).unwrap().block.title_suffix.as_deref(),
                Some(" (custom metadata)")
            );
        }
    }

    #[test]
    fn custom_displays_control_input_without_parsing_titles() {
        let mut state = ConversationView::default();
        for (display, expected_input) in [
            (ToolDisplay::title("custom label {not input}"), None),
            (
                ToolDisplay {
                    title: Some("custom label".into()),
                    input: Some("custom input".into()),
                    title_suffix: Some(" (metadata)".into()),
                    content: Some("result prefix".into()),
                    ..ToolDisplay::default()
                },
                Some("custom input"),
            ),
        ] {
            let title = display.title.clone().unwrap();
            let id = state.project_tool_call_started(&title, "custom", "{}", display);
            state.set_tool_output(id, "result");
            let block = &state.block(id).unwrap().block;
            assert_eq!(block.title, title);
            assert_eq!(block.input.as_deref(), expected_input);
            if expected_input.is_some() {
                assert_eq!(block.title_suffix.as_deref(), Some(" (metadata)"));
                assert!(
                    matches!(&block.body, BlockBody::Output(text) if text == "result prefix\nresult")
                );
            }
        }
    }

    #[test]
    fn mcp_projection_overrides_generic_tool_title() {
        let mut state = ConversationView::default();
        state.tool_display_name_projection.insert(
            "exa_web_search_exa".to_string(),
            "exa: web_search_exa".to_string(),
        );
        let formatter = tool_display_formatter;

        let id = state.project_tool_block("exa_web_search_exa", "{}", Some(&formatter));

        assert_eq!(
            state.block(id).unwrap().block.title,
            "MCP exa: web_search_exa"
        );
        assert_eq!(state.block(id).unwrap().block.input.as_deref(), Some("{}"));
        state.install_tool_titles(std::collections::HashMap::from([(
            "exa_web_search_exa".into(),
            "exa: renamed_search".into(),
        )]));
        assert_eq!(
            state.block(id).unwrap().block.title,
            "MCP exa: renamed_search"
        );
        assert_eq!(state.block(id).unwrap().block.input.as_deref(), Some("{}"));
        state.install_tool_titles(std::collections::HashMap::new());
        assert_eq!(state.block(id).unwrap().block.title, "exa_web_search_exa");
        assert_eq!(state.block(id).unwrap().block.input.as_deref(), Some("{}"));
    }

    #[test]
    fn bash_timing_updates_and_freezes_without_timing_other_tools() {
        let mut state = live_run_state();
        let bash = project_live_tool(&mut state, "Bash", r#"{"command":"sleep 1","timeout":20}"#);
        let read = project_live_tool(&mut state, "Read", r#"{"path":"README.md"}"#);

        assert_eq!(
            state.block(bash).unwrap().block.title_suffix.as_deref(),
            Some(" (timeout 20s)")
        );
        assert_eq!(tool_footer(&state, bash).text.trim(), "Elapsed 0.0s");
        assert_eq!(tool_footer(&state, bash).fg, ThemeColor::Dim);
        assert!(state.tool_block_meta[&bash].tool_timer.is_some());
        assert!(state.block(read).unwrap().block.footer.is_none());
        assert!(state.tool_block_meta[&read].tool_timer.is_none());
        let started = state.tool_block_meta[&bash].tool_timer.unwrap().started_at;
        assert!(!state.refresh_tool_timing(started));
        let observed = started + std::time::Duration::from_millis(1_100);
        assert!(state.refresh_tool_timing(observed));
        assert_eq!(tool_footer(&state, bash).text.trim(), "Elapsed 1.1s");
        assert!(!state.refresh_tool_timing(observed));
        state.finish_tool_timing_at(bash, observed);
        assert_eq!(tool_footer(&state, bash).text.trim(), "Took 1.1s");
        assert!(!state.refresh_tool_timing(observed + std::time::Duration::from_secs(60)));
        assert_eq!(tool_footer(&state, bash).text.trim(), "Took 1.1s");
    }

    #[test]
    fn live_tool_results_match_their_view_and_call() {
        let mut main_view = ConversationView::default();
        let mut child_view = ConversationView::default();
        let main =
            main_view.project_tool_call_started("same", "Read", "{}", ToolDisplay::default());
        let child =
            child_view.project_tool_call_started("same", "Read", "{}", ToolDisplay::default());
        assert!(main_view.live_run.is_some());
        assert!(!main_view.run_active_projection());
        assert!(!child_view.run_active_projection());
        assert_eq!(
            main_view.project_tool_call_started("same", "Read", "{}", ToolDisplay::default()),
            main
        );
        assert!(!main_view.project_tool_call_finished("missing", "wrong", false, None));
        assert!(child_view.project_tool_call_finished("same", "child result", false, None));
        assert!(matches!(&main_view.block(main).unwrap().block.body,
            BlockBody::Output(text) if text.is_empty()));
        assert!(main_view.project_tool_call_finished("same", "main result", false, None));
        assert!(!main_view.project_tool_call_finished("same", "duplicate", false, None));
        assert!(matches!(&main_view.block(main).unwrap().block.body,
            BlockBody::Output(text) if text == "main result"));
        assert!(matches!(&child_view.block(child).unwrap().block.body,
            BlockBody::Output(text) if text == "child result"));
    }

    fn live_run_state() -> ConversationView {
        let mut state = ConversationView::default();
        state.start_live_run();
        state
    }

    fn project_live_tool(state: &mut ConversationView, name: &str, args: &str) -> BlockId {
        state.project_tool_call_started(&format!("{name}-call"), name, args, ToolDisplay::default())
    }

    fn tool_footer(state: &ConversationView, id: BlockId) -> &StyledLine {
        state.block(id).unwrap().block.footer.as_ref().unwrap()
    }

    #[test]
    fn expansion_intent_applies_only_to_retained_and_future_blocks() {
        let mut state = ConversationView::default();
        let retired = state.project_tool_block("Read", "{}", None);
        let retained = state.project_tool_block("Read", "{}", None);
        state.buffer_first_block = Some(retained);

        assert!(state.toggle_all_collapsible_blocks());
        state.set_tool_output(retained, "result");
        let future = state.project_tool_block("Read", "{}", None);
        assert!(!state.block_is_expanded(retired));
        assert!(state.block_is_expanded(retained));
        assert!(state.block_is_expanded(future));

        assert!(state.toggle_all_collapsible_blocks());
        state.set_tool_diff(retained, DiffView::from_prefixed_text("+changed"));
        assert!(!state.block_is_expanded(retained));
        assert!(!state.block_is_expanded(future));
    }

    #[test]
    fn retires_only_complete_unpinned_groups_before_hot_tail() {
        let mut state = ConversationView::default();
        let ids = (0..7)
            .map(|index| state.push_text_block(BlockRole::Event, index.to_string()))
            .collect::<Vec<_>>();
        state.set_block_group(ids[4], ids[3]);

        state.retire_transcript_before(ids[4]);

        assert_eq!(state.blocks[0].id, ids[3]);
        assert_eq!(state.block_positions.get(&ids[3]), Some(&0));
        assert!(!state.block_positions.contains_key(&ids[2]));
    }

    #[test]
    fn live_route_pins_older_projection() {
        let mut state = ConversationView::default();
        state.start_live_run();
        let live = state.append_live_assistant("running").unwrap();
        let tail = (0..5)
            .map(|index| state.push_text_block(BlockRole::Event, index.to_string()))
            .collect::<Vec<_>>();

        state.retire_transcript_before(tail[3]);

        assert_eq!(state.blocks[0].id, live);
    }

    #[test]
    fn text_projection_shares_committed_text_and_appends_live_text_in_place() {
        let mut state = ConversationView::default();
        let committed = phi_sdk::transcript::SharedText::new("committed");
        let expected = committed.shared();
        let committed_id = state.push_shared_text_block(BlockRole::Assistant, &committed);
        let BlockBody::Text(BlockText::Shared(actual)) =
            &state.block(committed_id).unwrap().block.body
        else {
            panic!("shared text block missing");
        };
        assert!(std::sync::Arc::ptr_eq(&actual, &expected));

        state.start_live_run();
        let live_id = state.append_live_assistant("hello").unwrap();
        state.append_live_assistant(" world");
        assert!(matches!(
            &state.block(live_id).unwrap().block.body,
            BlockBody::Text(text) if text == "hello world"
        ));
    }
}
