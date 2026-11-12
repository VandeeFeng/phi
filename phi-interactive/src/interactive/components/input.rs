use std::time::{Duration, Instant};

use phi_tui::{
    Cursor, Editor, EditorHistoryEntry, InputContext, InputOutcome, KeyBindingResolver, KeyInput,
    MountOptions, NotifyKind, PickerAction, RenderContext, RuntimeEvent, SelectItem, SelectMenu,
    StyledLine, Theme, ThemeColor, TuiRuntime, Widget, WidgetHandle, text::pad,
};

use super::super::state::{PickerKind, PickerState, UiState};
use super::mentions::{complete_mention, file_mention_context};
use super::picker::session::{ResumePickerAction, handle_resume_picker_input};
use crate::commands::{SlashCommand, SlashCommandSource};
use crate::interactive::completion::{fuzzy_match, matching_arguments, matching_commands};
use crate::runtime::attachment::AgentCatalog;

#[derive(Debug, Clone, Default)]
pub(crate) struct ArgumentHintState {
    input: String,
    selected: Option<usize>,
    dismissed: bool,
}

impl ArgumentHintState {
    fn refresh(&mut self, input: &str) {
        if self.input != input {
            self.input = input.into();
            self.selected = None;
            self.dismissed = false;
        }
    }

    fn selection(&self, input: &str) -> Option<usize> {
        (self.input == input).then_some(self.selected).flatten()
    }

    fn is_dismissed(&self, input: &str) -> bool {
        self.input == input && self.dismissed
    }

    fn cycle(&mut self, input: &str, count: usize) {
        let selected = (self.selection(input).unwrap_or(0) + 1) % count;
        self.input = input.into();
        self.selected = Some(selected);
        self.dismissed = false;
    }

    fn dismiss(&mut self, input: &str) {
        self.input = input.into();
        self.selected = None;
        self.dismissed = true;
    }
}

struct ArgumentHint<'a> {
    start: usize,
    query: &'a str,
    usage: &'a str,
    values: Vec<&'a str>,
}

fn argument_hint<'a>(input: &'a str, commands: &'a [SlashCommand]) -> Option<ArgumentHint<'a>> {
    if input.contains(['\n', '\r']) {
        return None;
    }
    let (name, arguments) = crate::commands::slash_command_parts(input)?;
    let command = commands.iter().find(|command| command.name == name)?;
    let specification = command.arguments.as_ref()?;
    let query = arguments.trim_start();
    let start = input.len() - query.len();
    if query.contains(char::is_whitespace) {
        return None;
    }
    if !query.is_empty()
        && specification
            .values
            .iter()
            .any(|argument| argument.value == query)
    {
        return None;
    }
    let values = matching_arguments(&specification.values, query);
    if !query.is_empty() && values.is_empty() {
        return None;
    }
    Some(ArgumentHint {
        start,
        query,
        usage: &specification.usage,
        values: values
            .into_iter()
            .map(|argument| argument.value.as_str())
            .collect(),
    })
}

impl ArgumentHint<'_> {
    fn text(&self, selected: Option<usize>) -> String {
        if self.values.is_empty() {
            self.usage.to_string()
        } else {
            let text = hint_text(&self.values, selected);
            if self.query.is_empty() {
                text
            } else if self.values.len() > 1 && selected.is_none() {
                format!(" {text}")
            } else {
                text.strip_prefix(self.query)
                    .map(String::from)
                    .unwrap_or_else(|| format!(" [{text}]"))
            }
        }
    }
}

fn hint_text(values: &[&str], selected: Option<usize>) -> String {
    match values {
        [] => String::new(),
        [value] => (*value).into(),
        _ => match selected {
            Some(selected) => values[selected % values.len()].into(),
            None => format!("[{}]", values.join(" | ")),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryPrompt {
    pub user_prompt: String,
    pub injected_context: Vec<phi_sdk::turn::InjectedContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSurfaceKind {
    Editor,
    SlashMenu,
    FileMentions,
    Picker,
    Subagent,
}

#[derive(Debug, Clone)]
pub(crate) struct InputSurface {
    kind: InputSurfaceKind,
    bound_keys: Vec<KeyInput>,
}

impl Widget for InputSurface {
    type Action = UiAction;

    fn render(&self, _context: &RenderContext<'_>) -> Vec<StyledLine> {
        Vec::new()
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        if self.bound_keys.contains(key) {
            return InputOutcome::Ignored;
        }
        InputOutcome::Action(UiAction::SurfaceKey {
            surface: self.kind,
            key: key.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputSurfaceHandle(WidgetHandle<InputSurface>);

impl InputSurfaceHandle {
    pub(crate) fn activate(
        self,
        runtime: &mut TuiRuntime<UiAction>,
        kind: InputSurfaceKind,
        context: InputContext,
    ) {
        let bound_keys: Vec<_> = runtime
            .key_bindings()
            .bindings()
            .iter()
            .filter(|binding| {
                binding.context == context
                    || matches!(kind, InputSurfaceKind::Editor | InputSurfaceKind::SlashMenu)
                        && binding.context == InputContext::Global
            })
            .map(|binding| binding.key.clone())
            .collect();
        if runtime
            .widget(self.0)
            .is_some_and(|surface| surface.kind != kind || surface.bound_keys != bound_keys)
        {
            runtime.update_widget(self.0, |surface| {
                surface.kind = kind;
                surface.bound_keys = bound_keys;
            });
        }
        runtime.set_widget_input_context(self.0, context);
        runtime.focus_widget(self.0);
    }
}

pub(crate) fn key_bindings() -> KeyBindingResolver<UiAction> {
    KeyBindingResolver::new()
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('b'),
            UiAction::BackgroundRunningBash,
        )
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('t'),
            UiAction::ToggleTasks,
        )
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('o'),
            UiAction::ToggleAllBlocksExpanded,
        )
        .bind(
            InputContext::Global,
            KeyInput::CtrlUp,
            UiAction::CycleSwitch(-1),
        )
        .bind(
            InputContext::Global,
            KeyInput::CtrlDown,
            UiAction::CycleSwitch(1),
        )
        .bind(
            InputContext::Global,
            KeyInput::CtrlLeft,
            UiAction::AdjustThinkingLevel(-1),
        )
        .bind(
            InputContext::Global,
            KeyInput::CtrlRight,
            UiAction::AdjustThinkingLevel(1),
        )
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('y'),
            UiAction::PermissionAllowOnce,
        )
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('a'),
            UiAction::PermissionAllowAlways,
        )
        .bind(
            InputContext::Global,
            KeyInput::Ctrl('d'),
            UiAction::PermissionDeny,
        )
}

pub(crate) fn mount_input_surface(runtime: &mut TuiRuntime<UiAction>) -> InputSurfaceHandle {
    InputSurfaceHandle(runtime.mount_widget(
        InputSurface {
            kind: InputSurfaceKind::Editor,
            bound_keys: Vec::new(),
        },
        MountOptions::input(InputContext::Editor).focused(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiAction {
    None,
    SurfaceKey {
        surface: InputSurfaceKind,
        key: KeyInput,
    },
    Resize,
    RunExtensionKeybinding(phi_sdk::extension::ExtensionKeybindingId),
    Submit {
        query_prompt: QueryPrompt,
        display: String,
        history_entry: EditorHistoryEntry,
        dispatch: phi_sdk::turn::SubmissionDispatch,
    },
    Pick(String),
    DeletePick(String),
    CycleSwitch(i32),
    AdjustThinkingLevel(i32),
    CycleSwitchField {
        value: String,
        direction: i32,
    },
    SetSwitchAlias {
        switch_name: String,
        alias: String,
    },
    // Nested switch pickers stage edits; only a terminal menu exit emits this save.
    SaveSwitchConfig,
    ApplySwitchConfig(String),
    ToggleAllBlocksExpanded,
    ToggleTasks,
    BackgroundRunningBash,
    CloseBashSelector,
    BackgroundBash {
        owner: String,
        id: String,
    },
    StopBash {
        owner: String,
        id: String,
    },
    OpenBashOutput {
        owner: String,
        id: String,
    },
    OpenTaskOwner(String),
    PermissionAllowOnce,
    PermissionAllowAlways,
    PermissionDeny,
    McpElicitationSubmit(Option<serde_json::Value>),
    McpElicitationDecline,
    McpElicitationCancel,
    McpElicitationOpenUrl(String),
    CancelRun,
    CancelSubagent(String),
    EnterSubagent(String),
    OpenMemoryAnchor(String),
    CloseActiveOverlay,
    Quit,
}

pub(crate) fn poll_action(
    state: &mut UiState,
    last_ctrl_c: &mut Option<Instant>,
    agents: &AgentCatalog,
    timeout: Duration,
) -> anyhow::Result<UiAction> {
    let key = match state.runtime.poll_event_with_timeout(timeout)? {
        RuntimeEvent::Idle => return Ok(UiAction::None),
        RuntimeEvent::Resize => return Ok(UiAction::Resize),
        RuntimeEvent::TerminalFocus(_) => return Ok(UiAction::None),
        RuntimeEvent::Key(key) => key,
    };
    sync_input_surface(state);
    match state.runtime.dispatch_key(key) {
        InputOutcome::Action(UiAction::SurfaceKey { surface, key }) => {
            route_surface_key(state, surface, key, last_ctrl_c, agents)
        }
        InputOutcome::Action(action) => Ok(action),
        InputOutcome::Consumed | InputOutcome::Ignored => Ok(UiAction::None),
    }
}

fn sync_input_surface(state: &mut UiState) {
    if state
        .active_overlay
        .is_some_and(|handle| state.runtime.is_widget_visible(handle))
    {
        return;
    }
    if state.interaction.is_some() {
        state.activate_interaction();
        return;
    }
    if let Some(handle) = state.bash_selector {
        state.runtime.focus_widget(handle);
        return;
    }
    let kind = if state.picker.is_some() {
        InputSurfaceKind::Picker
    } else if state.file_mentions.is_open() {
        InputSurfaceKind::FileMentions
    } else if slash_menu_open(state) {
        InputSurfaceKind::SlashMenu
    } else {
        InputSurfaceKind::Editor
    };
    let context = surface_context(state, kind);
    state
        .input_surface
        .activate(&mut state.runtime, kind, context);
}

pub(crate) fn surface_context(state: &UiState, surface: InputSurfaceKind) -> InputContext {
    match surface {
        InputSurfaceKind::Editor => InputContext::Editor,
        InputSurfaceKind::SlashMenu => InputContext::custom("slash_menu"),
        InputSurfaceKind::FileMentions => InputContext::custom("file_mentions"),
        InputSurfaceKind::Picker => state
            .picker
            .as_ref()
            .map(|picker| picker_context(&picker.kind))
            .unwrap_or(InputContext::Picker),
        InputSurfaceKind::Subagent => InputContext::Transcript,
    }
}

fn picker_context(kind: &PickerKind) -> InputContext {
    match kind {
        PickerKind::Generic => InputContext::custom("picker.generic"),
        PickerKind::Login => InputContext::custom("picker.login"),
        PickerKind::Model => InputContext::custom("picker.model"),
        PickerKind::Resume => InputContext::custom("picker.resume"),
        PickerKind::Rewind => InputContext::custom("picker.rewind"),
        PickerKind::SwitchSelect | PickerKind::RuntimeSwitch => {
            InputContext::custom("picker.switch_select")
        }
        PickerKind::SwitchConfigure => InputContext::custom("picker.switch_configure"),
        PickerKind::SwitchEdit { .. } => InputContext::custom("picker.switch_edit"),
        PickerKind::SwitchName => InputContext::custom("picker.switch_name"),
        PickerKind::SwitchModel { .. } => InputContext::custom("picker.switch_model"),
    }
}

pub(crate) fn parse_input_context(context: &str) -> Option<InputContext> {
    Some(match context {
        "editor" => InputContext::Editor,
        "slash_menu" => InputContext::custom("slash_menu"),
        "file_mentions" => InputContext::custom("file_mentions"),
        "picker.generic" => InputContext::custom("picker.generic"),
        "picker.login" => InputContext::custom("picker.login"),
        "picker.model" => InputContext::custom("picker.model"),
        "picker.resume" => InputContext::custom("picker.resume"),
        "picker.rewind" => InputContext::custom("picker.rewind"),
        "picker.switch_select" => InputContext::custom("picker.switch_select"),
        "picker.switch_configure" => InputContext::custom("picker.switch_configure"),
        "picker.switch_edit" => InputContext::custom("picker.switch_edit"),
        "picker.switch_name" => InputContext::custom("picker.switch_name"),
        "picker.switch_model" => InputContext::custom("picker.switch_model"),
        "confirm" => InputContext::Confirm,
        _ => return None,
    })
}

fn route_surface_key(
    state: &mut UiState,
    surface: InputSurfaceKind,
    key: KeyInput,
    last_ctrl_c: &mut Option<Instant>,
    agents: &AgentCatalog,
) -> anyhow::Result<UiAction> {
    state.runtime.request_interactive_render();
    match surface {
        InputSurfaceKind::Picker => Ok(handle_picker_key(state, key)),
        InputSurfaceKind::FileMentions => {
            if let Some(action) = handle_clear_text_key(state, &key, last_ctrl_c, agents) {
                return Ok(action);
            }
            route_file_mentions(state, key, agents).map(outcome_action)
        }
        InputSurfaceKind::SlashMenu => {
            if let Some(action) = handle_clear_text_key(state, &key, last_ctrl_c, agents)
                .or_else(|| handle_global_key(state, &key, last_ctrl_c))
            {
                return Ok(action);
            }
            route_slash_menu(state, key, agents).map(outcome_action)
        }
        InputSurfaceKind::Editor => {
            if let Some(action) =
                handle_clear_text_key(state, &key, last_ctrl_c, agents).or_else(|| {
                    if input_hint(state).is_some() {
                        None
                    } else {
                        handle_global_key(state, &key, last_ctrl_c)
                    }
                })
            {
                return Ok(action);
            }
            route_editor(state, key, agents).map(outcome_action)
        }
        InputSurfaceKind::Subagent => Ok(UiAction::None),
    }
}

fn outcome_action(outcome: InputOutcome<UiAction>) -> UiAction {
    match outcome {
        InputOutcome::Action(action) => action,
        InputOutcome::Consumed | InputOutcome::Ignored => UiAction::None,
    }
}

fn route_file_mentions(
    state: &mut UiState,
    key: KeyInput,
    agents: &AgentCatalog,
) -> anyhow::Result<InputOutcome<UiAction>> {
    if state.file_mentions.is_open() {
        return Ok(InputOutcome::Action(handle_file_mention_key(
            state, key, agents,
        )?));
    }
    Ok(InputOutcome::Ignored)
}

fn route_slash_menu(
    state: &mut UiState,
    key: KeyInput,
    agents: &AgentCatalog,
) -> anyhow::Result<InputOutcome<UiAction>> {
    if !slash_menu_open(state) {
        return Ok(InputOutcome::Ignored);
    }
    match key {
        KeyInput::Up => state.slash_menu.move_up(slash_matches(state).len()),
        KeyInput::Down => state.slash_menu.move_down(slash_matches(state).len()),
        KeyInput::Tab => apply_slash_command(state),
        KeyInput::Enter | KeyInput::AltEnter => {
            let dispatch = dispatch_for_key(state, &key);
            apply_slash_command(state);
            return submit(state, agents, dispatch).map(InputOutcome::Action);
        }
        KeyInput::Esc | KeyInput::Ctrl('c') => state.slash_menu.close(),
        _ => handle_text_key(state, key, agents),
    }
    clamp_slash_selection(state);
    Ok(InputOutcome::Action(UiAction::None))
}

fn route_editor(
    state: &mut UiState,
    key: KeyInput,
    agents: &AgentCatalog,
) -> anyhow::Result<InputOutcome<UiAction>> {
    if let Some(hint) = input_hint(state) {
        match key {
            KeyInput::Tab if !hint.values.is_empty() && hint.query.is_empty() => {
                state
                    .argument_hint
                    .cycle(state.editor.text(), hint.values.len());
                return Ok(InputOutcome::Consumed);
            }
            KeyInput::Tab | KeyInput::Enter
                if !hint.values.is_empty()
                    && (key == KeyInput::Tab
                        || state.argument_hint.selection(state.editor.text()).is_some()
                        || !hint.query.is_empty()) =>
            {
                let selected = state
                    .argument_hint
                    .selection(state.editor.text())
                    .unwrap_or(0)
                    % hint.values.len();
                let separator = if hint.start == state.editor.text().len()
                    && !state.editor.text().ends_with(char::is_whitespace)
                {
                    " "
                } else {
                    ""
                };
                let replacement = format!("{separator}{} ", hint.values[selected]);
                let start = hint.start;
                let end = state.editor.text().len();
                state.editor.replace_range(start..end, &replacement);
                update_completion_menus(state, agents);
                return Ok(InputOutcome::Consumed);
            }
            KeyInput::Esc => {
                state.argument_hint.dismiss(state.editor.text());
                state.file_mentions.close();
                return Ok(InputOutcome::Consumed);
            }
            _ => {}
        }
    }
    if state.file_mentions.has_mode_hint(state.editor.text()) {
        state.file_mentions.close();
    }
    if key.inserts_editor_newline() {
        state.editor.insert("\n");
        update_completion_menus(state, agents);
        return Ok(InputOutcome::Action(UiAction::None));
    }
    if matches!(key, KeyInput::Enter | KeyInput::AltEnter) {
        if key == KeyInput::Enter && state.editor.pop_if('\\') {
            state.editor.insert("\n");
            return Ok(InputOutcome::Action(UiAction::None));
        }
        let dispatch = dispatch_for_key(state, &key);
        return submit(state, agents, dispatch).map(InputOutcome::Action);
    }
    match key {
        KeyInput::Up => {
            state.editor.move_up();
            update_completion_menus(state, agents);
        }
        KeyInput::Down => {
            state.editor.move_down();
            update_completion_menus(state, agents);
        }
        _ => handle_text_key(state, key, agents),
    }
    clamp_slash_selection(state);
    Ok(InputOutcome::Action(UiAction::None))
}

fn handle_clear_text_key(
    state: &mut UiState,
    key: &KeyInput,
    last_ctrl_c: &mut Option<Instant>,
    agents: &AgentCatalog,
) -> Option<UiAction> {
    if !matches!(key, KeyInput::Ctrl('c')) || state.editor.text().is_empty() {
        return None;
    }
    state.editor.clear();
    *last_ctrl_c = None;
    state.slash_menu.open();
    state.file_mentions.close();
    update_completion_menus(state, agents);
    Some(UiAction::None)
}

fn handle_global_key(
    state: &mut UiState,
    key: &KeyInput,
    last_ctrl_c: &mut Option<Instant>,
) -> Option<UiAction> {
    Some(match key {
        KeyInput::Esc | KeyInput::Ctrl('c') if state.conversation.run_status_view().active => {
            UiAction::CancelRun
        }
        KeyInput::Ctrl('c') => handle_ctrl_c(state, last_ctrl_c),
        _ => return None,
    })
}

fn handle_ctrl_c(state: &mut UiState, last_ctrl_c: &mut Option<Instant>) -> UiAction {
    let now = Instant::now();
    let quit =
        last_ctrl_c.is_some_and(|last| now.duration_since(last) < Duration::from_millis(900));
    *last_ctrl_c = Some(now);

    if quit {
        UiAction::Quit
    } else {
        state.editor.clear();
        state.slash_menu.open();
        UiAction::None
    }
}

fn handle_text_key(state: &mut UiState, key: KeyInput, agents: &AgentCatalog) {
    match key {
        key if key.inserts_editor_newline() => {
            state.editor.insert("\n");
            update_completion_menus(state, agents);
        }
        KeyInput::Up => state.editor.move_cursor_up(),
        KeyInput::Down => state.editor.move_cursor_down(),
        _ => {
            state.editor.handle_input(&key);
            if matches!(
                key,
                KeyInput::Char(_) | KeyInput::Paste(_) | KeyInput::Backspace
            ) {
                update_completion_menus(state, agents);
            }
        }
    }
}

fn handle_file_mention_key(
    state: &mut UiState,
    key: KeyInput,
    agents: &AgentCatalog,
) -> anyhow::Result<UiAction> {
    match key {
        KeyInput::Up => state
            .file_mentions
            .menu
            .move_up(state.file_mentions.items().len()),
        KeyInput::Down => state
            .file_mentions
            .menu
            .move_down(state.file_mentions.items().len()),
        KeyInput::Tab | KeyInput::Enter => apply_file_mention(state, key),
        KeyInput::Esc | KeyInput::Ctrl('c') => state.file_mentions.close(),
        _ => handle_text_key(state, key, agents),
    }
    Ok(UiAction::None)
}

fn handle_picker_key(state: &mut UiState, key: KeyInput) -> UiAction {
    if key == KeyInput::Ctrl('c')
        && state.active_menu == Some(crate::interactive::state::MenuKind::Switch)
    {
        return exit_switch_menu(state);
    }
    if state.naming_switch {
        return handle_switch_name_key(state, key);
    }

    if let Some(picker) = state
        .picker
        .as_mut()
        .filter(|picker| matches!(picker.kind, PickerKind::Resume))
    {
        if !state.features.sessions.deletion {
            state.picker_confirming_delete = None;
            if key == KeyInput::Ctrl('d') {
                return UiAction::None;
            }
        }
        let action = handle_resume_picker_input(picker, &mut state.picker_confirming_delete, &key);
        return apply_resume_picker_action(state, action);
    }

    if let Some(action) = handle_delete_confirmation(state, &key) {
        return action;
    }
    if let Some(action) = edit_selected_switch_alias(state, &key) {
        return action;
    }

    match key {
        KeyInput::Left => return cycle_selected_switch_field(state, -1),
        KeyInput::Right => return cycle_selected_switch_field(state, 1),
        _ => {}
    }

    let Some(picker) = &mut state.picker else {
        return UiAction::None;
    };
    if handle_rewind_search_key(picker, &key) {
        return UiAction::None;
    }
    match picker.handle_input_with_filter(&key, filter_items) {
        InputOutcome::Action(PickerAction::Cancelled) => cancel_picker(state),
        InputOutcome::Action(PickerAction::Selected(value)) => pick_value(state, value),
        InputOutcome::Consumed | InputOutcome::Ignored => UiAction::None,
    }
}

fn apply_resume_picker_action(state: &mut UiState, action: ResumePickerAction) -> UiAction {
    match action {
        ResumePickerAction::None => UiAction::None,
        ResumePickerAction::Selected(value) => pick_value(state, value),
        ResumePickerAction::Cancelled => cancel_picker(state),
        ResumePickerAction::Delete(value) => {
            state.close_picker();
            UiAction::DeletePick(value)
        }
    }
}

fn handle_rewind_search_key(picker: &mut PickerState, key: &KeyInput) -> bool {
    if !matches!(picker.kind, PickerKind::Rewind) {
        return false;
    }
    match key {
        KeyInput::Char('/') if !picker.filterable => {
            picker.filterable = true;
            true
        }
        KeyInput::Esc if picker.filterable => {
            picker.query.clear();
            picker.filter_with(filter_items);
            picker.filterable = false;
            true
        }
        _ => false,
    }
}

fn filter_items(items: &[SelectItem], query: &str) -> Vec<SelectItem> {
    if query.trim().is_empty() {
        return items.to_vec();
    }
    let mut matches = items
        .iter()
        .filter_map(|item| {
            let haystack = format!("{} {}", item.label, item.description);
            fuzzy_match(&haystack.to_lowercase(), &query.to_lowercase())
                .map(|gaps| (gaps, item.label.len(), item.clone()))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(gaps, len, _)| (*gaps, *len));
    matches.into_iter().map(|(_, _, item)| item).collect()
}

fn handle_switch_name_key(state: &mut UiState, key: KeyInput) -> UiAction {
    let kind = state.picker.as_ref().map(|picker| picker.kind.clone());
    match key {
        KeyInput::Enter => {
            let value = state.editor.text().trim().to_string();
            if value.is_empty() && matches!(kind, Some(PickerKind::SwitchName)) {
                state.notify(NotifyKind::Warning, "switch name is empty");
                sync_switch_text_picker(state);
                return UiAction::None;
            }
            state.editor.clear();
            state.naming_switch = false;
            match kind {
                Some(PickerKind::SwitchName) => UiAction::Pick(format!("switch:add:{value}")),
                _ => UiAction::None,
            }
        }
        KeyInput::Esc => {
            state.editor.clear();
            state.naming_switch = false;
            state.close_picker();
            UiAction::Pick(String::from("switch:configure"))
        }
        _ => {
            handle_text_key(state, key, &AgentCatalog::default());
            sync_switch_text_picker(state);
            UiAction::None
        }
    }
}

fn sync_switch_text_picker(state: &mut UiState) {
    if let Some(picker) = &mut state.picker {
        picker.set_items(vec![SelectItem::with_label(
            "switch:name",
            format!("name: {}", state.editor.text()),
            "",
        )]);
    }
}

fn edit_selected_switch_alias(state: &mut UiState, key: &KeyInput) -> Option<UiAction> {
    let (switch_name, mut alias) = {
        let picker = state.picker.as_ref()?;
        if !matches!(picker.kind, PickerKind::SwitchEdit { .. }) {
            return None;
        }
        let item = picker.selected_item()?;
        let switch_name = item.value.strip_prefix("switch:alias:")?.to_string();
        (switch_name, item.description.clone())
    };
    match key {
        KeyInput::Char(character) => alias.push(*character),
        KeyInput::Paste(text) => alias.extend(text.chars().filter(|ch| !matches!(ch, '\r' | '\n'))),
        KeyInput::Backspace => {
            phi_tui::pop_grapheme(&mut alias);
        }
        KeyInput::Enter => {
            return Some(UiAction::None);
        }
        _ => return None,
    }
    Some(UiAction::SetSwitchAlias { switch_name, alias })
}

fn handle_delete_confirmation(state: &mut UiState, key: &KeyInput) -> Option<UiAction> {
    let value = state.picker_confirming_delete.clone()?;
    match key {
        KeyInput::Enter => {
            state.close_picker();
            Some(UiAction::DeletePick(value))
        }
        KeyInput::Esc | KeyInput::Ctrl('c') => {
            state.picker_confirming_delete = None;
            if let Some(picker) = &mut state.picker {
                picker.title = String::from("configure switch");
            }
            Some(UiAction::None)
        }
        _ => Some(UiAction::None),
    }
}

fn cancel_picker(state: &mut UiState) -> UiAction {
    let Some(picker) = &state.picker else {
        return UiAction::None;
    };
    let action = match &picker.kind {
        PickerKind::SwitchModel { switch_name } => {
            UiAction::Pick(format!("switch:edit:{switch_name}"))
        }
        PickerKind::SwitchEdit { switch_name } => {
            UiAction::Pick(format!("switch:back:configure:{switch_name}"))
        }
        PickerKind::SwitchConfigure => UiAction::Pick(String::from("switch:back")),
        PickerKind::SwitchSelect => UiAction::Pick(String::from("switch:sync")),
        _ => UiAction::None,
    };
    state.close_picker();
    action
}

fn exit_switch_menu(state: &mut UiState) -> UiAction {
    let action = match state.picker.as_ref().map(|picker| &picker.kind) {
        Some(PickerKind::SwitchEdit { switch_name } | PickerKind::SwitchModel { switch_name }) => {
            UiAction::ApplySwitchConfig(switch_name.clone())
        }
        _ => UiAction::SaveSwitchConfig,
    };
    state.editor.clear();
    state.naming_switch = false;
    state.close_picker();
    action
}

fn pick_value(state: &mut UiState, value: String) -> UiAction {
    if value.starts_with("switch:alias:") {
        return UiAction::None;
    }
    if value.starts_with("switch:set:") && !value.ends_with(":model") {
        return UiAction::None;
    }
    if !value.starts_with("switch:delete:") {
        state.close_picker();
    }
    UiAction::Pick(value)
}

fn cycle_selected_switch_field(state: &mut UiState, direction: i32) -> UiAction {
    let Some(picker) = &state.picker else {
        return UiAction::None;
    };
    let Some(item) = picker.selected_item() else {
        return UiAction::None;
    };
    if !item.value.starts_with("switch:set:") || item.value.ends_with(":model") {
        return UiAction::None;
    }
    UiAction::CycleSwitchField {
        value: item.value.clone(),
        direction,
    }
}

fn input_hint(state: &UiState) -> Option<ArgumentHint<'_>> {
    let input = state.editor.text();
    if state.editor.cursor() != input.len() || state.argument_hint.is_dismissed(input) {
        return None;
    }
    if state.file_mentions.has_mode_hint(input) {
        Some(ArgumentHint {
            start: input.len(),
            query: "",
            usage: "",
            values: vec!["-f", "-b"],
        })
    } else {
        argument_hint(input, &state.slash_commands)
    }
}

fn slash_menu_open(state: &UiState) -> bool {
    state.slash_menu.open && !slash_matches(state).is_empty()
}

fn apply_slash_command(state: &mut UiState) {
    let Some(range) = slash_range(state.editor.text()) else {
        return;
    };
    let matches = slash_matches(state);
    let Some(command) = matches.get(state.slash_menu.selected) else {
        return;
    };
    let start = range.start;
    let end = state.editor.text().len();
    let replacement = format!("/{} ", command.name);
    state.editor.replace_range(start..end, &replacement);
    state.slash_menu.open();
    state.slash_menu.selected = 0;
}

fn clamp_slash_selection(state: &mut UiState) {
    let len = slash_matches(state).len();
    state.slash_menu.clamp(len);
}

pub(crate) fn slash_matches(state: &UiState) -> Vec<&SlashCommand> {
    if !state.slash_menu.open || argument_hint(state.editor.text(), &state.slash_commands).is_some()
    {
        return Vec::new();
    }

    let Some(range) = slash_range(state.editor.text()) else {
        return Vec::new();
    };

    matching_commands(&state.slash_commands, range.prefix)
}

pub(crate) fn slash_range(input: &str) -> Option<SlashRange<'_>> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(index, c)| c.is_whitespace().then_some(index + c.len_utf8()))
        .unwrap_or(0);
    let token = &input[start..];
    let prefix = token.strip_prefix('/')?;
    Some(SlashRange { start, prefix })
}

pub(crate) struct SlashRange<'a> {
    pub start: usize,
    pub prefix: &'a str,
}

fn dispatch_for_key(state: &UiState, key: &KeyInput) -> phi_sdk::turn::SubmissionDispatch {
    if state.conversation.run_status_view().active && *key == KeyInput::AltEnter {
        phi_sdk::turn::SubmissionDispatch::FollowUp
    } else {
        phi_sdk::turn::SubmissionDispatch::Steer
    }
}

fn submit(
    state: &mut UiState,
    agents: &AgentCatalog,
    dispatch: phi_sdk::turn::SubmissionDispatch,
) -> anyhow::Result<UiAction> {
    let display = state.editor.text().trim().to_string();
    let user_prompt = state.editor.expanded_text(&display);
    let file_context = file_mention_context(&user_prompt, agents);
    if user_prompt.is_empty() {
        return Ok(UiAction::None);
    }
    let history_entry = state.editor.history_entry(display.clone());
    state.editor.clear();
    state.argument_hint.refresh(state.editor.text());
    state.file_mentions.close();
    state.slash_menu.open();
    Ok(UiAction::Submit {
        query_prompt: QueryPrompt {
            user_prompt,
            injected_context: file_context
                .map(|content| phi_sdk::turn::InjectedContext {
                    source: phi_sdk::transcript::ContextSource::FileMention,
                    content,
                })
                .into_iter()
                .collect(),
        },
        display,
        history_entry,
        dispatch,
    })
}

fn update_completion_menus(state: &mut UiState, agents: &AgentCatalog) {
    state.argument_hint.refresh(state.editor.text());
    state.file_mentions.refresh(state.editor.text(), agents);
    if state.file_mentions.is_open() {
        state.slash_menu.close();
    } else {
        state.slash_menu.open();
    }
}

fn apply_file_mention(state: &mut UiState, key: KeyInput) {
    let Some(selected) = state.file_mentions.selected().cloned() else {
        return;
    };
    let mut input = state.editor.text().to_string();
    complete_mention(&mut input, &selected);
    let suggest_mode = key == KeyInput::Tab
        && selected.kind == super::mentions::MentionKind::Agent
        && input == format!("@{} ", selected.value);
    state.editor.replace_text(input);
    state.file_mentions.close();
    if suggest_mode {
        state.file_mentions.suggest_mode(state.editor.text());
    }
}

pub(super) fn render_queued_submissions(state: &UiState, width: u16) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    if !state.queued_submissions.is_empty() {
        lines.push(StyledLine::new(String::new(), ThemeColor::Default).fit(width));
    }
    for input in &state.queued_submissions {
        let label = match input.dispatch {
            phi_sdk::turn::SubmissionDispatch::Steer => "steer",
            phi_sdk::turn::SubmissionDispatch::FollowUp => "follow-up",
        };
        lines.push(
            StyledLine::new(
                format!("[{label} queued] {}", input.display),
                ThemeColor::Muted,
            )
            .fit(width),
        );
    }
    lines
}

pub(in crate::interactive) fn render_editor(
    state: &mut UiState,
    label: &str,
    width: u16,
    theme: &Theme,
) -> (Vec<StyledLine>, Option<Cursor>) {
    state.editor.set_width(width);
    let editor_text = if state.naming_switch {
        ""
    } else {
        state.editor.text()
    };
    let editor_visual_rows = if state.naming_switch {
        1
    } else {
        state.editor.visual_rows()
    };
    let editor = Editor {
        text: editor_text.to_string(),
        height: editor_visual_rows.saturating_add(2).clamp(3, 8) as u16,
        label: Some(label.to_string()),
        cursor: Some(state.editor.cursor()),
        cursor_row: state.editor.cursor_row(),
    };
    let (mut lines, (col, row)) = editor.render_with_cursor(&RenderContext::width(width, theme), 0);
    let first_line_scrolled = editor_visual_rows > lines.len().saturating_sub(2);
    highlight_bang_command(&mut lines, editor_text, width, first_line_scrolled);
    let inline_hint = input_hint(state).map(|hint| {
        let text = hint.text(state.argument_hint.selection(editor_text));
        let separator = if editor_text.ends_with(char::is_whitespace) || !hint.query.is_empty() {
            ""
        } else {
            " "
        };
        format!("{separator}{text}")
    });
    if !state.naming_switch
        && let Some(hint) = inline_hint
        && let Some(line) = lines.get_mut(usize::from(row))
    {
        *line = StyledLine::segments([
            (
                pad(line.text.trim_end(), usize::from(col)),
                ThemeColor::Default,
            ),
            (hint, ThemeColor::Dim),
        ])
        .fit(width);
    }
    (
        lines,
        Some(Cursor {
            row: row as usize,
            col,
        }),
    )
}

fn highlight_bang_command(
    lines: &mut [StyledLine],
    input: &str,
    width: u16,
    first_line_scrolled: bool,
) {
    if !input.starts_with('!') || first_line_scrolled {
        return;
    }
    let Some(line) = lines.get_mut(1) else {
        return;
    };
    let rest = line.text.strip_prefix('!').unwrap_or(&line.text);
    *line = StyledLine::segments([
        (String::from("!"), ThemeColor::Success),
        (
            pad(rest, width.saturating_sub(1) as usize),
            ThemeColor::Default,
        ),
    ])
    .fit(width);
}

pub(in crate::interactive) fn render_menu(
    state: &UiState,
    width: u16,
    theme: &Theme,
) -> (Vec<StyledLine>, Option<Cursor>) {
    if state.naming_switch || state.picker.is_some() {
        return super::picker::render_menu(state, width, theme);
    }
    let context = RenderContext::width(width, theme);
    let lines = if state.file_mentions.is_open() {
        state.file_mentions.render_menu(&context)
    } else {
        render_slash_menu(state, &context)
    };
    (lines, None)
}

fn render_slash_menu(state: &UiState, context: &RenderContext<'_>) -> Vec<StyledLine> {
    let matches = slash_matches(state);
    if matches.is_empty() {
        return Vec::new();
    }
    let items = matches
        .iter()
        .map(|command| {
            let description = match command.source {
                SlashCommandSource::Builtin => command.description.clone(),
                SlashCommandSource::User => format!("[u] {}", command.description),
                SlashCommandSource::Extension => format!("[e] {}", command.description),
            };
            SelectItem::new(&command.name, description)
        })
        .collect();
    SelectMenu {
        title: None,
        items,
        selected: state.slash_menu.selected,
        max_visible: 5,
    }
    .render(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommandArguments, CommandRegistry};
    use crate::interactive::components::permission::{ConfirmState, InteractionPrompt};

    #[test]
    fn runtime_switch_picker_does_not_emit_administration() {
        let mut state = UiState::default();
        state.open_picker(
            crate::interactive::state::MenuKind::Other,
            PickerState::with_kind(
                PickerKind::RuntimeSwitch,
                "select",
                vec![SelectItem::with_label("switch:plan", "plan", "")],
            ),
        );
        assert_eq!(
            handle_picker_key(&mut state, KeyInput::Ctrl('d')),
            UiAction::None
        );
        assert!(state.picker_confirming_delete.is_none());
        assert_eq!(handle_picker_key(&mut state, KeyInput::Esc), UiAction::None);
        assert!(state.picker.is_none());
    }

    #[test]
    fn resume_picker_only_emits_enabled_management_intents() {
        use crate::interactive::assembly::InteractiveFeatures;
        for (features, allowed) in [
            (InteractiveFeatures::all_enabled(), true),
            (InteractiveFeatures::default(), false),
        ] {
            let mut state = UiState {
                features,
                ..Default::default()
            };
            state.open_picker(
                crate::interactive::state::MenuKind::Other,
                PickerState::with_kind(
                    PickerKind::Resume,
                    super::super::picker::session::RESUME_PICKER_TITLE,
                    vec![SelectItem::with_label("resume:saved", "saved", "")],
                ),
            );
            assert_eq!(
                handle_picker_key(&mut state, KeyInput::Ctrl('d')),
                UiAction::None
            );
            assert_eq!(
                state.picker_confirming_delete.as_deref(),
                allowed.then_some("resume:saved")
            );
            let action = handle_picker_key(&mut state, KeyInput::Enter);
            assert_eq!(
                action,
                if allowed {
                    UiAction::DeletePick("resume:saved".into())
                } else {
                    UiAction::Pick("resume:saved".into())
                }
            );
            assert!(features.action(&action));
        }
    }

    #[test]
    fn thinking_shortcuts_preserve_editor_and_respect_overlays() {
        let mut state = UiState::default();
        state.editor.set_text("unfinished prompt");
        for (key, direction) in [(KeyInput::CtrlLeft, -1), (KeyInput::CtrlRight, 1)] {
            assert_eq!(
                state.runtime.dispatch_key(key),
                InputOutcome::Action(UiAction::AdjustThinkingLevel(direction))
            );
            assert_eq!(state.editor.text(), "unfinished prompt");
        }
        super::super::super::overlays::open_tasks(&mut state);
        for key in [KeyInput::CtrlLeft, KeyInput::CtrlRight] {
            assert!(!matches!(
                state.runtime.dispatch_key(key),
                InputOutcome::Action(UiAction::AdjustThinkingLevel(_))
            ));
        }
    }

    #[test]
    fn bash_handoff_and_unified_tasks_have_distinct_bindings() {
        let mut state = UiState::default();
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('b')),
            InputOutcome::Action(UiAction::BackgroundRunningBash)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('t')),
            InputOutcome::Action(UiAction::ToggleTasks)
        );
        super::super::super::overlays::open_tasks(&mut state);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('b')),
            InputOutcome::Action(UiAction::None)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('t')),
            InputOutcome::Action(UiAction::CloseActiveOverlay)
        );
    }

    #[test]
    fn slash_argument_hints_preview_and_confirm_canonical_values() {
        let agents = AgentCatalog::default();
        let settings = crate::config::Settings {
            switches: Some(vec![crate::config::SwitchConfig {
                name: "coding".into(),
                alias: Some("dev".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let commands = CommandRegistry::new(&[]).declarations(Vec::new(), &settings);
        for (input, confirmed, preview) in [
            ("/switch p", "/switch plan ", "/switch plan"),
            ("/switch dev", "/switch coding ", "/switch dev [coding]"),
            (
                "/login o",
                "/login openai ",
                "/login o [openai | openai-codex]",
            ),
            ("/model DATE", "/model update ", "/model DATE [update]"),
        ] {
            for key in [KeyInput::Tab, KeyInput::Enter] {
                let mut state = UiState::default();
                state.slash_commands = commands.clone();
                state.editor.set_text(input);
                update_completion_menus(&mut state, &agents);
                let (lines, _) = render_editor(&mut state, "", 100, &Theme::default());
                assert!(
                    lines.iter().any(|line| line.text.contains(preview)),
                    "{input}"
                );
                assert_eq!(state.editor.text(), input);
                assert!(matches!(
                    route_editor(&mut state, key, &agents).unwrap(),
                    InputOutcome::Consumed
                ));
                assert_eq!(state.editor.text(), confirmed);
            }
        }
        for input in [
            "/model update",
            "/login openai",
            "/switch unknown",
            "/switch plan",
            "/switch dev ",
            "/switch p extra",
            "text /switch p",
            "/switch\n",
        ] {
            let mut state = UiState::default();
            state.slash_commands = commands.clone();
            state.editor.set_text(input);
            assert!(input_hint(&state).is_none(), "{input}");
        }
    }

    #[test]
    fn empty_slash_arguments_submit_without_accepting_candidates() {
        let agents = AgentCatalog::default();
        let mut commands = CommandRegistry::new(&[]).declarations(Vec::new(), &Default::default());
        let mut memory = SlashCommand::builtin("memory-check", "check memory");
        memory.arguments = Some(CommandArguments::new("[--global]", &["--global"]));
        commands.push(memory);
        for name in ["model", "login", "switch", "memory-check"] {
            for separator in ["", " ", "  ", "\t", "\u{2003}"] {
                let mut state = UiState::default();
                state.slash_commands = commands.clone();
                state.editor.set_text(format!("/{name}{separator}"));
                update_completion_menus(&mut state, &agents);
                let action = route_surface_key(
                    &mut state,
                    InputSurfaceKind::Editor,
                    KeyInput::Enter,
                    &mut None,
                    &agents,
                )
                .unwrap();
                let UiAction::Submit { display, .. } = action else {
                    panic!("empty arguments must submit /{name}, not accept a candidate");
                };
                assert_eq!(display, format!("/{name}"));
            }
        }
        let mut state = UiState::default();
        state.slash_commands = commands;
        state.editor.set_text("/mod");
        update_completion_menus(&mut state, &agents);
        route_slash_menu(&mut state, KeyInput::Tab, &agents).unwrap();
        assert_eq!(state.editor.text(), "/model ");
        assert!(matches!(
            route_editor(&mut state, KeyInput::Enter, &agents).unwrap(),
            InputOutcome::Action(UiAction::Submit { display, .. }) if display == "/model"
        ));
    }

    #[test]
    fn argument_hints_follow_dismissal_and_editor_changes() {
        let agents = AgentCatalog::default();
        for values in [&[][..], &["fast", "full"][..]] {
            let mut command = SlashCommand::extension("inspect", "test");
            command.arguments = Some(CommandArguments::new("<mode>", values));
            let mut state = UiState::default();
            state.slash_commands = vec![command];
            state.editor.set_text("/inspect");
            update_completion_menus(&mut state, &agents);
            let (lines, _) = render_editor(&mut state, "", 100, &Theme::default());
            let expected = if values.is_empty() {
                "<mode>"
            } else {
                "[fast | full]"
            };
            assert!(lines.iter().any(|line| line.text.contains(expected)));
            assert!(slash_matches(&state).is_empty());
            route_editor(&mut state, KeyInput::Esc, &agents).unwrap();
            assert!(input_hint(&state).is_none());
            assert!(slash_matches(&state).is_empty());
            assert!(matches!(
                route_editor(&mut state, KeyInput::Enter, &agents).unwrap(),
                InputOutcome::Action(UiAction::Submit { .. })
            ));
            state.editor.set_text("/inspect");
            update_completion_menus(&mut state, &agents);
            assert!(input_hint(&state).is_some());
            route_editor(&mut state, KeyInput::Esc, &agents).unwrap();
            state.editor.insert(" ");
            update_completion_menus(&mut state, &agents);
            assert!(input_hint(&state).is_some());
            for input in [
                "/inspect fast",
                "/inspect unmatched",
                "/inspect unmatched more",
            ] {
                state.editor.set_text(input);
                update_completion_menus(&mut state, &agents);
                let (lines, _) = render_editor(&mut state, "", 100, &Theme::default());
                assert!(
                    lines.iter().any(|line| line.text.trim_end() == input),
                    "{input}"
                );
                assert_eq!(state.editor.text(), input);
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]

        #[test]
        fn registered_arguments_cycle_without_editing(
            name in "custom-[a-z]{1,12}", count in 1usize..16, tabs in 0usize..32,
        ) {
            let values = (0..count).map(|index| format!("mode-{index}")).collect::<Vec<_>>();
            let registration = phi_sdk::extension::RegisteredCommand {
                name: name.clone(),
                description: None,
                source: "test.lua".into(),
                target: phi_sdk::extension::CommandTarget::Handler,
                arguments: Some(CommandArguments::new("<mode>", &values.iter().map(String::as_str).collect::<Vec<_>>())),
            };
            let mut state = UiState::default();
            state.slash_commands = CommandRegistry::new(&[registration]).declarations(Vec::new(), &Default::default());
            let agents = AgentCatalog::default();
            let input = format!("/{name} ");
            state.editor.set_text(&input);
            update_completion_menus(&mut state, &agents);
            sync_input_surface(&mut state);
            proptest::prop_assert_eq!(state.runtime.input_context(), InputContext::Editor);
            proptest::prop_assert!(slash_matches(&state).is_empty());
            for _ in 0..tabs {
                route_editor(&mut state, KeyInput::Tab, &agents).unwrap();
                proptest::prop_assert_eq!(state.editor.text(), input.as_str());
                proptest::prop_assert_eq!(state.editor.cursor(), input.len());
            }
            let outcome = route_editor(&mut state, KeyInput::Enter, &agents).unwrap();
            if tabs == 0 {
                proptest::prop_assert!(matches!(outcome, InputOutcome::Action(UiAction::Submit { ref display, .. }) if display == &format!("/{name}")), "unselected candidates must not be inserted");
            } else {
                proptest::prop_assert!(matches!(outcome, InputOutcome::Consumed));
                proptest::prop_assert_eq!(state.editor.text(), format!("/{name} {} ", values[tabs % count]));
                let outcome = route_editor(&mut state, KeyInput::Enter, &agents).unwrap();
                proptest::prop_assert!(matches!(outcome, InputOutcome::Action(UiAction::Submit { .. })), "confirmed arguments must submit on the next Enter");
            }
            state.editor.set_text(&input);
            update_completion_menus(&mut state, &agents);
            route_editor(&mut state, KeyInput::Tab, &agents).unwrap();
            route_editor(&mut state, KeyInput::Char(' '), &agents).unwrap();
            let outcome = route_editor(&mut state, KeyInput::Enter, &agents).unwrap();
            proptest::prop_assert!(matches!(outcome, InputOutcome::Action(UiAction::Submit { ref display, .. }) if display == &format!("/{name}")), "editing must retire the previous selection");
        }
    }

    #[test]
    fn agent_mode_hints_require_selection_before_enter_inserts() {
        let agents = AgentCatalog::new(vec![crate::resources::AgentDefinitionView {
            name: "reviewer".into(),
            description: String::new(),
        }]);
        for tabs in 0..4 {
            let mut state = UiState::default();
            state.editor.set_text("@review");
            update_completion_menus(&mut state, &agents);
            handle_file_mention_key(&mut state, KeyInput::Tab, &agents).unwrap();
            assert_eq!(state.editor.text(), "@reviewer ");
            assert!(input_hint(&state).is_some());
            for _ in 0..tabs {
                route_editor(&mut state, KeyInput::Tab, &agents).unwrap();
            }
            if tabs == 0 {
                assert!(matches!(
                    route_editor(&mut state, KeyInput::Enter, &agents).unwrap(),
                    InputOutcome::Action(UiAction::Submit { display, .. }) if display == "@reviewer"
                ));
                assert!(!state.file_mentions.has_mode_hint(state.editor.text()));
                continue;
            }
            let flag = if tabs % 2 == 0 { "-f" } else { "-b" };
            assert_eq!(state.editor.text(), "@reviewer ");
            assert!(matches!(
                route_editor(&mut state, KeyInput::Enter, &agents).unwrap(),
                InputOutcome::Consumed
            ));
            assert_eq!(state.editor.text(), format!("@reviewer {flag} "));
            assert!(!state.file_mentions.has_mode_hint(state.editor.text()));
        }
        for key in [
            KeyInput::Char('i'),
            KeyInput::Paste("inspect".into()),
            KeyInput::Esc,
        ] {
            let mut state = UiState::default();
            state.editor.set_text("@review");
            update_completion_menus(&mut state, &agents);
            handle_file_mention_key(&mut state, KeyInput::Tab, &agents).unwrap();
            route_editor(&mut state, key.clone(), &agents).unwrap();
            assert!(!state.file_mentions.has_mode_hint(state.editor.text()));
            let suffix = match key {
                KeyInput::Char(_) => "i",
                KeyInput::Paste(_) => "inspect",
                _ => "",
            };
            assert_eq!(state.editor.text(), format!("@reviewer {suffix}"));
        }
        let mut state = UiState::default();
        state.editor.set_text("inspect @review");
        update_completion_menus(&mut state, &agents);
        handle_file_mention_key(&mut state, KeyInput::Tab, &agents).unwrap();
        assert!(!state.file_mentions.has_mode_hint(state.editor.text()));
    }

    #[test]
    fn completed_file_mentions_remain_normal_prompts_on_enter() {
        use super::super::mentions::{MentionKind, SelectedMention};
        use crate::interactive::agent::directed_agent_invocation;

        let agents = AgentCatalog::new(vec![crate::resources::AgentDefinitionView {
            name: "reviewer".into(),
            description: String::new(),
        }]);
        for path in ["README.md", "phi-interactive/README.md"] {
            for suffix in ["", "inspect this file"] {
                let mut input = "@READ".to_string();
                complete_mention(
                    &mut input,
                    &SelectedMention {
                        kind: MentionKind::File,
                        value: path.into(),
                    },
                );
                input.push_str(suffix);
                let mut state = UiState::default();
                state.editor.set_text(input.clone());
                let InputOutcome::Action(UiAction::Submit {
                    query_prompt,
                    display,
                    ..
                }) = route_editor(&mut state, KeyInput::Enter, &agents).unwrap()
                else {
                    panic!("expected submitted prompt");
                };
                assert_eq!(query_prompt.user_prompt, input.trim());
                assert_eq!(query_prompt.injected_context.len(), 1);
                assert!(
                    query_prompt.injected_context[0]
                        .content
                        .contains(&format!("<file path=\"{path}\" />"))
                );
                assert!(
                    directed_agent_invocation(&display, &agents).is_none(),
                    "{display}"
                );
            }
        }
    }

    #[test]
    fn completed_agent_with_file_reference_keeps_directed_intent() {
        let agents = AgentCatalog::new(vec![crate::resources::AgentDefinitionView {
            name: "reviewer".into(),
            description: String::new(),
        }]);
        let mut state = UiState::default();
        state.editor.set_text("@review");
        update_completion_menus(&mut state, &agents);
        handle_file_mention_key(&mut state, KeyInput::Enter, &agents).unwrap();
        assert_eq!(state.editor.text(), "@reviewer ");
        state.editor.insert("inspect @README.md");
        let InputOutcome::Action(UiAction::Submit {
            query_prompt,
            display,
            ..
        }) = route_editor(&mut state, KeyInput::Enter, &agents).unwrap()
        else {
            panic!("expected submitted prompt");
        };
        let directed = crate::interactive::agent::directed_agent_invocation(&display, &agents)
            .unwrap()
            .unwrap();
        assert_eq!(directed.agent, "reviewer");
        assert_eq!(query_prompt.user_prompt, "@reviewer inspect @README.md");
        assert_eq!(query_prompt.injected_context.len(), 1);
        assert!(
            query_prompt.injected_context[0]
                .content
                .contains("<file path=\"README.md\" />")
        );
        assert!(
            !query_prompt.injected_context[0]
                .content
                .contains("<file path=\"reviewer\" />")
        );
    }

    #[tokio::test]
    async fn login_picker_selects_provider_and_cancels_without_login() {
        for (selection, expected) in [(0, "login:openai"), (1, "login:openai-codex")] {
            let mut state = UiState::default();
            crate::interactive::commands::Authentication::default()
                .execute("", &mut state)
                .await;
            let picker = state.picker.as_ref().unwrap();
            assert_eq!(picker.kind, PickerKind::Login);
            assert_eq!(picker.items[0].label, "OpenAI");
            assert_eq!(picker.items[1].label, "OpenAI Codex");
            for _ in 0..selection {
                handle_picker_key(&mut state, KeyInput::Down);
            }
            assert_eq!(
                handle_picker_key(&mut state, KeyInput::Enter),
                UiAction::Pick(expected.into())
            );
            assert!(state.picker.is_none());
        }
        for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
            let mut state = UiState::default();
            crate::interactive::commands::Authentication::default()
                .execute("", &mut state)
                .await;
            assert_eq!(handle_picker_key(&mut state, key), UiAction::None);
            assert!(state.picker.is_none());
        }
    }

    #[test]
    fn text_editing_preserves_unicode_paste_and_completion_policy() {
        let mut state = UiState::default();
        let agents = AgentCatalog::default();
        handle_text_key(&mut state, KeyInput::Paste("a\r\ne\u{301}".into()), &agents);
        assert_eq!(state.editor.text(), "a\ne\u{301}");
        handle_text_key(&mut state, KeyInput::Left, &agents);
        assert_eq!(state.editor.cursor(), 2);
        handle_text_key(&mut state, KeyInput::Right, &agents);
        handle_text_key(&mut state, KeyInput::Backspace, &agents);
        assert_eq!(state.editor.text(), "a\n");
        state.editor.clear();
        handle_text_key(&mut state, KeyInput::Char('/'), &agents);
        assert!(state.slash_menu.open);
        handle_text_key(&mut state, KeyInput::ShiftEnter, &agents);
        assert_eq!(state.editor.text(), "/\n");
    }

    #[test]
    fn paste_placeholder_is_limited_to_the_editor_display() {
        let mut state = UiState::default();
        let pasted = (0..11)
            .map(|line| format!("pasted line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.editor.insert_paste(&pasted);

        let action = submit(
            &mut state,
            &AgentCatalog::default(),
            phi_sdk::turn::SubmissionDispatch::Steer,
        )
        .unwrap();

        let UiAction::Submit {
            query_prompt,
            display,
            ..
        } = action
        else {
            panic!("expected submitted prompt");
        };
        assert_eq!(query_prompt.user_prompt, pasted);
        assert_eq!(display, "[paste #1 +11 lines]");
    }

    #[test]
    fn mounted_input_surface_owns_dynamic_context_and_priority() {
        let mut state = UiState::default();
        assert_eq!(state.runtime.input_context(), InputContext::Editor);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('o')),
            InputOutcome::Action(UiAction::ToggleAllBlocksExpanded)
        );

        state.set_interaction(InteractionPrompt::Permission(ConfirmState {
            title: String::from("confirm"),
            message: String::new(),
            choice: phi_tui::Choice::new(["Allow once", "Always", "Deny"]),
            help: String::new(),
        }));
        sync_input_surface(&mut state);

        assert_eq!(state.runtime.input_context(), InputContext::Confirm);
        let theme = phi_tui::Theme::default();
        for width in [0, 1, 12, 48, 120] {
            for height in [0, 1, 3, 8] {
                let context = phi_tui::RenderContext::new(
                    phi_tui::RenderArea {
                        width,
                        height: Some(height),
                    },
                    &theme,
                );
                let lines = state.interaction_lines(&context).unwrap();
                assert!(lines.len() <= height);
                assert!(
                    lines
                        .iter()
                        .all(|line| phi_tui::text::display_width(&line.text) <= width as usize)
                );
                if width == 120 && height == 8 {
                    assert!(
                        lines
                            .iter()
                            .any(|line| line.text.contains("[ Allow once ]"))
                    );
                }
            }
        }
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::PermissionAllowOnce)
        );

        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('o')),
            InputOutcome::Consumed
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('y')),
            InputOutcome::Consumed
        );
        state.runtime.dispatch_key(KeyInput::Left);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::PermissionDeny)
        );
        state.runtime.dispatch_key(KeyInput::Tab);
        state.runtime.dispatch_key(KeyInput::Right);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::PermissionAllowAlways)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Esc),
            InputOutcome::Action(UiAction::PermissionDeny)
        );
        assert!(state.interaction.is_some());

        state.runtime.set_key_bindings(key_bindings().bind(
            InputContext::Confirm,
            KeyInput::Right,
            UiAction::Resize,
        ));
        sync_input_surface(&mut state);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Right),
            InputOutcome::Action(UiAction::Resize)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::PermissionAllowAlways)
        );

        super::super::super::overlays::open_tasks(&mut state);
        let overlay_focus = state.runtime.active_focus();
        sync_input_surface(&mut state);
        assert_eq!(state.runtime.active_focus(), overlay_focus);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Esc),
            InputOutcome::Action(UiAction::CloseActiveOverlay)
        );
        super::super::super::overlays::close_active(&mut state);
        sync_input_surface(&mut state);
        assert_eq!(state.runtime.input_context(), InputContext::Confirm);

        state.clear_interaction();
        sync_input_surface(&mut state);
        assert_eq!(state.runtime.input_context(), InputContext::Editor);
        state.input_surface.activate(
            &mut state.runtime,
            InputSurfaceKind::Subagent,
            InputContext::Transcript,
        );
        assert_eq!(state.runtime.input_context(), InputContext::Transcript);
        let revision = state.runtime.render_revision();
        state.input_surface.activate(
            &mut state.runtime,
            InputSurfaceKind::Subagent,
            InputContext::Transcript,
        );
        assert_eq!(state.runtime.render_revision(), revision);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('b')),
            InputOutcome::Action(UiAction::SurfaceKey {
                surface: InputSurfaceKind::Subagent,
                key: KeyInput::Ctrl('b'),
            })
        );
    }

    #[test]
    fn mounted_mcp_url_maps_actions_without_settling_the_request() {
        use crate::interactive::components::panel::mcp::ElicitationState;
        let request = phi_sdk::extension::mcp::McpElicitationRequest::Url {
            server: "server".into(),
            protocol_request_id: serde_json::json!(1),
            parent_tool_call_id: None,
            message: "Continue".into(),
            url: "https://example.com/login".into(),
            elicitation_id: "request".into(),
        };
        let mut state = UiState::default();
        super::super::super::overlays::open_tasks(&mut state);
        let overlay_focus = state.runtime.active_focus();
        state.set_interaction(InteractionPrompt::Elicitation(
            ElicitationState::from_request(&request).unwrap(),
        ));
        sync_input_surface(&mut state);
        assert_eq!(state.runtime.active_focus(), overlay_focus);
        super::super::super::overlays::close_active(&mut state);
        sync_input_surface(&mut state);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::McpElicitationOpenUrl(
                "https://example.com/login".into()
            ))
        );
        assert!(state.interaction.is_some());
        state.runtime.dispatch_key(KeyInput::Right);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::McpElicitationSubmit(None))
        );
        state.runtime.dispatch_key(KeyInput::Tab);
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(UiAction::McpElicitationDecline)
        );
        assert_eq!(
            state.runtime.dispatch_key(KeyInput::Ctrl('c')),
            InputOutcome::Action(UiAction::McpElicitationCancel)
        );
        assert!(state.interaction.is_some());
        state.clear_interaction();
        sync_input_surface(&mut state);
        assert_eq!(state.runtime.input_context(), InputContext::Editor);
    }

    #[test]
    fn escape_and_ctrl_c_cancel_an_active_run() {
        for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
            let mut state = UiState::default();
            let mut last_ctrl_c = None;
            state.conversation.project_main_run_started();

            assert_eq!(
                handle_global_key(&mut state, &key, &mut last_ctrl_c),
                Some(UiAction::CancelRun)
            );
        }
    }

    #[test]
    fn switch_escape_navigates_and_ctrl_c_exits() {
        let mut state = UiState::default();
        let open_configure = |state: &mut UiState| {
            state.open_picker(
                crate::interactive::state::MenuKind::Switch,
                PickerState::with_kind(PickerKind::SwitchConfigure, "configure", Vec::new()),
            );
        };

        open_configure(&mut state);
        assert_eq!(
            handle_picker_key(&mut state, KeyInput::Esc),
            UiAction::Pick(String::from("switch:back"))
        );

        state.open_picker(
            crate::interactive::state::MenuKind::Switch,
            PickerState::with_kind(PickerKind::SwitchSelect, "switches", Vec::new()),
        );
        assert_eq!(
            handle_picker_key(&mut state, KeyInput::Esc),
            UiAction::Pick(String::from("switch:sync"))
        );

        open_configure(&mut state);
        assert_eq!(
            handle_picker_key(&mut state, KeyInput::Ctrl('c')),
            UiAction::SaveSwitchConfig
        );
        for (kind, escape) in [
            (
                PickerKind::SwitchEdit {
                    switch_name: "custom".into(),
                },
                "switch:back:configure:custom",
            ),
            (
                PickerKind::SwitchModel {
                    switch_name: "custom".into(),
                },
                "switch:edit:custom",
            ),
        ] {
            for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
                state.editor.replace_text("retained draft");
                state.open_picker(
                    crate::interactive::state::MenuKind::Switch,
                    PickerState::with_kind(kind.clone(), "edit", Vec::new()),
                );
                let expected = if key == KeyInput::Esc {
                    UiAction::Pick(escape.into())
                } else {
                    UiAction::ApplySwitchConfig("custom".into())
                };
                assert_eq!(handle_picker_key(&mut state, key.clone()), expected);
                assert!(state.picker.is_none());
                assert_eq!(state.active_menu, None);
                assert_eq!(
                    state.editor.text(),
                    if key == KeyInput::Esc {
                        "retained draft"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    #[test]
    fn switch_alias_backspace_removes_a_complete_grapheme() {
        let mut state = UiState::default();
        state.open_picker(
            crate::interactive::state::MenuKind::Switch,
            PickerState::with_kind(
                PickerKind::SwitchEdit {
                    switch_name: "custom".into(),
                },
                "edit",
                vec![SelectItem::with_label(
                    "switch:alias:custom",
                    "alias",
                    "ae\u{301}",
                )],
            ),
        );
        assert_eq!(
            edit_selected_switch_alias(&mut state, &KeyInput::Backspace),
            Some(UiAction::SetSwitchAlias {
                switch_name: "custom".into(),
                alias: "a".into()
            })
        );
    }

    #[test]
    fn filterable_rewind_picker_searches_tool_labels() {
        let items = vec![
            SelectItem::with_label("rewind:1", "[Edit: src/main.rs]", ""),
            SelectItem::with_label("rewind:2", "user: explain this", ""),
        ];
        let mut picker = PickerState::with_kind(PickerKind::Rewind, "rewind", items);

        assert!(handle_rewind_search_key(&mut picker, &KeyInput::Char('/')));
        for key in "edit".chars().map(KeyInput::Char) {
            picker.handle_input_with_filter(&key, filter_items);
        }

        assert_eq!(picker.items.len(), 1);
        assert_eq!(picker.items[0].value, "rewind:1");

        assert!(handle_rewind_search_key(&mut picker, &KeyInput::Esc));
        assert!(!picker.filterable);
        assert!(picker.query.is_empty());
        assert_eq!(picker.items.len(), 2);
    }
}
