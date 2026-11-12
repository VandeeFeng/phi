use serde_json::Value;

pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";
pub const CODEMODE_SOURCE_GRAMMAR: &str = r#"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE
OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceOptions {
    pub max_output_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSource {
    pub code: String,
    pub options: SourceOptions,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct SourceError(String);

/// Parses metadata only; the caller decides how to apply output and timeout options.
pub fn parse_codemode_source(input: &str) -> Result<ParsedSource, SourceError> {
    let fail = |message: &str| SourceError(message.into());
    if input.trim().is_empty() {
        return Err(fail("expected non-empty JavaScript source"));
    }
    let newline = input.find('\n');
    let first = input[..newline.unwrap_or(input.len())]
        .trim_end_matches('\r')
        .trim_start();
    let Some(directive) = first.strip_prefix(CODEMODE_OPTIONS_PREFIX) else {
        return Ok(ParsedSource {
            code: input.into(),
            options: SourceOptions::default(),
        });
    };
    let code = newline.map(|index| &input[index..]).unwrap_or("");
    if code.trim().is_empty() {
        return Err(fail(
            "the @options line must be followed by JavaScript source",
        ));
    }
    let value: Value = serde_json::from_str(directive.trim())
        .map_err(|error| SourceError(format!("@options must be valid JSON: {error}")))?;
    let fields = value
        .as_object()
        .ok_or_else(|| fail("@options must be a JSON object"))?;
    let mut options = SourceOptions::default();
    for (name, value) in fields {
        let integer = value
            .as_f64()
            .filter(|value| {
                value.is_finite()
                    && *value >= 0.0
                    && value.fract() == 0.0
                    && *value <= MAX_SAFE_INTEGER as f64
            })
            .map(|value| value as u64);
        match name.as_str() {
            "max_output_tokens" => {
                options.max_output_tokens =
                    Some(integer.ok_or_else(|| {
                        fail("max_output_tokens must be a non-negative safe integer")
                    })?)
            }
            "timeout_ms" => {
                options.timeout_ms = Some(
                    integer
                        .filter(|value| *value > 0 && *value <= MAX_TIMEOUT_MS)
                        .ok_or_else(|| {
                            fail("timeout_ms must be a positive integer up to 2147483647")
                        })?,
                )
            }
            _ => return Err(SourceError(format!("unsupported @options field: {name}"))),
        }
    }
    Ok(ParsedSource {
        code: code.into(),
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn options_preserve_lines() {
        let source = parse_codemode_source(
            "  // @options: {\"timeout_ms\":10,\"max_output_tokens\":0}\r\nreturn 1",
        )
        .unwrap();
        assert_eq!(source.code, "\nreturn 1");
        assert_eq!(source.options.timeout_ms, Some(10));
        assert_eq!(source.options.max_output_tokens, Some(0));
        assert_eq!(
            parse_codemode_source(
                "// @options: {\"timeout_ms\":1e3,\"max_output_tokens\":1.0}\nreturn 1"
            )
            .unwrap()
            .options,
            SourceOptions {
                timeout_ms: Some(1000),
                max_output_tokens: Some(1)
            }
        );
        for source in [
            "",
            "// @options: {}",
            "// @options: []\nreturn 1",
            "// @options: {\"x\":1}\nreturn 1",
            "// @options: {\"timeout_ms\":0}\nreturn 1",
            "// @options: {\"max_output_tokens\":1.5}\nreturn 1",
        ] {
            assert!(parse_codemode_source(source).is_err(), "{source}");
        }
    }

    proptest! {
        #[test]
        fn plain_source_is_unchanged(source in "[a-zA-Z][a-zA-Z0-9 ;]{0,128}") {
            prop_assert_eq!(parse_codemode_source(&source).unwrap().code, source);
        }
    }
}
