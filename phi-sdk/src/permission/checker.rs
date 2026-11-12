use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permission::command::CommandPermissionRequest;
use crate::permission::pattern::Pattern;
use crate::permission::shell::{ShellAnalyzer, ShellOperation};
use crate::permission::{Action, PermissionConfig, PermissionMode, ToolPerm};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

#[derive(Debug, Clone)]
pub enum CheckResult {
    Allowed,
    Ask,
    Denied(String),
}

struct SessionGrant {
    tool: String,
    pattern: String,
    matcher: Pattern,
}

pub struct PermissionChecker {
    rules: HashMap<String, Vec<(Pattern, Action)>>,
    ext_dir_rules: Vec<(Pattern, Action)>,
    non_interactive_ask: Action,
    working_dir: String,
    session_allowlist: Vec<SessionGrant>,
    shell_analyzer: ShellAnalyzer,
    mode: PermissionMode,
}

impl PermissionChecker {
    pub fn new(config: &PermissionConfig, working_dir: Option<std::path::PathBuf>) -> Self {
        let mut rules = HashMap::new();
        for (name, permission) in configured_tools(config) {
            let entries = rules
                .entry(name.to_ascii_lowercase())
                .or_insert_with(Vec::new);
            entries.extend(compile_rules(permission));
        }
        Self {
            rules,
            ext_dir_rules: config
                .external_directory
                .as_ref()
                .map(compile_patterns)
                .unwrap_or_default(),
            non_interactive_ask: config.non_interactive_ask.unwrap_or(Action::Deny),
            working_dir: working_dir
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .to_string_lossy()
                .into_owned(),
            session_allowlist: Vec::new(),
            shell_analyzer: ShellAnalyzer::new(),
            mode: config.mode,
        }
    }

    #[cfg(test)]
    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        self.check_with_preapprovals(tool, input, &[])
    }

    pub(crate) fn check_with_preapprovals(
        &mut self,
        tool: &str,
        input: &str,
        preapprovals: &[(String, String)],
    ) -> CheckResult {
        let tool = tool.to_ascii_lowercase();
        if tool == "bash" {
            return self.check_shell(input, preapprovals);
        }
        self.resolve(
            self.matching_actions(&tool, input),
            self.has_matching_approval(&tool, input, preapprovals),
        )
    }

    #[cfg(test)]
    pub fn check_command(&mut self, request: &CommandPermissionRequest<'_>) -> CheckResult {
        self.check_command_with_preapprovals(request, &[])
    }

    pub(crate) fn check_command_with_preapprovals(
        &mut self,
        request: &CommandPermissionRequest<'_>,
        preapprovals: &[(String, String)],
    ) -> CheckResult {
        if let Some(script) = request.shell_script() {
            self.check_shell(script, preapprovals)
        } else {
            self.check_shell_operation(
                &ShellOperation {
                    aliases: request.permission_inputs(),
                },
                false,
                preapprovals,
            )
        }
    }

    #[cfg(test)]
    pub fn check_shell_evidence<'a>(
        &mut self,
        commands: impl IntoIterator<Item = &'a str>,
    ) -> CheckResult {
        self.check_shell_evidence_with_preapprovals(commands, &[])
    }

    pub(crate) fn check_shell_evidence_with_preapprovals<'a>(
        &mut self,
        commands: impl IntoIterator<Item = &'a str>,
        preapprovals: &[(String, String)],
    ) -> CheckResult {
        commands
            .into_iter()
            .map(|command| self.check_shell(command, preapprovals))
            .fold(CheckResult::Allowed, strictest_result)
    }

    fn check_shell(&mut self, input: &str, preapprovals: &[(String, String)]) -> CheckResult {
        if self.mode == PermissionMode::Yolo {
            return CheckResult::Allowed;
        }
        let approved = self.has_matching_approval("bash", input, preapprovals);
        let analysis = self.shell_analyzer.analyze(input);
        let result = analysis
            .operations
            .iter()
            .map(|operation| self.check_shell_operation(operation, approved, preapprovals))
            .fold(CheckResult::Allowed, strictest_result);
        if analysis.ambiguous || analysis.operations.is_empty() {
            strictest_result(
                result,
                self.check_shell_operation(
                    &ShellOperation { aliases: vec![] },
                    approved,
                    preapprovals,
                ),
            )
        } else {
            result
        }
    }

    fn check_shell_operation(
        &self,
        operation: &ShellOperation,
        approved: bool,
        preapprovals: &[(String, String)],
    ) -> CheckResult {
        let inputs = operation
            .aliases
            .iter()
            .map(String::as_str)
            .chain(operation.aliases.is_empty().then_some(""));
        let actions = inputs.flat_map(|input| self.matching_actions("bash", input));
        let approved = approved
            || operation
                .aliases
                .iter()
                .any(|input| self.has_matching_approval("bash", input, preapprovals));
        self.resolve(actions, approved)
    }

    fn matching_actions<'a>(
        &'a self,
        tool: &str,
        input: &'a str,
    ) -> impl Iterator<Item = Action> + 'a {
        self.rules
            .get(tool)
            .into_iter()
            .flatten()
            .filter(move |(pattern, _)| pattern.matches(input))
            .map(|(_, action)| *action)
    }

    fn resolve(&self, actions: impl IntoIterator<Item = Action>, approved: bool) -> CheckResult {
        if self.mode == PermissionMode::Yolo {
            return CheckResult::Allowed;
        }
        let action = strictest_action(actions).unwrap_or(match self.mode {
            PermissionMode::Allow => Action::Allow,
            PermissionMode::Deny => Action::Deny,
            PermissionMode::Ask | PermissionMode::Yolo => Action::Ask,
        });
        check_result(if action == Action::Ask && approved {
            Action::Allow
        } else {
            action
        })
    }

    #[cfg(test)]
    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        self.check_path_with_preapprovals(tool, path, &[])
    }

    pub(crate) fn check_path_with_preapprovals(
        &mut self,
        tool: &str,
        path: &str,
        preapprovals: &[(String, String)],
    ) -> CheckResult {
        let tool = tool.to_ascii_lowercase();
        let absolute = resolve_absolute(path, &self.working_dir);
        let actions = self
            .matching_actions(&tool, path)
            .chain(self.matching_actions(&tool, &absolute));
        let is_external = !Path::new(&absolute).starts_with(&self.working_dir);
        let external = self
            .ext_dir_rules
            .iter()
            .filter(|(pattern, _)| is_external && pattern.matches(&absolute))
            .map(|(_, action)| *action);
        self.resolve(
            actions.chain(external),
            self.has_matching_approval(&tool, path, preapprovals)
                || self.has_matching_approval(&tool, &absolute, preapprovals),
        )
    }

    fn has_matching_approval(
        &self,
        tool: &str,
        input: &str,
        preapprovals: &[(String, String)],
    ) -> bool {
        self.session_allowlist
            .iter()
            .any(|grant| grant.tool.eq_ignore_ascii_case(tool) && grant.matcher.matches(input))
            || preapprovals.iter().any(|(name, pattern)| {
                name.eq_ignore_ascii_case(tool)
                    && (pattern == "**" || Pattern::new(pattern).matches(input))
            })
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern: &str) {
        if self
            .session_allowlist
            .iter()
            .any(|grant| grant.tool == tool && grant.pattern == pattern)
        {
            return;
        }
        self.session_allowlist.push(SessionGrant {
            tool,
            pattern: pattern.to_string(),
            matcher: Pattern::new(pattern),
        });
    }

    pub(crate) fn session_allowlist(&self) -> Vec<(String, String)> {
        self.session_allowlist
            .iter()
            .map(|grant| (grant.tool.clone(), grant.pattern.clone()))
            .collect()
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        for (tool, pattern) in entries {
            self.add_session_allowlist(tool.clone(), pattern);
        }
    }

    pub fn non_interactive_ask(&self) -> Action {
        self.non_interactive_ask
    }
}

fn configured_tools(config: &PermissionConfig) -> impl Iterator<Item = (&str, &ToolPerm)> {
    [
        ("bash", config.bash.as_ref()),
        ("worktree", config.worktree.as_ref()),
        ("read", config.read.as_ref()),
        ("write", config.write.as_ref()),
        ("edit", config.edit.as_ref()),
        ("mcp", config.mcp.as_ref()),
        ("grep", config.grep.as_ref()),
        ("glob", config.glob.as_ref()),
        ("find", config.find.as_ref()),
        ("ls", config.ls.as_ref()),
        ("write_todo_list", config.write_todo_list.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, permission)| permission.map(|permission| (name, permission)))
    .chain(
        config
            .tools
            .iter()
            .map(|(name, permission)| (name.as_str(), permission)),
    )
}

fn compile_rules(permission: &ToolPerm) -> Vec<(Pattern, Action)> {
    match permission {
        ToolPerm::Simple(action) => vec![(Pattern::new("**"), *action)],
        ToolPerm::Granular(patterns) => compile_patterns(patterns),
    }
}

fn compile_patterns(patterns: &HashMap<String, Action>) -> Vec<(Pattern, Action)> {
    patterns
        .iter()
        .map(|(pattern, action)| (Pattern::new(pattern), *action))
        .collect()
}

fn check_result(action: Action) -> CheckResult {
    match action {
        Action::Allow => CheckResult::Allowed,
        Action::Ask => CheckResult::Ask,
        Action::Deny => CheckResult::Denied("Blocked by permission rules".to_string()),
    }
}

fn strictest_result(current: CheckResult, next: CheckResult) -> CheckResult {
    match (current, next) {
        (denied @ CheckResult::Denied(_), _) | (_, denied @ CheckResult::Denied(_)) => denied,
        (CheckResult::Ask, _) | (_, CheckResult::Ask) => CheckResult::Ask,
        _ => CheckResult::Allowed,
    }
}

fn strictest_action(actions: impl IntoIterator<Item = Action>) -> Option<Action> {
    actions.into_iter().max_by_key(|action| match action {
        Action::Allow => 0,
        Action::Ask => 1,
        Action::Deny => 2,
    })
}

fn resolve_absolute(path: &str, working_dir: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        Path::new(working_dir)
            .join(path)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(value: serde_json::Value) -> PermissionChecker {
        let config = PermissionConfig::try_from_value(value).unwrap();
        PermissionChecker::new(&config, Some("/workspace".into()))
    }

    #[test]
    fn unified_modes_apply_to_generic_paths_and_commands() {
        for (mode, expected) in [
            ("ask", Action::Ask),
            ("allow", Action::Allow),
            ("deny", Action::Deny),
            ("yolo", Action::Allow),
        ] {
            let mut checker = checker(serde_json::json!({"mode":mode}));
            for result in [
                checker.check("mcp", "server/tool"),
                checker.check_path("grep", "/outside/file"),
                checker.check("bash", "rm file"),
            ] {
                assert_action(result, expected);
            }
        }
    }

    #[test]
    fn explicit_rules_precede_fallback_and_grants_cannot_override_deny() {
        for mode in ["ask", "allow", "deny", "yolo"] {
            let mut checker = checker(
                serde_json::json!({"mode":mode, "read":{"allowed":"allow", "blocked":"deny", "prompted":"ask"}, "bash":{"rm **":"deny"}}),
            );
            for (tool, input) in [
                ("read", "blocked"),
                ("read", "prompted"),
                ("bash", "rm file"),
            ] {
                checker.add_session_allowlist(tool.into(), input);
            }
            assert_action(checker.check("read", "allowed"), Action::Allow);
            assert_action(checker.check("read", "prompted"), Action::Allow);
            let expected = if mode == "yolo" {
                Action::Allow
            } else {
                Action::Deny
            };
            assert_action(checker.check("read", "blocked"), expected);
            assert_action(checker.check_path("read", "blocked"), expected);
            assert_action(checker.check("bash", "rm file"), expected);
            assert_action(
                checker.check("read", "ungranted"),
                if mode == "yolo" {
                    Action::Allow
                } else {
                    match mode {
                        "allow" => Action::Allow,
                        "deny" => Action::Deny,
                        _ => Action::Ask,
                    }
                },
            );
        }
    }

    #[test]
    fn grants_only_resolve_ask_not_a_deny_fallback() {
        let mut checker = checker(serde_json::json!({"mode":"deny", "read":{"prompted":"ask"}}));
        checker.add_session_allowlist("read".into(), "**");
        assert_action(checker.check("read", "prompted"), Action::Allow);
        assert_action(checker.check("read", "unmatched"), Action::Deny);
    }

    #[test]
    fn overlapping_explicit_rules_remain_strictest_in_allow_mode() {
        let mut checker = checker(
            serde_json::json!({"mode":"allow", "bash":{"git **":"allow", "git -C ** push **":"ask", "git -C /protected push **":"deny"}}),
        );
        assert_action(checker.check("bash", "git status"), Action::Allow);
        assert_action(checker.check("bash", "git -C /repo push main"), Action::Ask);
        assert_action(
            checker.check("bash", "git -C /protected push main"),
            Action::Deny,
        );
    }

    #[test]
    fn nested_commands_and_original_evidence_preserve_denials() {
        let mut checker = checker(
            serde_json::json!({"mode":"allow", "bash":{"git push **":"ask", "rm **":"deny"}}),
        );
        checker.add_session_allowlist("bash".into(), "**");
        assert_action(
            checker.check("bash", "echo $(env MODE=test command /usr/bin/rm file)"),
            Action::Deny,
        );
        assert_action(
            checker.check_shell_evidence(["rm file", "echo rewritten"]),
            Action::Deny,
        );
    }

    #[test]
    fn direct_argv_is_not_a_shell_and_explicit_shells_are_inspected() {
        let mut checker = checker(
            serde_json::json!({"mode":"allow", "bash":{"git push **":"ask", "rm **":"deny"}}),
        );
        let args = vec!["rewrite".into(), "echo; rm file".into()];
        assert_action(
            checker.check_command(&CommandPermissionRequest::new("rtk", &args)),
            Action::Allow,
        );
        let args = vec!["push".into(), "main".into()];
        assert_action(
            checker.check_command(&CommandPermissionRequest::new("/usr/bin/git", &args)),
            Action::Ask,
        );
        let args = vec!["-c".into(), "sh -c 'rm file'".into()];
        assert_action(
            checker.check_command(&CommandPermissionRequest::new("bash", &args)),
            Action::Deny,
        );
    }

    #[test]
    fn ambiguous_execution_uses_the_same_fallback() {
        for (mode, expected) in [
            ("ask", Action::Ask),
            ("allow", Action::Allow),
            ("deny", Action::Deny),
            ("yolo", Action::Allow),
        ] {
            let mut checker = checker(serde_json::json!({"mode":mode}));
            for command in ["$COMMAND push", "bash -c \"$SCRIPT\""] {
                assert_action(checker.check("bash", command), expected);
            }
        }
    }

    #[test]
    fn paths_match_raw_resolved_and_explicit_external_rules() {
        let mut checker = checker(
            serde_json::json!({"mode":"allow", "grep":{"/workspace/secret":"deny"}, "read":{"relative":"ask"}, "external_directory":{"/outside/**":"deny"}}),
        );
        assert_action(checker.check_path("Grep", "secret"), Action::Deny);
        assert_action(checker.check_path("read", "relative"), Action::Ask);
        checker.add_session_allowlist("read".into(), "**");
        assert_action(checker.check_path("read", "/outside/file"), Action::Deny);
        assert_action(checker.check_path("read", "/other/file"), Action::Allow);
    }

    #[test]
    fn restoration_is_additive_exact_pair_idempotent_and_tool_scoped() {
        let mut checker = checker(serde_json::json!({}));
        checker.add_session_allowlist("read".into(), "live.txt");
        let restored = vec![("read".into(), "saved.txt".into())];
        checker.load_session_allowlist(&restored);
        checker.load_session_allowlist(&restored);
        assert_eq!(checker.session_allowlist.len(), 2);
        for input in ["live.txt", "saved.txt"] {
            assert_action(checker.check("read", input), Action::Allow);
        }
        assert_action(checker.check("bash", "saved.txt"), Action::Ask);
    }

    #[test]
    fn dynamic_and_builtin_rules_share_one_strictest_policy() {
        let mut checker = checker(
            serde_json::json!({"mode":"allow", "read":"allow", "tools":{"Read":{"secret":"deny"}, "custom":{"**":"ask"}}}),
        );
        assert_action(checker.check_path("Read", "secret"), Action::Deny);
        assert_action(checker.check("custom", "{}"), Action::Ask);
    }

    fn assert_action(result: CheckResult, expected: Action) {
        let actual = match result {
            CheckResult::Allowed => Action::Allow,
            CheckResult::Ask => Action::Ask,
            CheckResult::Denied(_) => Action::Deny,
        };
        assert_eq!(actual, expected);
    }
}
