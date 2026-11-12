use regex::Regex;

use crate::utils::path::expand_home_path;

#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let expanded = expand_home_path(pattern);
        let regex_str = glob_to_regex(&expanded.to_string_lossy());
        let regex = Regex::new(&regex_str).unwrap_or_else(|_| Regex::new("^$").unwrap());
        Pattern { regex }
    }

    pub fn matches(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() * 2);
    let mut chars = pattern.chars().peekable();

    re.push('^');
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' => re.push_str("\\."),
            '\\' => re.push_str("\\\\"),
            '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    if re.ends_with(" [^/]*") {
        re.truncate(re.len() - 6);
        re.push_str("(?: [^/]*)?");
    }
    if re.ends_with(" .*") {
        re.truncate(re.len() - 3);
        re.push_str("(?: .*)?");
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    #[test]
    fn wildcard_does_not_match_across_slashes() {
        let pattern = Pattern::new("src/*");
        assert!(pattern.matches("src/main.rs"));
        assert!(!pattern.matches("src/bin/main.rs"));
    }

    #[test]
    fn globstar_matches_across_slashes() {
        let pattern = Pattern::new("src/**");
        assert!(pattern.matches("src/main.rs"));
        assert!(pattern.matches("src/bin/main.rs"));
    }

    #[test]
    fn globstar_slash_matches_zero_or_more_directories() {
        let pattern = Pattern::new("src/**/main.rs");
        assert!(pattern.matches("src/main.rs"));
        assert!(pattern.matches("src/bin/main.rs"));
        assert!(pattern.matches("src/bin/nested/main.rs"));
    }

    #[test]
    fn trailing_space_wildcard_is_optional() {
        let pattern = Pattern::new("git status *");
        assert!(pattern.matches("git status"));
        assert!(pattern.matches("git status --porcelain"));
    }

    #[test]
    fn question_matches_one_character() {
        let pattern = Pattern::new("file-?.md");
        assert!(pattern.matches("file-a.md"));
        assert!(!pattern.matches("file-ab.md"));
    }
}
