use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct CommandPermissionRequest<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub origin: Option<&'a str>,
}

impl<'a> CommandPermissionRequest<'a> {
    pub fn new(program: &'a str, args: &'a [String]) -> Self {
        Self {
            program,
            args,
            origin: None,
        }
    }

    pub fn with_origin(mut self, origin: &'a str) -> Self {
        self.origin = Some(origin);
        self
    }

    pub(crate) fn permission_input(&self) -> String {
        self.permission_inputs().remove(0)
    }

    pub(crate) fn permission_inputs(&self) -> Vec<String> {
        if let Some(script) = self.shell_script() {
            return vec![script.to_string()];
        }
        let mut inputs = vec![render_argv(self.program, self.args)];
        if let Some(basename) = executable_name(self.program).filter(|name| *name != self.program) {
            inputs.push(render_argv(basename, self.args));
        }
        inputs
    }

    pub(crate) fn shell_script(&self) -> Option<&str> {
        if !matches!(
            executable_name(self.program),
            Some("bash" | "sh" | "zsh" | "dash" | "ksh")
        ) {
            return None;
        }
        self.args
            .windows(2)
            .find(|args| shell_command_flag(&args[0]))
            .map(|args| args[1].as_str())
    }
}

fn executable_name(program: &str) -> Option<&str> {
    Path::new(program).file_name()?.to_str()
}

fn render_argv(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote_argument)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_+-./:=,@%".contains(ch))
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn shell_command_flag(argument: &str) -> bool {
    argument == "-c"
        || (argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('c'))
}

#[cfg(test)]
mod tests {
    use super::CommandPermissionRequest;

    #[test]
    fn normalizes_direct_and_shell_requests() {
        let args = vec!["rewrite".to_string(), "echo ok; rm file".to_string()];
        let direct = CommandPermissionRequest::new("rtk", &args);
        assert_eq!(direct.permission_input(), "rtk rewrite 'echo ok; rm file'");
        assert!(direct.shell_script().is_none());

        let args = vec!["push".to_string(), "main".to_string()];
        let absolute = CommandPermissionRequest::new("/usr/bin/git", &args);
        assert_eq!(
            absolute.permission_inputs(),
            vec!["/usr/bin/git push main", "git push main"]
        );

        let args = vec!["-lc".to_string(), "rm file".to_string()];
        let shell = CommandPermissionRequest::new("bash", &args);
        assert_eq!(shell.permission_input(), "rm file");
        assert_eq!(shell.shell_script(), Some("rm file"));
    }
}
