use super::{Settings, SwitchConfig};

#[derive(Debug, Clone, Default)]
pub(crate) struct SwitchSettings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub switches: Option<Vec<SwitchConfig>>,
}

impl From<&Settings> for SwitchSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            default_provider: settings.default_provider.clone(),
            default_model: settings.default_model.clone(),
            default_thinking_level: settings.default_thinking_level.clone(),
            switches: settings.switches.clone(),
        }
    }
}

impl SwitchSettings {
    pub(crate) fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    pub(crate) fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    pub(crate) fn default_thinking_level(&self) -> Option<&str> {
        self.default_thinking_level.as_deref()
    }

    pub(crate) fn default_switch_config(&self) -> SwitchConfig {
        self.builtin_switches()
            .into_iter()
            .find(|mode| mode.name == "default")
            .unwrap_or_default()
    }

    pub(crate) fn custom_switches(&self) -> Vec<SwitchConfig> {
        self.switches.clone().unwrap_or_default()
    }

    pub(crate) fn edit_switch<R>(
        &mut self,
        name: &str,
        edit: impl FnOnce(&Self, &mut SwitchConfig) -> R,
    ) -> Option<R> {
        let mut switches = self.custom_switches();
        let index = match switches.iter().position(|switch| switch.name == name) {
            Some(index) => index,
            None => {
                switches.push(self.switch_by_name(name)?);
                switches.len() - 1
            }
        };
        let result = edit(self, &mut switches[index]);
        self.switches = Some(switches);
        Some(result)
    }

    pub(crate) fn switch_list(&self) -> Vec<SwitchConfig> {
        let custom_switches = self.custom_switches();
        let mut modes = self
            .builtin_switches()
            .into_iter()
            .map(|builtin| self.resolve_builtin_override(builtin, &custom_switches))
            .collect::<Vec<_>>();
        modes.extend(
            custom_switches
                .iter()
                .filter(|mode| !self.is_builtin_switch_name(&mode.name))
                .cloned()
                .map(|mode| self.resolve_switch_defaults(mode, &custom_switches)),
        );
        modes
    }

    pub(crate) fn switch_names(&self) -> Vec<String> {
        self.switch_list()
            .into_iter()
            .map(|switch| switch.name)
            .collect()
    }

    pub(crate) fn switch_by_name(&self, name: &str) -> Option<SwitchConfig> {
        self.switch_list()
            .into_iter()
            .find(|mode| mode.name == name)
    }

    pub(crate) fn switch_by_name_or_alias(&self, value: &str) -> Option<SwitchConfig> {
        let switches = self.switch_list();
        switches
            .iter()
            .find(|mode| mode.name == value)
            .cloned()
            .or_else(|| {
                switches
                    .into_iter()
                    .find(|mode| mode.alias.as_deref() == Some(value))
            })
    }

    pub(crate) fn switch_mode_names(&self) -> Vec<String> {
        let mut names = vec![String::from("none")];
        names.extend(
            self.builtin_switches()
                .into_iter()
                .filter_map(|mode| mode.mode),
        );
        for mode in self
            .custom_switches()
            .into_iter()
            .filter_map(|mode| mode.mode)
        {
            if !names.iter().any(|name| name == &mode) {
                names.push(mode);
            }
        }
        names
    }

    fn resolve_builtin_override(
        &self,
        mut builtin: SwitchConfig,
        custom_switches: &[SwitchConfig],
    ) -> SwitchConfig {
        if let Some(custom) = custom_switches
            .iter()
            .find(|mode| mode.name == builtin.name)
        {
            builtin.alias = custom.alias.clone().or(builtin.alias);
            builtin.provider = custom.provider.clone().or(builtin.provider);
            builtin.model = custom.model.clone().or(builtin.model);
            builtin.thinking_level = custom.thinking_level.clone().or(builtin.thinking_level);
            builtin.prompt = custom.prompt.clone().or(builtin.prompt);
        }
        builtin
    }

    fn resolve_switch_defaults(
        &self,
        mut mode: SwitchConfig,
        custom_switches: &[SwitchConfig],
    ) -> SwitchConfig {
        let Some(mode_name) = mode.mode.clone() else {
            return mode;
        };
        if let Some(builtin) = self
            .builtin_switches()
            .into_iter()
            .find(|builtin| builtin.mode.as_deref() == Some(mode_name.as_str()))
        {
            mode.provider = mode.provider.or(builtin.provider);
            mode.model = mode.model.or(builtin.model);
            mode.thinking_level = mode.thinking_level.or(builtin.thinking_level);
        }
        mode.prompt = mode
            .prompt
            .or_else(|| self.prompt_for_switch_mode(&mode_name, custom_switches));
        mode
    }

    fn prompt_for_switch_mode(
        &self,
        mode_name: &str,
        custom_switches: &[SwitchConfig],
    ) -> Option<String> {
        self.builtin_switches()
            .into_iter()
            .find(|builtin| builtin.mode.as_deref() == Some(mode_name))
            .map(|builtin| self.resolve_builtin_override(builtin, custom_switches))
            .and_then(|builtin| builtin.prompt)
            .or_else(|| {
                custom_switches
                    .iter()
                    .find(|mode| {
                        mode.prompt.is_some()
                            && (mode.mode.as_deref() == Some(mode_name) || mode.name == mode_name)
                    })
                    .and_then(|mode| mode.prompt.clone())
            })
    }

    pub(crate) fn is_builtin_switch_name(&self, name: &str) -> bool {
        self.builtin_switches().iter().any(|mode| mode.name == name)
    }

    fn builtin_switches(&self) -> Vec<SwitchConfig> {
        vec![
            SwitchConfig {
                name: String::from("default"),
                alias: None,
                provider: self.default_provider.clone(),
                model: self.default_model.clone(),
                thinking_level: self.default_thinking_level.clone(),
                mode: Some(String::from("default")),
                prompt: None,
            },
            SwitchConfig {
                name: String::from("plan"),
                alias: None,
                provider: self.default_provider.clone(),
                model: self.default_model.clone(),
                thinking_level: self.default_thinking_level.clone(),
                mode: Some(String::from("plan")),
                prompt: None,
            },
        ]
    }
}
