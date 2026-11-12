use crate::config::SwitchSettings;
use crate::interactive::state::{MenuKind, ModelCatalogNotice, PickerKind, PickerState, UiState};
use crate::session::Session;
use phi_tui::{NotifyKind, SelectItem, StyledLine, ThemeColor, text::display_width};

use super::PickerDraft;
use crate::runtime::attachment::AgentAttachment;
use crate::runtime::preparation::ModelChoices;

const INITIAL_DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::interactive) struct ModelFeatures {
    pub(in crate::interactive) catalog_updates: bool,
}

impl ModelFeatures {
    pub(in crate::interactive) fn command(self, command: crate::commands::ModelCommand) -> bool {
        command != crate::commands::ModelCommand::Update || self.catalog_updates
    }
}

enum ChoiceSource {
    Runtime,
    Catalog {
        models: ModelChoices,
        draft: PickerDraft,
    },
}

pub(in crate::interactive) struct ModelPicker {
    choices: ChoiceSource,
    updates: Option<CatalogUpdates>,
    default_configuration: Option<DefaultModelConfiguration>,
}

pub(in crate::interactive) struct DefaultModelConfiguration {
    draft: PickerDraft,
}

impl DefaultModelConfiguration {
    pub(in crate::interactive) fn new(draft: PickerDraft) -> Self {
        Self { draft }
    }

    pub(in crate::interactive) fn prepare(&self, state: &mut UiState) {
        let mut settings = self.draft.borrow_mut();
        if settings.default_model().is_some() {
            return;
        }
        settings.default_model = Some(INITIAL_DEFAULT_MODEL.into());
        use crate::config::settings::{SettingsWriteOutcome, persist_default_model};
        match persist_default_model(INITIAL_DEFAULT_MODEL.into()) {
            SettingsWriteOutcome::Saved => {
                state.notify(NotifyKind::Info, "generated defaultModel setting")
            }
            SettingsWriteOutcome::Denied { message } => state.notify(NotifyKind::Warning, message),
            SettingsWriteOutcome::Failed => {
                state.notify(NotifyKind::Error, "failed to generate defaultModel setting")
            }
        }
    }
}

pub(in crate::interactive) struct CatalogUpdates {
    models: ModelChoices,
    results: tokio::sync::mpsc::UnboundedReceiver<ModelUpdateResult>,
    sender: tokio::sync::mpsc::UnboundedSender<ModelUpdateResult>,
    discovery: tokio::sync::mpsc::UnboundedReceiver<ModelDiscoveryResult>,
    discovery_sender: tokio::sync::mpsc::UnboundedSender<ModelDiscoveryResult>,
}

impl CatalogUpdates {
    pub(in crate::interactive) fn new(models: ModelChoices) -> Self {
        let (sender, results) = tokio::sync::mpsc::unbounded_channel();
        let (discovery_sender, discovery) = tokio::sync::mpsc::unbounded_channel();
        Self {
            models,
            results,
            sender,
            discovery,
            discovery_sender,
        }
    }
}

impl ModelPicker {
    pub(in crate::interactive) fn runtime() -> Self {
        Self {
            choices: ChoiceSource::Runtime,
            updates: None,
            default_configuration: None,
        }
    }

    pub(in crate::interactive) fn catalog(models: ModelChoices, draft: PickerDraft) -> Self {
        Self {
            choices: ChoiceSource::Catalog { models, draft },
            updates: None,
            default_configuration: None,
        }
    }

    pub(in crate::interactive) fn with_updates(mut self, updates: CatalogUpdates) -> Self {
        self.updates = Some(updates);
        self
    }

    pub(in crate::interactive) fn with_default_configuration(
        mut self,
        configuration: DefaultModelConfiguration,
    ) -> Self {
        self.default_configuration = Some(configuration);
        self
    }

    pub(in crate::interactive) fn features(&self) -> ModelFeatures {
        ModelFeatures {
            catalog_updates: self.updates.is_some(),
        }
    }

    pub(in crate::interactive) async fn open(
        &self,
        attachment: &AgentAttachment,
        state: &mut UiState,
        session: &Session,
    ) {
        if let Some(configuration) = &self.default_configuration {
            configuration.prepare(state);
        }
        let choices = match &self.choices {
            ChoiceSource::Catalog { models, draft } => models.model_choices(
                session.provider.as_str(),
                session.model.as_str(),
                draft.borrow().default_model(),
            ),
            ChoiceSource::Runtime => match attachment.runtime_catalog().await {
                Ok(catalog) => catalog.models,
                Err(error) => {
                    state.notify(
                        NotifyKind::Error,
                        format!("failed to load runtime choices: {error}"),
                    );
                    return;
                }
            },
        };
        open_model_choices(choices, state, session);
    }

    pub(in crate::interactive) fn start_updates(&self, attachment: &AgentAttachment) {
        let Some(updates) = &self.updates else { return };
        if updates.models.automatic_model_catalog_update_enabled() {
            spawn_model_update(
                updates.models.clone(),
                CatalogUpdateKind::Automatic,
                updates.sender.clone(),
            );
            spawn_model_discovery(attachment.clone(), updates.discovery_sender.clone());
        }
    }

    pub(in crate::interactive) fn update_catalog(&self, state: &mut UiState) {
        let Some(updates) = &self.updates else {
            state.notify(
                NotifyKind::Warning,
                "Catalog updates are unavailable in this interface.",
            );
            return;
        };
        state.notify(NotifyKind::Info, "refreshing model catalogs...");
        spawn_model_update(
            updates.models.clone(),
            CatalogUpdateKind::Manual,
            updates.sender.clone(),
        );
    }

    pub(in crate::interactive) fn poll(&mut self, state: &mut UiState, session: &Session) -> bool {
        let Some(updates) = &mut self.updates else {
            return false;
        };
        let mut changed = false;
        while let Ok((kind, result)) = updates.results.try_recv() {
            changed = true;
            let notice_kind = apply_model_update_result(state, &result);
            if matches!(kind, CatalogUpdateKind::Manual)
                && let Some(notice) = &state.model_catalog_notice
            {
                state.notify(notice_kind, notice.message.clone());
            }
        }
        while let Ok(result) = updates.discovery.try_recv() {
            changed |= result.is_ok();
        }
        if changed && let ChoiceSource::Catalog { models, draft } = &self.choices {
            refresh_open_model_picker(models, state, session, &draft.borrow());
        }
        changed
    }
}

enum CatalogUpdateKind {
    Automatic,
    Manual,
}

type ModelUpdateResult = (
    CatalogUpdateKind,
    Result<phi_sdk::models::CatalogUpdateResult, String>,
);
type ModelDiscoveryResult = Result<phi_sdk::models::ModelDiscoveryResult, String>;

fn spawn_model_discovery(
    attachment: crate::runtime::attachment::AgentAttachment,
    tx: tokio::sync::mpsc::UnboundedSender<ModelDiscoveryResult>,
) {
    tokio::spawn(async move {
        let result = attachment
            .discover_models()
            .await
            .map_err(|error| error.to_string());
        let _ = tx.send(result);
    });
}

fn spawn_model_update(
    models: crate::runtime::preparation::ModelChoices,
    kind: CatalogUpdateKind,
    tx: tokio::sync::mpsc::UnboundedSender<ModelUpdateResult>,
) {
    tokio::spawn(async move {
        let result = models
            .update_model_catalog(matches!(kind, CatalogUpdateKind::Manual))
            .await
            .map_err(|error| error.to_string());
        let _ = tx.send((kind, result));
    });
}

fn apply_model_update_result(
    state: &mut UiState,
    result: &Result<phi_sdk::models::CatalogUpdateResult, String>,
) -> NotifyKind {
    let (kind, message) = match result {
        Ok(result) if result.errors.is_empty() => (
            NotifyKind::Success,
            String::from("Model catalogs refreshed."),
        ),
        Ok(result) if !result.updated.is_empty() || !result.unchanged.is_empty() => (
            NotifyKind::Warning,
            format!(
                "Model catalogs refreshed with warnings: {}",
                format_errors(result)
            ),
        ),
        Ok(result) => (
            NotifyKind::Error,
            format!("Model catalog refresh failed: {}", format_errors(result)),
        ),
        Err(error) => (
            NotifyKind::Error,
            format!("Model catalog refresh failed: {error}"),
        ),
    };
    state.model_catalog_notice = Some(ModelCatalogNotice { kind, message });
    kind
}

fn format_errors(result: &phi_sdk::models::CatalogUpdateResult) -> String {
    result
        .errors
        .iter()
        .map(|error| format!("{}: {}", error.source, error.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn refresh_open_model_picker(
    models: &crate::runtime::preparation::ModelChoices,
    state: &mut UiState,
    session: &Session,
    settings: &SwitchSettings,
) {
    if !state
        .picker
        .as_ref()
        .is_some_and(|picker| matches!(picker.kind, PickerKind::Model))
    {
        return;
    }
    let items = read_model_items(models, session, settings);
    state.picker.as_mut().unwrap().set_items(items);
}

fn open_model_picker(state: &mut UiState, items: Vec<SelectItem>) {
    state.open_picker(
        MenuKind::Other,
        PickerState {
            filterable: true,
            ..PickerState::with_kind(PickerKind::Model, "select model", items)
        },
    );
    state.runtime.request_render();
}

pub(crate) fn open_model_choices(
    choices: Vec<crate::runtime::preparation::ModelChoice>,
    state: &mut UiState,
    session: &Session,
) {
    open_model_picker(
        state,
        choices
            .into_iter()
            .map(|choice| model_item(session, choice))
            .collect(),
    );
}

fn read_model_items(
    models: &crate::runtime::preparation::ModelChoices,
    session: &Session,
    settings: &SwitchSettings,
) -> Vec<SelectItem> {
    models
        .model_choices(
            session.provider.as_str(),
            session.model.as_str(),
            settings.default_model(),
        )
        .into_iter()
        .map(|choice| model_item(session, choice))
        .collect()
}

fn model_item(session: &Session, choice: crate::runtime::preparation::ModelChoice) -> SelectItem {
    let provider = choice.provider.as_str();
    let model = choice.model.as_str();
    if !choice.complete {
        return SelectItem::with_label(
            format!("model-incomplete:{provider}:{model}"),
            model,
            format!("[{provider}] incomplete — add to models.json"),
        );
    }
    let current = provider == session.provider.as_str() && model == session.model.as_str();
    let description = if current {
        format!("[{provider}] ✔")
    } else {
        format!("[{provider}]")
    };
    SelectItem::with_label(format!("model:{provider}:{model}"), model, description)
}

pub(super) fn render_items(picker: &PickerState, width: u16) -> Vec<StyledLine> {
    let selected = picker
        .menu
        .selected
        .min(picker.items.len().saturating_sub(1));
    let range = phi_tui::centered_visible_range(selected, picker.items.len(), 5);
    let mut lines = Vec::new();
    for (offset, item) in picker.items[range.range()].iter().enumerate() {
        let row = range.start + offset;
        lines.push(render_model_item(item, row == selected, width));
    }
    if picker.items.len() > range.len() {
        lines.push(
            StyledLine::new(
                format!("  ({}/{})", selected + 1, picker.items.len()),
                ThemeColor::Dim,
            )
            .fit(width),
        );
    }
    lines
}

fn render_model_item(item: &SelectItem, selected: bool, width: u16) -> StyledLine {
    let prefix = if selected { "→ " } else { "  " };
    let provider = item.description.trim_end_matches(" ✔");
    let has_check = item.description.ends_with(" ✔");
    let reserved = display_width(prefix) + display_width(provider) + usize::from(has_check) * 2 + 1;
    let name_width = (width as usize).saturating_sub(reserved).max(1);
    let name = phi_tui::text::truncate(&item.label, name_width);
    let name_color = if selected {
        ThemeColor::Accent
    } else {
        ThemeColor::Default
    };
    let mut segments = vec![
        (prefix.to_string(), ThemeColor::Muted),
        (name, name_color),
        (String::from(" "), ThemeColor::Default),
        (provider.to_string(), ThemeColor::Dim),
    ];
    if has_check {
        segments.push((String::from(" ✔"), ThemeColor::Success));
    }
    StyledLine::segments(segments).fit(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use clap::Parser;
    use std::sync::Arc;

    #[test]
    fn catalog_updates_preserve_picker_and_manual_notices() {
        let (root, policy, _) = crate::runtime::preparation::tests::session_test_runtime();
        let models = policy.model_choices_owner();
        let draft = std::rc::Rc::new(std::cell::RefCell::new(models.picker_seed()));
        draft.borrow_mut().default_model = None;
        let updates = CatalogUpdates::new(models.clone());
        updates
            .sender
            .send((CatalogUpdateKind::Automatic, Err("offline".into())))
            .unwrap();
        let mut picker = ModelPicker::catalog(models, draft.clone()).with_updates(updates);
        let session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        let mut state = UiState::default();
        open_model_picker(&mut state, Vec::new());
        state.picker.as_mut().unwrap().query = "test".into();
        state.editor.set_text("unsent draft");
        assert!(picker.poll(&mut state, &session));
        assert!(!picker.poll(&mut state, &session));
        assert!(state.conversation.blocks.is_empty());
        assert!(
            state
                .model_catalog_notice
                .as_ref()
                .unwrap()
                .message
                .contains("offline")
        );
        picker
            .updates
            .as_ref()
            .unwrap()
            .sender
            .send((CatalogUpdateKind::Manual, Err("still offline".into())))
            .unwrap();
        assert!(picker.poll(&mut state, &session));
        assert_eq!(state.picker.as_ref().unwrap().query, "test");
        assert_eq!(state.editor.text(), "unsent draft");
        assert!(draft.borrow().default_model.is_none());
        assert!(
            matches!(&state.conversation.blocks.last().unwrap().block.body,
            phi_tui::BlockBody::Notify(notice) if notice.kind == NotifyKind::Error && notice.message.contains("still offline"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_picker_preserves_order_dedup_and_incomplete_discovery() {
        let root = std::env::temp_dir().join(format!("phi-model-picker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("models.json");
        std::fs::write(
            &path,
            r#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{}}},"other":{"api":"openai-codex-responses","baseUrl":"http://localhost/v1","models":{"other-model":{}}}}}"#,
        )
        .unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![path],
            offline: false,
        })
        .unwrap();
        assert!(catalog.diagnostics().is_empty());
        for (provider, known) in [("local", "test"), ("other", "other-model")] {
            catalog
                .record_discovery(provider, [known.into(), "unknown".into()])
                .unwrap();
        }
        let mut settings = Settings::default();
        settings.default_model = Some("default-missing".into());
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
        let mut session = crate::session::new(
            "local",
            "current-missing",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut state = UiState::default();
        let items = read_model_items(&models, &session, &settings);
        assert_eq!(
            items
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            [
                "model-incomplete:local:current-missing",
                "model-incomplete:local:default-missing",
                "model:local:test",
                "model:other:other-model",
                "model-incomplete:local:unknown",
                "model-incomplete:other:unknown",
            ]
        );
        assert_eq!(
            items[0].description,
            "[local] incomplete — add to models.json"
        );
        session.model = "test".into();
        settings.default_model = Some("test".into());
        let items = read_model_items(&models, &session, &settings);
        assert_eq!(items[0].value, "model:local:test");
        assert_eq!(items[0].description, "[local] ✔");
        assert_eq!(items.len(), 4);
        open_model_picker(&mut state, items);
        state.picker.as_mut().unwrap().query = "test".into();
        refresh_open_model_picker(&models, &mut state, &session, &settings);
        assert_eq!(state.picker.as_ref().unwrap().query, "test");
        std::fs::remove_dir_all(root).unwrap();
    }
}
