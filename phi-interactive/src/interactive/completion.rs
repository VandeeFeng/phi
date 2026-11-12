use crate::commands::{CommandArgument, SlashCommand, SlashCommandSource};

pub(super) fn matching_commands<'a>(
    commands: &'a [SlashCommand],
    prefix: &str,
) -> Vec<&'a SlashCommand> {
    if prefix.is_empty() {
        return commands.iter().collect();
    }
    let mut matches = commands
        .iter()
        .filter_map(|command| slash_match_key(command, prefix).map(|key| (key, command)))
        .collect::<Vec<_>>();
    matches.sort_by_key(|(key, _)| *key);
    matches.into_iter().map(|(_, command)| command).collect()
}

fn slash_match_key<'a>(
    command: &'a SlashCommand,
    prefix: &str,
) -> Option<(u8, u8, usize, usize, &'a str)> {
    let source = match command.source {
        SlashCommandSource::Builtin => 0,
        SlashCommandSource::User => 1,
        SlashCommandSource::Extension => 2,
    };
    let name = command.name.as_str();
    if name == prefix {
        return Some((source, 0, 0, name.len(), name));
    }
    if name.starts_with(prefix) {
        return Some((source, 1, 0, name.len(), name));
    }
    if name.contains(prefix) {
        return Some((
            source,
            2,
            name.find(prefix).unwrap_or(usize::MAX),
            name.len(),
            name,
        ));
    }
    fuzzy_match(name, prefix).map(|gaps| (source, 3, gaps, name.len(), name))
}

pub(super) fn matching_arguments<'a>(
    arguments: &'a [CommandArgument],
    query: &str,
) -> Vec<&'a CommandArgument> {
    if query.is_empty() {
        return arguments.iter().collect();
    }
    let query = query.to_lowercase();
    let mut matches = arguments
        .iter()
        .filter_map(|argument| {
            let key = std::iter::once(argument.value.as_str())
                .chain(argument.alias.as_deref())
                .filter_map(|value| {
                    let name = value.to_lowercase();
                    let rank = if name == query {
                        0
                    } else if name.starts_with(&query) {
                        1
                    } else {
                        2
                    };
                    fuzzy_match(&name, &query).map(|gaps| (rank, gaps, value.len()))
                })
                .min()?;
            Some((key, argument))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(key, _)| *key);
    matches.into_iter().map(|(_, argument)| argument).collect()
}

pub(super) fn completion_match_key(value: &str, query: &str) -> Option<(usize, usize)> {
    if query.is_empty() {
        return Some((0, value.len()));
    }
    fuzzy_match(&value.to_lowercase(), &query.to_lowercase()).map(|gaps| (gaps, value.len()))
}

pub(super) fn fuzzy_match(name: &str, prefix: &str) -> Option<usize> {
    let mut gaps = 0;
    let mut last_match = None;
    let mut chars = name.char_indices();
    for needle in prefix.chars() {
        let (index, _) = chars.find(|(_, haystack)| *haystack == needle)?;
        if let Some(last) = last_match {
            gaps += index - last - 1;
        }
        last_match = Some(index);
    }
    Some(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_ranking_uses_the_strongest_name_or_alias_match() {
        let arguments = [
            CommandArgument {
                value: "development".into(),
                alias: None,
            },
            CommandArgument {
                value: "delivery-env".into(),
                alias: Some("dev".into()),
            },
        ];
        let matches = matching_arguments(&arguments, "dev");
        assert_eq!(matches[0].value, "delivery-env");
    }

    #[test]
    fn matching_commands_supports_fuzzy_subsequence() {
        let commands = vec![
            SlashCommand::builtin("model", "choose model"),
            SlashCommand {
                name: "skill:memory-md".into(),
                description: "memory management".into(),
                source: SlashCommandSource::User,
                arguments: Some(Default::default()),
            },
        ];
        let matches = matching_commands(&commands, "skilm");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "skill:memory-md");
    }
}
