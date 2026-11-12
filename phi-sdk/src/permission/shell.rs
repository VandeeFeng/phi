use std::path::Path;

const MAX_RECURSION_DEPTH: usize = 8;
const TRANSPARENT_WRAPPERS: &[&str] = &["command", "exec", "env", "nohup"];
const EXECUTING_WRAPPERS: &[&str] = &["doas", "parallel", "pkexec", "sudo", "xargs"];
const SHELLS: &[&str] = &["bash", "dash", "ksh", "sh", "zsh"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellOperation {
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ShellAnalysis {
    pub operations: Vec<ShellOperation>,
    pub ambiguous: bool,
}

pub(crate) struct ShellAnalyzer {
    parser: Option<tree_sitter::Parser>,
}

impl ShellAnalyzer {
    pub(crate) fn new() -> Self {
        let mut parser = tree_sitter::Parser::new();
        let parser = parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .is_ok()
            .then_some(parser);
        Self { parser }
    }

    pub(crate) fn analyze(&mut self, script: &str) -> ShellAnalysis {
        self.analyze_script(script, 0)
    }

    fn analyze_script(&mut self, script: &str, depth: usize) -> ShellAnalysis {
        if depth >= MAX_RECURSION_DEPTH {
            return ambiguous_fallback(script);
        }
        let Some(tree) = self
            .parser
            .as_mut()
            .and_then(|parser| parser.parse(script, None))
        else {
            return ambiguous_fallback(script);
        };
        let root = tree.root_node();
        let mut analysis = ShellAnalysis {
            ambiguous: root.has_error(),
            ..ShellAnalysis::default()
        };
        self.walk(root, script, depth, &mut analysis);
        if analysis.operations.is_empty() || root.has_error() {
            analysis.operations.extend(fallback_operation(script));
        }
        analysis
    }

    fn walk(
        &mut self,
        node: tree_sitter::Node<'_>,
        source: &str,
        depth: usize,
        analysis: &mut ShellAnalysis,
    ) {
        if node.kind() == "command" {
            self.analyze_command(node, source, depth, analysis);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, source, depth, analysis);
        }
    }

    fn analyze_command(
        &mut self,
        node: tree_sitter::Node<'_>,
        source: &str,
        depth: usize,
        analysis: &mut ShellAnalysis,
    ) {
        let raw = normalized_text(node, source);
        let Some(name_node) = node.child_by_field_name("name") else {
            analysis.ambiguous = true;
            analysis.operations.extend(fallback_operation(&raw));
            return;
        };
        let Some(name) = literal_token(name_node, source) else {
            analysis.ambiguous = true;
            analysis.operations.extend(fallback_operation(&raw));
            return;
        };
        let mut cursor = node.walk();
        let arguments = node
            .children_by_field_name("argument", &mut cursor)
            .map(|argument| Argument {
                text: normalized_text(argument, source),
                literal: literal_token(argument, source),
            })
            .collect::<Vec<_>>();
        let executable = executable_name(&name);
        let direct = render_command(&executable, &arguments);

        if let Some((inner, consumed, executing)) = peel_wrappers(&executable, &arguments) {
            let uncertain_inner = inner.starts_with('-');
            let inner_aliases = if uncertain_inner {
                possible_inner_aliases(&arguments)
            } else {
                command_aliases(&inner, &arguments[consumed..])
            };
            if executing {
                analysis.operations.push(operation([raw.clone(), direct]));
                analysis.operations.push(operation(inner_aliases));
            } else {
                analysis.operations.push(operation(
                    [raw.clone(), direct].into_iter().chain(inner_aliases),
                ));
            }
            if !uncertain_inner {
                self.inspect_literal_executor(&inner, &arguments[consumed..], depth, analysis);
            } else {
                analysis.ambiguous = true;
            }
            return;
        }

        analysis.operations.push(operation(
            command_aliases(&name, &arguments).into_iter().chain([raw]),
        ));
        self.inspect_literal_executor(&executable, &arguments, depth, analysis);
    }

    fn inspect_literal_executor(
        &mut self,
        executable: &str,
        arguments: &[Argument],
        depth: usize,
        analysis: &mut ShellAnalysis,
    ) {
        let script = if SHELLS.contains(&executable) {
            shell_script(arguments)
        } else if executable == "eval" {
            literal_eval_script(arguments)
        } else {
            None
        };
        let Some(script) = script else {
            if SHELLS.contains(&executable)
                || executable == "eval"
                || matches!(executable, "." | "source")
            {
                analysis.ambiguous = true;
            }
            return;
        };
        let nested = self.analyze_script(&script, depth + 1);
        analysis.operations.extend(nested.operations);
        analysis.ambiguous |= nested.ambiguous;
    }
}

fn ambiguous_fallback(script: &str) -> ShellAnalysis {
    ShellAnalysis {
        operations: fallback_operation(script),
        ambiguous: true,
    }
}

fn shell_script(arguments: &[Argument]) -> Option<String> {
    arguments.windows(2).find_map(|window| {
        let flag = window[0].literal.as_deref()?;
        shell_command_flag(flag)
            .then(|| window[1].literal.clone())
            .flatten()
    })
}

fn literal_eval_script(arguments: &[Argument]) -> Option<String> {
    arguments
        .iter()
        .map(|argument| argument.literal.clone())
        .collect::<Option<Vec<_>>>()
        .map(|arguments| arguments.join(" "))
}

fn peel_wrappers(executable: &str, arguments: &[Argument]) -> Option<(String, usize, bool)> {
    let mut current = executable.to_string();
    let mut consumed = 0;
    let mut executing = false;
    loop {
        if !TRANSPARENT_WRAPPERS.contains(&current.as_str())
            && !EXECUTING_WRAPPERS.contains(&current.as_str())
        {
            break;
        }
        executing |= EXECUTING_WRAPPERS.contains(&current.as_str());
        let remaining = &arguments[consumed..];
        let mut index = 0;
        if current == "env" {
            while let Some(argument) = remaining
                .get(index)
                .and_then(|argument| argument.literal.as_deref())
            {
                if argument == "--" || argument.contains('=') || argument == "-i" {
                    index += 1;
                    continue;
                }
                if matches!(argument, "-u" | "--unset" | "-C" | "--chdir") {
                    index += 2;
                    continue;
                }
                break;
            }
        } else {
            while remaining
                .get(index)
                .and_then(|argument| argument.literal.as_deref())
                == Some("--")
            {
                index += 1;
            }
        }
        current = remaining.get(index)?.literal.clone()?;
        consumed += index + 1;
    }
    (consumed > 0).then_some((current, consumed, executing))
}

fn possible_inner_aliases(arguments: &[Argument]) -> Vec<String> {
    arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            let executable = argument.literal.as_deref()?;
            (!executable.starts_with('-') && !executable.contains('='))
                .then(|| command_aliases(executable, &arguments[index + 1..]))
        })
        .flatten()
        .collect()
}

fn command_aliases(executable: &str, arguments: &[Argument]) -> Vec<String> {
    let basename = executable_name(executable);
    let mut aliases = vec![render_command(executable, arguments)];
    if arguments.iter().all(|argument| argument.literal.is_some()) {
        aliases.push(render_literal_command(executable, arguments));
    }
    if basename != executable {
        aliases.push(render_command(&basename, arguments));
        if arguments.iter().all(|argument| argument.literal.is_some()) {
            aliases.push(render_literal_command(&basename, arguments));
        }
    }
    aliases
}

fn render_literal_command(executable: &str, arguments: &[Argument]) -> String {
    std::iter::once(executable)
        .chain(
            arguments
                .iter()
                .filter_map(|argument| argument.literal.as_deref()),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_command(executable: &str, arguments: &[Argument]) -> String {
    std::iter::once(executable)
        .chain(arguments.iter().map(|argument| argument.text.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn operation(aliases: impl IntoIterator<Item = String>) -> ShellOperation {
    let mut unique = Vec::new();
    for alias in aliases {
        if !alias.is_empty() && !unique.contains(&alias) {
            unique.push(alias);
        }
    }
    ShellOperation { aliases: unique }
}

fn fallback_operation(script: &str) -> Vec<ShellOperation> {
    let normalized = normalize(script);
    (!normalized.is_empty())
        .then(|| operation([normalized]))
        .into_iter()
        .collect()
}

fn normalized_text(node: tree_sitter::Node<'_>, source: &str) -> String {
    node.utf8_text(source.as_bytes())
        .map(normalize)
        .unwrap_or_default()
}

fn normalize(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn literal_token(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_syntax(node) {
        return None;
    }
    let text = node.utf8_text(source.as_bytes()).ok()?;
    let ansi_c_string = text.starts_with("$'") && text.ends_with('\'');
    let literal = if ansi_c_string {
        if text.contains('\\') {
            return None;
        }
        text.strip_prefix('$')?
    } else {
        text
    };
    let tokens = shlex::split(literal)?;
    (tokens.len() == 1).then(|| tokens[0].clone())
}

fn contains_dynamic_syntax(node: tree_sitter::Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "arithmetic_expansion"
            | "command_substitution"
            | "expansion"
            | "process_substitution"
            | "simple_expansion"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(contains_dynamic_syntax)
}

fn executable_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_string()
}

fn shell_command_flag(argument: &str) -> bool {
    argument == "-c"
        || (argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('c'))
}

struct Argument {
    text: String,
    literal: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::ShellAnalyzer;

    fn analyze_shell(script: &str) -> super::ShellAnalysis {
        ShellAnalyzer::new().analyze(script)
    }

    fn contains_alias(script: &str, expected: &str) -> bool {
        analyze_shell(script)
            .operations
            .iter()
            .flat_map(|operation| &operation.aliases)
            .any(|alias| alias == expected || alias.starts_with(expected))
    }

    #[test]
    fn extracts_static_operations_across_shell_forms() {
        for (script, expected) in [
            ("echo ok & git push $(rm file)", "rm file"),
            ("env MODE=test command /usr/bin/git push", "git push"),
            ("bash -c \"eval 'git push'\"", "git push"),
            ("$'rm' file", "rm file"),
        ] {
            assert!(contains_alias(script, expected), "{script}");
        }
        assert!(!contains_alias(
            "printf '%s' 'rm protected'",
            "rm protected"
        ));
    }

    #[test]
    fn marks_dynamic_execution_ambiguous() {
        for script in ["$COMMAND push", "bash -c \"$SCRIPT\"", "source script.sh"] {
            assert!(analyze_shell(script).ambiguous, "{script}");
        }
    }
}
