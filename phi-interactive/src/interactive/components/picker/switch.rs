use crate::config::{SwitchConfig, SwitchSettings};
use crate::interactive::state::{MenuKind, PickerKind, PickerState, UiState};
use crate::session::Session;
use phi_tui::{NotifyKind, SelectItem};

use super::PickerDraft;
use crate::interactive::components::input::UiAction;
use crate::runtime::attachment::AgentAttachment;
use crate::runtime::preparation::ModelChoices;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::interactive) struct SwitchFeatures {
    pub(in crate::interactive) configuration: bool,
}

impl SwitchFeatures {
    pub(in crate::interactive) fn action(self, action: &UiAction) -> Option<bool> {
        let configuration = match action {
            UiAction::Pick(value) => configuration_value(value),
            UiAction::DeletePick(value) => value.starts_with("switch:delete:"),
            UiAction::CycleSwitchField { .. } | UiAction::SetSwitchAlias { .. } => true,
            _ => return None,
        };
        configuration.then_some(self.configuration)
    }
}

fn configuration_value(value: &str) -> bool {
    matches!(value, "switch:configure" | "switch:back" | "switch:add")
        || [
            "switch:back:configure:",
            "switch:add:",
            "switch:edit:",
            "switch:set:",
            "switch:model:",
            "switch:delete:",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

pub(in crate::interactive) enum SwitchIntent {
    Updated,
    Select(String),
    Apply(SwitchSettings),
}

pub(in crate::interactive) struct SwitchEditor {
    models: ModelChoices,
    draft: PickerDraft,
}

impl SwitchEditor {
    pub(in crate::interactive) fn new(models: ModelChoices, draft: PickerDraft) -> Self {
        Self { models, draft }
    }
}

pub(in crate::interactive) struct SwitchPicker {
    editor: Option<SwitchEditor>,
}

impl SwitchPicker {
    pub(in crate::interactive) fn new() -> Self {
        Self { editor: None }
    }

    pub(in crate::interactive) fn with_configuration(mut self, editor: SwitchEditor) -> Self {
        self.editor = Some(editor);
        self
    }

    pub(in crate::interactive) fn features(&self) -> SwitchFeatures {
        SwitchFeatures {
            configuration: self.editor.is_some(),
        }
    }

    pub(in crate::interactive) async fn names(
        &self,
        attachment: &AgentAttachment,
    ) -> anyhow::Result<Vec<String>> {
        match &self.editor {
            Some(editor) => Ok(editor.draft.borrow().switch_names()),
            None => Ok(attachment.runtime_catalog().await?.switches),
        }
    }

    pub(in crate::interactive) async fn open(
        &self,
        attachment: &AgentAttachment,
        state: &mut UiState,
        session: &Session,
    ) {
        if let Some(editor) = &self.editor {
            open_switch_picker(state, session, &editor.draft.borrow());
            return;
        }
        let names = match self.names(attachment).await {
            Ok(names) => names,
            Err(error) => {
                state.notify(
                    NotifyKind::Error,
                    format!("failed to load runtime choices: {error}"),
                );
                return;
            }
        };
        let items = names
            .into_iter()
            .map(|name| {
                let description = if name == session.current_switch.as_str() {
                    "current"
                } else {
                    ""
                };
                SelectItem::with_label(format!("switch:{name}"), name, description)
            })
            .collect();
        state.open_picker(
            MenuKind::Other,
            PickerState::with_kind(PickerKind::RuntimeSwitch, "select Switch", items),
        );
    }

    pub(in crate::interactive) fn pick(
        &self,
        value: &str,
        state: &mut UiState,
        session: &Session,
    ) -> Option<SwitchIntent> {
        let name = value.strip_prefix("switch:")?;
        let Some(editor) = &self.editor else {
            return (!configuration_value(value)).then(|| SwitchIntent::Select(name.to_owned()));
        };
        let mut draft = editor.draft.borrow_mut();
        match value {
            "switch:configure" => open_switch_configure_picker(state, &draft),
            "switch:back" => open_switch_picker(state, session, &draft),
            "switch:add" => start_add_switch(state),
            _ => {
                if let Some(name) = value.strip_prefix("switch:add:") {
                    add_switch(name, state, session, &mut draft);
                    return Some(SwitchIntent::Apply(draft.clone()));
                } else if value.starts_with("switch:back:configure:") {
                    open_switch_configure_picker(state, &draft);
                } else if let Some(name) = value.strip_prefix("switch:edit:") {
                    open_switch_edit_picker(name, state, session, &draft);
                } else if let Some(rest) = value.strip_prefix("switch:set:") {
                    if let Some((name, field)) = rest.rsplit_once(':') {
                        if field == "model" {
                            open_switch_model_picker(&editor.models, name, state, session, &draft);
                        } else {
                            update_switch_field(
                                &editor.models,
                                name,
                                field,
                                1,
                                state,
                                session,
                                &mut draft,
                            );
                            return Some(SwitchIntent::Apply(draft.clone()));
                        }
                    }
                } else if let Some(rest) = value.strip_prefix("switch:model:") {
                    if let Some((name, selection)) = rest.split_once(':') {
                        set_switch_model(name, selection, state, session, &mut draft);
                        open_switch_edit_picker(name, state, session, &draft);
                        return Some(SwitchIntent::Apply(draft.clone()));
                    }
                } else if value.starts_with("switch:delete:") {
                    if let Some(picker) = &mut state.picker {
                        picker.title =
                            String::from("delete switch? enter confirm · esc/ctrl+c cancel");
                    }
                    state.picker_confirming_delete = Some(value.to_owned());
                } else {
                    return Some(SwitchIntent::Select(name.to_owned()));
                }
            }
        }
        Some(SwitchIntent::Updated)
    }

    pub(in crate::interactive) fn action(
        &self,
        action: UiAction,
        state: &mut UiState,
        session: &Session,
    ) -> Option<SwitchIntent> {
        let editor = self.editor.as_ref()?;
        if self.features().action(&action) != Some(true) {
            return None;
        }
        let mut draft = editor.draft.borrow_mut();
        match action {
            UiAction::SetSwitchAlias { switch_name, alias } => {
                set_switch_alias(&switch_name, &alias, state, session, &mut draft);
                return Some(SwitchIntent::Apply(draft.clone()));
            }
            UiAction::CycleSwitchField { value, direction } => {
                if let Some(rest) = value.strip_prefix("switch:set:")
                    && let Some((name, field)) = rest.rsplit_once(':')
                {
                    update_switch_field(
                        &editor.models,
                        name,
                        field,
                        direction,
                        state,
                        session,
                        &mut draft,
                    );
                    return Some(SwitchIntent::Apply(draft.clone()));
                }
            }
            UiAction::DeletePick(value) => {
                if let Some(name) = value.strip_prefix("switch:delete:") {
                    delete_switch(name, state, session, &mut draft);
                    return Some(SwitchIntent::Apply(draft.clone()));
                }
            }
            _ => return None,
        }
        Some(SwitchIntent::Updated)
    }
}

fn open_switch_picker(state: &mut UiState, session: &Session, settings: &SwitchSettings) {
    state.open_picker(
        MenuKind::Switch,
        PickerState::with_kind(
            PickerKind::SwitchSelect,
            "select switch",
            switch_items(session, settings, session.current_switch.as_str()),
        ),
    );
    state.runtime.request_render();
}

fn open_switch_configure_picker(state: &mut UiState, settings: &SwitchSettings) {
    state.open_picker(
        MenuKind::Switch,
        PickerState::with_kind(
            PickerKind::SwitchConfigure,
            "configure switches",
            switch_configure_items(settings),
        ),
    );
    state.runtime.request_render();
}

fn open_switch_edit_picker(
    name: &str,
    state: &mut UiState,
    session: &Session,
    settings: &SwitchSettings,
) {
    let Some(mode) = settings.switch_by_name(name) else {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    };
    state.open_picker(
        MenuKind::Switch,
        PickerState::with_kind(
            PickerKind::SwitchEdit {
                switch_name: name.to_string(),
            },
            format!("configure switch · {name}"),
            switch_edit_items(&mode, session, settings),
        ),
    );
    state.runtime.request_render();
}

fn open_switch_model_picker(
    models: &crate::runtime::preparation::ModelChoices,
    name: &str,
    state: &mut UiState,
    session: &Session,
    settings: &SwitchSettings,
) {
    let Some(mode) = settings.switch_by_name(name) else {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    };
    let provider = mode
        .provider
        .as_deref()
        .unwrap_or(session.provider.as_str());
    let current_model = mode.model.as_deref().unwrap_or(session.model.as_str());
    let models = models.switch_model_choices(provider, current_model, settings.default_model());
    let items = models
        .into_iter()
        .filter(|model| {
            settings.default_provider() == Some(provider)
                || settings.default_model() != Some(model.as_str())
        })
        .map(|model| {
            let marker = if model == current_model { " ✔" } else { "" };
            phi_tui::SelectItem::with_label(
                format!("switch:model:{name}:model:{provider}:{model}"),
                model,
                format!("[{provider}]{marker}"),
            )
        })
        .collect();
    state.open_picker(
        MenuKind::Switch,
        PickerState {
            filterable: true,
            ..PickerState::with_kind(
                PickerKind::SwitchModel {
                    switch_name: name.to_string(),
                },
                format!("select model for switch · {name}"),
                items,
            )
        },
    );
    state.runtime.request_render();
}

fn update_switch_field(
    models: &crate::runtime::preparation::ModelChoices,
    name: &str,
    field: &str,
    direction: i32,
    state: &mut UiState,
    session: &Session,
    settings: &mut SwitchSettings,
) {
    if !matches!(field, "provider" | "model" | "thinking" | "mode") {
        return;
    }
    if field == "mode" && settings.is_builtin_switch_name(name) {
        state.notify(NotifyKind::Error, "builtin switch cannot be changed");
        return;
    }

    let current_provider = session.provider.as_str();
    let current_model = session.model.as_str();
    let Some(switch) = settings
        .custom_switches()
        .into_iter()
        .find(|switch| switch.name == name)
        .or_else(|| settings.switch_by_name(name))
    else {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    };
    let choices = match field {
        "provider" => models.switch_provider_choices(current_provider),
        "model" => models.switch_model_choices(
            switch.provider.as_deref().unwrap_or(current_provider),
            current_model,
            settings.default_model(),
        ),
        _ => Vec::new(),
    };
    let edited = settings.edit_switch(name, |settings, switch| match field {
        "provider" => {
            switch.provider = Some(next_choice(
                &choices,
                current_provider,
                switch.provider.as_deref(),
                direction,
            ));
        }
        "model" => {
            switch.model = Some(next_choice(
                &choices,
                current_model,
                switch.model.as_deref(),
                direction,
            ));
        }
        "thinking" => {
            switch.thinking_level = next_thinking(switch.thinking_level.as_deref(), direction);
        }
        "mode" => {
            switch.mode = next_work_mode(settings, switch.mode.as_deref(), direction);
        }
        _ => unreachable!(),
    });
    if edited.is_none() {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    }
    refresh_switch_edit_picker(name, state, session, settings);
}

pub(crate) fn set_switch_model(
    name: &str,
    selection: &str,
    state: &mut UiState,
    session: &Session,
    settings: &mut SwitchSettings,
) {
    let (provider, model) = parse_model_selection(selection, session);
    let edited = settings.edit_switch(name, |_, switch| {
        switch.provider = Some(provider);
        switch.model = Some(model);
    });
    if edited.is_none() {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
    }
}

fn refresh_switch_edit_picker(
    name: &str,
    state: &mut UiState,
    session: &Session,
    settings: &SwitchSettings,
) {
    let Some(mode) = settings.switch_by_name(name) else {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    };
    if let Some(picker) = &mut state.picker {
        picker.title = format!("configure switch · {name}");
        picker.set_items(switch_edit_items(&mode, session, settings));
    }
}

fn start_add_switch(state: &mut UiState) {
    state.editor.clear();
    state.naming_switch = true;
    state.open_picker(
        MenuKind::Switch,
        PickerState::with_kind(
            PickerKind::SwitchName,
            "add switch · enter confirm · esc/ctrl+c cancel",
            vec![phi_tui::SelectItem::with_label("switch:name", "name: ", "")],
        ),
    );
    state.runtime.request_render();
}

pub(crate) fn set_switch_alias(
    name: &str,
    alias: &str,
    state: &mut UiState,
    session: &Session,
    settings: &mut SwitchSettings,
) {
    let alias = alias.trim();
    let edited = settings.edit_switch(name, |_, switch| {
        switch.alias = (!alias.is_empty()).then(|| alias.to_string());
    });
    if edited.is_none() {
        state.notify(NotifyKind::Error, format!("switch not found: {name}"));
        return;
    }
    refresh_switch_edit_picker(name, state, session, settings);
}

fn add_switch(name: &str, state: &mut UiState, session: &Session, settings: &mut SwitchSettings) {
    let mut modes = settings.custom_switches();
    if settings.switch_by_name(name).is_some() {
        state.notify(NotifyKind::Error, format!("switch already exists: {name}"));
        open_switch_configure_picker(state, settings);
        return;
    }
    modes.push(SwitchConfig {
        name: name.to_string(),
        alias: None,
        provider: settings.default_provider().map(ToString::to_string),
        model: settings.default_model().map(ToString::to_string),
        thinking_level: settings.default_thinking_level().map(ToString::to_string),
        mode: Some(String::from("default")),
        prompt: None,
    });
    settings.switches = Some(modes);
    state.notify(NotifyKind::Info, format!("switch added: {name}"));
    open_switch_edit_picker(name, state, session, settings);
}

fn delete_switch(
    name: &str,
    state: &mut UiState,
    session: &Session,
    settings: &mut SwitchSettings,
) {
    if name == session.current_switch.as_str() {
        state.notify(NotifyKind::Error, "cannot delete current switch");
        return;
    }
    if !settings
        .custom_switches()
        .iter()
        .any(|mode| mode.name == name)
    {
        state.notify(NotifyKind::Error, "cannot delete builtin switch");
        return;
    }
    let modes = settings
        .custom_switches()
        .into_iter()
        .filter(|mode| mode.name != name)
        .collect::<Vec<_>>();
    settings.switches = Some(modes);
    state.notify(NotifyKind::Info, format!("switch deleted: {name}"));
    open_switch_configure_picker(state, settings);
}

fn next_choice(
    choices: &[String],
    fallback: &str,
    current: Option<&str>,
    direction: i32,
) -> String {
    if choices.is_empty() {
        return fallback.to_string();
    }
    let index = current
        .and_then(|value| choices.iter().position(|choice| choice == value))
        .unwrap_or(0);
    choices[cycle_index(index, choices.len(), direction)].clone()
}

fn next_thinking(current: Option<&str>, direction: i32) -> Option<String> {
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    let index = current
        .and_then(|level| levels.iter().position(|known| *known == level))
        .unwrap_or(0);
    let next = cycle_index(index, levels.len(), direction);
    Some(levels[next].to_string())
}

fn cycle_index(index: usize, len: usize, direction: i32) -> usize {
    if direction < 0 {
        (index + len - 1) % len
    } else {
        (index + 1) % len
    }
}

fn next_work_mode(
    settings: &SwitchSettings,
    current: Option<&str>,
    direction: i32,
) -> Option<String> {
    let modes = settings.switch_mode_names();
    let current = current.unwrap_or("none");
    let len = modes.len();
    if len == 0 {
        return None;
    }
    let current_index = modes.iter().position(|mode| mode == current).unwrap_or(0);
    let next = cycle_index(current_index, len, direction);
    match modes[next].as_str() {
        "none" => None,
        mode => Some(mode.to_string()),
    }
}

// Persistence failure cannot roll back an accepted runtime selection.
pub(crate) fn save_switches(settings: &SwitchSettings, state: &mut UiState) {
    use crate::config::settings::SettingsWriteOutcome;
    match crate::config::settings::save_switches(settings.switches.clone()) {
        SettingsWriteOutcome::Saved => {}
        SettingsWriteOutcome::Denied { message } => state.notify(NotifyKind::Warning, message),
        SettingsWriteOutcome::Failed => state.notify(NotifyKind::Error, "failed to save switches"),
    }
}

pub(crate) fn cycle_switch_name(names: &[String], current: &str, direction: i32) -> String {
    next_choice(names, "default", Some(current), direction)
}

fn parse_model_selection(selection: &str, session: &Session) -> (String, String) {
    let Some(rest) = selection.strip_prefix("model:") else {
        return (session.provider.to_string(), selection.to_string());
    };
    let Some((provider, model)) = rest.split_once(':') else {
        return (session.provider.to_string(), selection.to_string());
    };
    (provider.to_string(), model.to_string())
}

fn switch_items(
    session: &Session,
    settings: &SwitchSettings,
    current_switch: &str,
) -> Vec<SelectItem> {
    let mut items = settings
        .switch_list()
        .into_iter()
        .map(|mode| switch_item(mode, session, settings, current_switch))
        .collect::<Vec<_>>();
    items.push(SelectItem::with_label(
        "switch:configure",
        "configure switches",
        "edit or delete switches",
    ));
    items
}

fn switch_item(
    mode: SwitchConfig,
    session: &Session,
    settings: &SwitchSettings,
    current_switch: &str,
) -> SelectItem {
    let kind = mode.mode.clone().unwrap_or_else(|| String::from("none"));
    let provider = mode
        .provider
        .clone()
        .unwrap_or_else(|| session.provider.to_string());
    let model = mode
        .model
        .clone()
        .unwrap_or_else(|| session.model.to_string());
    let thinking = thinking_label(mode.thinking_level.as_deref(), settings);
    let alias = mode
        .alias
        .as_deref()
        .map(|alias| format!("alias {alias} · "))
        .unwrap_or_default();
    let description = if mode.name == current_switch {
        format!("current · {alias}{kind} · [{provider}] {model} · thinking {thinking}")
    } else {
        format!("{alias}{kind} · [{provider}] {model} · thinking {thinking}")
    };
    SelectItem::with_label(format!("switch:{}", mode.name), mode.name, description)
}

fn thinking_label<'a>(level: Option<&'a str>, settings: &'a SwitchSettings) -> &'a str {
    level
        .or_else(|| settings.default_thinking_level())
        .unwrap_or("off")
}

fn switch_configure_items(settings: &SwitchSettings) -> Vec<SelectItem> {
    let mut items = settings
        .switch_list()
        .into_iter()
        .map(|mode| {
            let description = mode
                .alias
                .as_deref()
                .map(|alias| format!("alias {alias} · configure"))
                .unwrap_or_else(|| String::from("configure"));
            SelectItem::with_label(format!("switch:edit:{}", mode.name), mode.name, description)
        })
        .collect::<Vec<_>>();
    items.push(SelectItem::with_label(
        "switch:add",
        "add switch",
        "create a new switch",
    ));
    items.push(SelectItem::with_label(
        "switch:back",
        "back",
        "return to switches",
    ));
    items
}

fn switch_edit_items(
    mode: &SwitchConfig,
    session: &Session,
    settings: &SwitchSettings,
) -> Vec<SelectItem> {
    let mut items = vec![
        SelectItem::with_label(
            format!("switch:alias:{}", mode.name),
            "alias",
            mode.alias.as_deref().unwrap_or(""),
        ),
        SelectItem::with_label(
            format!("switch:set:{}:provider", mode.name),
            "provider",
            mode.provider
                .as_deref()
                .unwrap_or(session.provider.as_str()),
        ),
        SelectItem::with_label(
            format!("switch:set:{}:model", mode.name),
            "model",
            mode.model.as_deref().unwrap_or(session.model.as_str()),
        ),
        SelectItem::with_label(
            format!("switch:set:{}:thinking", mode.name),
            "thinking level",
            thinking_label(mode.thinking_level.as_deref(), settings),
        ),
        SelectItem::with_label(
            format!("switch:set:{}:mode", mode.name),
            "mode",
            mode.mode.as_deref().unwrap_or("none"),
        ),
        SelectItem::with_label(
            format!("switch:delete:{}", mode.name),
            "delete",
            "remove this switch",
        ),
    ];
    items.push(SelectItem::with_label(
        format!("switch:back:configure:{}", mode.name),
        "back",
        "return to configure switches",
    ));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use clap::Parser;
    use std::sync::Arc;

    #[test]
    fn switch_catalog_choices_preserve_filtering_and_raw_draft_cycling() {
        let root = std::env::temp_dir().join(format!("phi-switch-picker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("models.json");
        std::fs::write(
            &path,
            r#"{"providers":{"local":{"api":"openai-codex-responses","baseUrl":"http://localhost/v1","models":{"test":{},"zed":{}}},"other":{"api":"openai-codex-responses","baseUrl":"http://localhost/v1","models":{"other-model":{}}}}}"#,
        ).unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![path],
            offline: false,
        })
        .unwrap();
        assert!(catalog.diagnostics().is_empty());
        let settings = Settings {
            default_provider: Some("local".into()),
            default_model: Some("default-missing".into()),
            switches: Some(vec![SwitchConfig {
                name: "review".into(),
                mode: Some("default".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let providers = phi_sdk::models::available_providers(
            &catalog,
            &settings.runtime_config(&catalog),
            "other",
        );
        let after_codex = providers[2].as_str();
        let cli = Arc::new(crate::cli::Cli::parse_from(["phi"]));
        let catalog = Arc::new(catalog);
        let resources = crate::runtime::RuntimeResources::new(
            phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
            Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
            catalog.clone(),
            phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
        );
        let preparation = crate::runtime::RuntimePreparation::new(
            crate::runtime::AgentRuntimeFactory::new(resources),
        );
        let settings_manager = crate::config::SettingsManager::from_settings(
            &root,
            settings.clone(),
            Settings::default(),
        )
        .unwrap();
        let models = preparation.initialize(
            &root,
            cli.clone(),
            settings.clone(),
            settings_manager,
            Vec::new(),
            catalog,
        );
        let mut settings = models.picker_seed();
        let session = crate::session::new(
            "other",
            "session-model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut state = UiState::default();
        open_switch_model_picker(&models, "review", &mut state, &session, &settings);
        assert_eq!(
            state
                .picker
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["default-missing", "test", "zed"]
        );
        assert_eq!(
            state.picker.as_ref().unwrap().items[0].description,
            "[local] ✔"
        );
        settings.default_provider = Some("unlisted".into());
        settings.switches.as_mut().unwrap()[0].provider = Some("local".into());
        open_switch_model_picker(&models, "review", &mut state, &session, &settings);
        assert_eq!(
            state
                .picker
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["test", "zed"]
        );
        settings.default_provider = Some("local".into());
        settings.switches.as_mut().unwrap()[0].provider = None;
        for direction in [1, 1] {
            update_switch_field(
                &models,
                "review",
                "model",
                direction,
                &mut state,
                &session,
                &mut settings,
            );
        }
        let raw = &settings.switches.as_ref().unwrap()[0];
        assert_eq!(raw.provider, None);
        assert_eq!(raw.model.as_deref(), Some("other-model"));
        update_switch_field(
            &models,
            "review",
            "model",
            -1,
            &mut state,
            &session,
            &mut settings,
        );
        assert_eq!(
            settings.switches.as_ref().unwrap()[0].model.as_deref(),
            Some("default-missing")
        );
        for (direction, expected) in [(1, "openai-codex"), (1, after_codex), (-1, "openai-codex")] {
            update_switch_field(
                &models,
                "review",
                "provider",
                direction,
                &mut state,
                &session,
                &mut settings,
            );
            assert_eq!(
                settings.switches.as_ref().unwrap()[0].provider.as_deref(),
                Some(expected)
            );
        }
        settings.switches.as_mut().unwrap()[0].model = Some("unknown".into());
        update_switch_field(
            &models,
            "review",
            "model",
            1,
            &mut state,
            &session,
            &mut settings,
        );
        assert_eq!(
            settings.switches.as_ref().unwrap()[0].model.as_deref(),
            Some("default-missing")
        );
        update_switch_field(
            &models,
            "review",
            "thinking",
            1,
            &mut state,
            &session,
            &mut settings,
        );
        assert_eq!(
            settings.switches.as_ref().unwrap()[0]
                .thinking_level
                .as_deref(),
            Some("minimal")
        );
        let before = serde_json::to_value(&settings.switches).unwrap();
        update_switch_field(
            &models,
            "plan",
            "mode",
            1,
            &mut state,
            &session,
            &mut settings,
        );
        assert_eq!(serde_json::to_value(&settings.switches).unwrap(), before);
        assert!(!root.join("store.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
