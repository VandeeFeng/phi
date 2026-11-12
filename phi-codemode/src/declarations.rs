use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use crate::types::{CodemodeTool, ToolDefinition};

pub const DEFAULT_SCHEMA_MAX_CHARS: usize = 16_000;
pub const MCP_TYPESCRIPT_PREAMBLE: &str = include_str!("mcp-types.d.ts");
const MAX_REF_EXPANSIONS: usize = 32;
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_SCHEMA_DEPTH: usize = 64;

pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(character) if character.is_ascii_alphabetic() || matches!(character, '_' | '$'))
        && chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'))
}

pub fn to_codemode_identifier(name: &str) -> String {
    let mut result: String = name
        .chars()
        .enumerate()
        .map(|(index, character)| {
            if character.is_ascii_alphabetic()
                || matches!(character, '_' | '$')
                || (index > 0 && character.is_ascii_digit())
            {
                character
            } else {
                '_'
            }
        })
        .collect();
    if result.is_empty() {
        result.push('_');
    }
    result
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.into()
    } else {
        json!(name).to_string()
    }
}

fn comment(description: &str, indent: &str) -> String {
    let description = description.trim().replace("*/", "*\\/");
    if description.is_empty() {
        return String::new();
    }
    if !description.contains('\n') {
        return format!("{indent}/** {description} */\n");
    }
    let lines = description
        .lines()
        .map(|line| format!("{indent} * {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}/**\n{lines}\n{indent} */\n")
}

fn render_schema(schema: Option<&Value>) -> String {
    schema
        .map(|schema| schema_to_type(schema, DEFAULT_SCHEMA_MAX_CHARS))
        .unwrap_or_else(|| "unknown".into())
}

pub fn render_tool_signature(tool: &ToolDefinition) -> String {
    let input = render_schema(tool.input_schema.as_ref());
    format!(
        "{}(args: {input}): Promise<{}>;",
        to_codemode_identifier(&tool.name),
        render_tool_output_type(tool.output_schema.as_ref())
    )
}

pub fn render_tool_output_type(schema: Option<&Value>) -> String {
    let properties = &schema.unwrap_or(&Value::Null)["properties"];
    if properties["content"]["type"] == "array"
        && properties["content"]["items"]["type"] == "object"
        && properties["isError"]["type"] == "boolean"
        && properties["_meta"]["type"] == "object"
    {
        let structured = render_schema(properties.get("structuredContent"));
        return if structured == "unknown" {
            "CallToolResult".into()
        } else {
            format!("CallToolResult<{structured}>")
        };
    }
    render_schema(schema)
}

pub fn render_declarations(tools: &[CodemodeTool], globals: &[CodemodeTool]) -> String {
    let mut sections = Vec::new();
    if !tools.is_empty() {
        let mut identifiers = HashSet::new();
        let members = tools
            .iter()
            .filter(|tool| identifiers.insert(to_codemode_identifier(&tool.definition().name)))
            .map(|tool| {
                let definition = tool.definition();
                format!(
                    "{}  {}",
                    comment(&definition.description, "  "),
                    render_tool_signature(definition)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("declare const tools: {{\n{members}\n}};"));
    }
    let mut namespaces: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for global in globals {
        let definition = global.definition();
        let signature = global.signature.clone().unwrap_or_else(|| {
            let input = render_schema(definition.input_schema.as_ref());
            let output = render_schema(definition.output_schema.as_ref());
            format!("(args: {input}): Promise<{output}>")
        });
        if let Some((namespace, member)) = definition.name.split_once('.') {
            namespaces.entry(namespace).or_default().push(format!(
                "{}  {member}{signature};",
                comment(&definition.description, "  ")
            ));
        } else {
            sections.push(format!(
                "{}declare function {}{signature};",
                comment(&definition.description, ""),
                definition.name
            ));
        }
    }
    for (name, members) in namespaces {
        sections.push(format!(
            "declare const {name}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// Local references expand with bounded recursion; unsupported schemas become unknown.
pub fn schema_to_type(schema: &Value, max_chars: usize) -> String {
    let mut context = SchemaContext {
        root: schema,
        resolving: HashSet::new(),
        expansions: 0,
        nodes: 0,
    };
    let rendered = context.render(schema, 0);
    if rendered.encode_utf16().count() > max_chars {
        "unknown".into()
    } else {
        rendered
    }
}

struct SchemaContext<'a> {
    root: &'a Value,
    resolving: HashSet<String>,
    expansions: usize,
    nodes: usize,
}

fn resolve_reference<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    if reference == "#" {
        return Some(root);
    }
    let fragment = reference.strip_prefix("#/")?;
    let mut bytes = fragment.bytes();
    let mut decoded = vec![b'/'];
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    root.pointer(std::str::from_utf8(&decoded).ok()?)
}

fn union(parts: Vec<String>) -> String {
    if parts.iter().any(|part| part == "unknown") {
        return "unknown".into();
    }
    let mut unique = Vec::new();
    for part in parts {
        if !unique.contains(&part) {
            unique.push(part);
        }
    }
    if unique.is_empty() {
        "never".into()
    } else {
        unique.join(" | ")
    }
}

impl SchemaContext<'_> {
    fn render(&mut self, schema: &Value, depth: usize) -> String {
        self.nodes += 1;
        if self.nodes > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH {
            return "unknown".into();
        }
        match schema {
            Value::Bool(true) => return "unknown".into(),
            Value::Bool(false) => return "never".into(),
            Value::Object(_) => (),
            _ => return "unknown".into(),
        }
        if let Some(reference) = schema["$ref"].as_str() {
            if self.expansions >= MAX_REF_EXPANSIONS || !self.resolving.insert(reference.into()) {
                return "unknown".into();
            }
            self.expansions += 1;
            let target = resolve_reference(self.root, reference);
            let result = target
                .map(|target| self.render(target, depth + 1))
                .unwrap_or_else(|| "unknown".into());
            self.resolving.remove(reference);
            return result;
        }
        if let Some(value) = schema.get("const") {
            return value.to_string();
        }
        if let Some(values) = schema["enum"].as_array() {
            return union(values.iter().map(Value::to_string).collect());
        }
        if let Some(variants) = schema["anyOf"]
            .as_array()
            .or_else(|| schema["oneOf"].as_array())
        {
            return union(
                variants
                    .iter()
                    .map(|variant| self.render(variant, depth + 1))
                    .collect(),
            );
        }
        if let Some(parts) = schema["allOf"].as_array() {
            let parts: Vec<_> = parts
                .iter()
                .map(|part| self.render(part, depth + 1))
                .filter(|part| part != "unknown")
                .map(|part| {
                    if part.contains(" | ") {
                        format!("({part})")
                    } else {
                        part
                    }
                })
                .collect();
            return if parts.is_empty() {
                "unknown".into()
            } else {
                parts.join(" & ")
            };
        }
        if let Some(types) = schema["type"].as_array() {
            return union(
                types
                    .iter()
                    .map(|kind| {
                        let mut schema = schema.clone();
                        schema["type"] = kind.clone();
                        self.render(&schema, depth + 1)
                    })
                    .collect(),
            );
        }
        match schema["type"].as_str() {
            Some("string") => "string".into(),
            Some("integer" | "number") => "number".into(),
            Some("boolean") => "boolean".into(),
            Some("null") => "null".into(),
            Some("array") => self.array(schema, depth),
            Some("object") => self.object(schema, depth),
            None if schema.get("properties").is_some()
                || schema.get("additionalProperties").is_some()
                || schema.get("required").is_some() =>
            {
                self.object(schema, depth)
            }
            None if schema.get("items").is_some() || schema.get("prefixItems").is_some() => {
                self.array(schema, depth)
            }
            _ => "unknown".into(),
        }
    }

    fn array(&mut self, schema: &Value, depth: usize) -> String {
        if let Some(items) = schema.get("items").filter(|items| !items.is_array()) {
            return format!("Array<{}>", self.render(items, depth + 1));
        }
        let tuple = schema["prefixItems"]
            .as_array()
            .or_else(|| schema["items"].as_array());
        match tuple.filter(|tuple| !tuple.is_empty()) {
            Some(tuple) => format!(
                "[{}]",
                tuple
                    .iter()
                    .map(|item| self.render(item, depth + 1))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "unknown[]".into(),
        }
    }

    fn object(&mut self, schema: &Value, depth: usize) -> String {
        let properties = schema["properties"].as_object();
        let mut names: Vec<_> = properties
            .into_iter()
            .flat_map(|properties| properties.keys())
            .collect();
        names.sort();
        let required = schema["required"].as_array();
        let mut members = Vec::new();
        let mut descriptions = Vec::new();
        for name in names {
            let optional =
                if required.is_some_and(|values| values.iter().any(|value| value == name)) {
                    ""
                } else {
                    "?"
                };
            let property = &schema["properties"][name];
            members.push(format!(
                "{}{optional}: {};",
                property_key(name),
                self.render(property, depth + 1)
            ));
            descriptions.push(property["description"].as_str().unwrap_or("").trim());
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => (),
            Some(additional) => members.push(format!(
                "[key: string]: {};",
                self.render(additional, depth + 1)
            )),
            None if members.is_empty() => members.push("[key: string]: unknown;".into()),
            None => (),
        }
        if members.is_empty() {
            return "{}".into();
        }
        if descriptions
            .iter()
            .all(|description| description.is_empty())
        {
            return format!("{{ {} }}", members.join(" "));
        }
        let mut lines = vec!["{".to_owned()];
        for (index, member) in members.iter().enumerate() {
            if let Some(description) = descriptions.get(index) {
                for line in description
                    .split(['\r', '\n', '\u{2028}', '\u{2029}'])
                    .filter(|line| !line.trim().is_empty())
                {
                    lines.push(format!("  // {}", line.trim()));
                }
            }
            lines.push(format!("  {}", member.replace('\n', "\n  ")));
        }
        lines.push("}".into());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn schema_variants_and_refs() {
        assert_eq!(to_codemode_identifier("3d-render"), "_d_render");
        let schema = json!({"type":"object","required":["path"],"properties":{"path":{"$ref":"#/$defs/Path"},"limit":{"type":"integer"}},"$defs":{"Path":{"type":"string"}},"additionalProperties":false});
        assert_eq!(
            schema_to_type(&schema, DEFAULT_SCHEMA_MAX_CHARS),
            "{ limit?: number; path: string; }"
        );
        assert_eq!(
            schema_to_type(&json!({"$ref":"#"}), DEFAULT_SCHEMA_MAX_CHARS),
            "unknown"
        );
        assert_eq!(
            schema_to_type(&json!({"type":["string","null"]}), DEFAULT_SCHEMA_MAX_CHARS),
            "string | null"
        );
        assert_eq!(schema_to_type(&schema, 1), "unknown");
        assert_eq!(
            schema_to_type(
                &json!({"$ref":"#/%24defs/a~1b", "$defs":{"a/b":{"type":"string"}}}),
                DEFAULT_SCHEMA_MAX_CHARS
            ),
            "string"
        );
    }

    #[test]
    fn declaration_contract() {
        let tool = CodemodeTool::new(
            ToolDefinition {
                name: "my-tool".into(),
                description: "bad */ text".into(),
                ..Default::default()
            },
            |_, _| async { Ok(None) },
        );
        let global = CodemodeTool::new(
            ToolDefinition {
                name: "models.list".into(),
                input_schema: Some(json!({"type": "string"})),
                output_schema: Some(json!({"type": "array", "items": {"type": "string"}})),
                ..Default::default()
            },
            |_, _| async { Ok(None) },
        );
        let custom = CodemodeTool::new(
            ToolDefinition {
                name: "searchTools".into(),
                ..Default::default()
            },
            |_, _| async { Ok(None) },
        )
        .with_signature("(query: string): Promise<string[]>");
        let declaration = render_declarations(&[tool], &[global, custom]);
        assert_eq!(
            declaration,
            concat!(
                "declare const tools: {\n  /** bad *\\/ text */\n  my_tool(args: unknown): Promise<unknown>;\n};\n\n",
                "declare function searchTools(query: string): Promise<string[]>;\n\n",
                "declare const models: {\n  list(args: string): Promise<Array<string>>;\n};"
            )
        );
        let mut schema = json!({"properties": {
            "content": {"type": "array", "items": {"type": "object"}},
            "isError": {"type": "boolean"},
            "_meta": {"type": "object"}
        }});
        assert_eq!(render_tool_output_type(Some(&schema)), "CallToolResult");
        schema["properties"]["structuredContent"] = json!({"type": "string"});
        assert_eq!(
            render_tool_output_type(Some(&schema)),
            "CallToolResult<string>"
        );
    }

    proptest! {
        #[test]
        fn normalized_names_are_valid(name in ".{0,100}") {
            prop_assert!(is_identifier(&to_codemode_identifier(&name)));
        }
    }
}
