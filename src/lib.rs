use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "reasonml";
const ROOT_NODE_TYPE: &str = "reasonml_file";
const DETECT_EXTENSIONS: &[&str] = &[".re", ".rei"];
const DEFAULT_OLD: &str = "let title = \"old\";\n";
const DEFAULT_NEW: &str = "let title = \"new\";\n";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

struct ReasonmlParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let lower = filename.to_lowercase();
    if DETECT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

fn parse_reasonml(source: &str) -> String {
    let mut children = Vec::new();
    let mut total_lines = 0u32;

    for (index, raw) in source.lines().enumerate() {
        let line_no = index as u32;
        total_lines = line_no;
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let id = format!("0.{}", children.len());
        children.push(line_node(&id, line.trim_end_matches(';').trim(), line_no));
    }

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        0,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &children),
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

fn strip_line_comment(line: &str) -> &str {
    line.split_once("/*").map_or(line, |(before, _)| before)
}

fn line_node(id: &str, line: &str, line_no: u32) -> SemanticNode {
    for parser in [
        parse_module,
        parse_open,
        parse_include,
        parse_type,
        parse_let,
        parse_external,
        parse_exception,
    ] {
        if let Some((node_type, label, children)) = parser(line) {
            let mut children = children;
            // #46: a let-binding's RHS is review content — carry it as a child so value
            // edits (numeric bumps, string edits) surface instead of hashing name-only
            // (the ocaml sibling got the same fix).
            if matches!(node_type, "value" | "recursive_value") && children.is_empty() {
                if let Some((_, rhs)) = line.split_once('=') {
                    let rhs = rhs.trim().trim_end_matches(';').trim();
                    if !rhs.is_empty() {
                        children.push(node(
                            &format!("{id}.rhs"),
                            "value_body",
                            rhs,
                            line_no,
                            0,
                            &[],
                        ));
                    }
                }
            }
            return node(id, node_type, &label, line_no, 0, &children);
        }
    }
    node(id, "reasonml_statement", line, line_no, 0, &[])
}

type ParsedLine = Option<(&'static str, String, Vec<SemanticNode>)>;

fn parse_module(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("module ")?;
    let name = take_identifier(rest)?;
    let node_type = if rest[name.len()..].trim_start().starts_with(':') {
        "module_signature"
    } else {
        "module"
    };
    Some((node_type, name.to_string(), Vec::new()))
}

fn parse_open(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("open ")?;
    let name = take_identifier(rest)?;
    Some(("open", name.to_string(), Vec::new()))
}

fn parse_include(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("include ")?;
    let name = take_identifier(rest)?;
    Some(("include", name.to_string(), Vec::new()))
}

fn parse_type(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("type ")?;
    let name = take_identifier(rest.trim_start_matches("nonrec ").trim_start())?;
    Some((
        "type",
        name.trim_start_matches('\'').to_string(),
        Vec::new(),
    ))
}

fn parse_let(line: &str) -> ParsedLine {
    let rest = line
        .strip_prefix("let rec ")
        .or_else(|| line.strip_prefix("let "))?;
    let name = take_identifier(rest)?;
    let node_type = if line.starts_with("let rec ") {
        "recursive_value"
    } else if name.chars().next().is_some_and(|ch| ch.is_uppercase()) {
        "component"
    } else {
        "value"
    };
    Some((node_type, name.to_string(), Vec::new()))
}

fn parse_external(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("external ")?;
    let name = take_identifier(rest)?;
    Some(("external", name.to_string(), Vec::new()))
}

fn parse_exception(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("exception ")?;
    let name = take_identifier(rest)?;
    Some(("exception", name.to_string(), Vec::new()))
}

fn take_identifier(input: &str) -> Option<&str> {
    let trimmed = input.trim_start();
    let end = trimmed
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '.'))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .last()?;
    Some(&trimmed[..end])
}

fn node(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    col: u32,
    children: &[SemanticNode],
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line,
        col,
        line,
        col + label.len() as u32,
        stable_hash(node_type, label, children),
    )
    .children(children.to_vec())
    .build()
}

fn stable_hash(node_type: &str, label: &str, children: &[SemanticNode]) -> String {
    let mut value = format!("{node_type}:{label}");
    for child in children {
        value.push('|');
        value.push_str(&child.structural_hash);
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

impl Guest for ReasonmlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        parse_reasonml(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(ReasonmlParser);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_by_type(node: &SemanticNode, node_type: &str, labels: &mut Vec<String>) {
        if node.node_type == node_type {
            labels.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, labels);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(ReasonmlParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn grammar_id_is_language_id() {
        assert_eq!(ReasonmlParser::grammar_id(), LANGUAGE_ID);
        assert_eq!(
            ReasonmlParser::language_ids(),
            vec![LANGUAGE_ID.to_string()]
        );
    }

    #[test]
    fn detects_reasonml_extensions() {
        assert_eq!(
            detect_language_impl("component.re", DEFAULT_NEW),
            LANGUAGE_ID
        );
        assert_eq!(
            detect_language_impl("component.rei", DEFAULT_NEW),
            LANGUAGE_ID
        );
    }

    #[test]
    fn process_returns_valid_json() {
        let parsed = parse_reasonml(DEFAULT_NEW);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
    }

    #[test]
    fn process_extracts_modules_types_values_components_and_externals() {
        let parsed = parse_reasonml(
            r#"
open React;
include Shared;
module Store: STORE = {};
type account = {id: string, active: bool};
let rec loadAccount = id => id;
let UserCard = props => <div />;
external digest: string => string = "digest";
exception MissingAccount;
"#,
        );
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut opens = Vec::new();
        let mut includes = Vec::new();
        let mut modules = Vec::new();
        let mut types = Vec::new();
        let mut values = Vec::new();
        let mut components = Vec::new();
        let mut externals = Vec::new();
        let mut exceptions = Vec::new();
        labels_by_type(&root, "open", &mut opens);
        labels_by_type(&root, "include", &mut includes);
        labels_by_type(&root, "module_signature", &mut modules);
        labels_by_type(&root, "type", &mut types);
        labels_by_type(&root, "recursive_value", &mut values);
        labels_by_type(&root, "component", &mut components);
        labels_by_type(&root, "external", &mut externals);
        labels_by_type(&root, "exception", &mut exceptions);

        assert!(opens.contains(&"React".to_string()));
        assert!(includes.contains(&"Shared".to_string()));
        assert!(modules.contains(&"Store".to_string()));
        assert!(types.contains(&"account".to_string()));
        assert!(values.contains(&"loadAccount".to_string()));
        assert!(components.contains(&"UserCard".to_string()));
        assert!(externals.contains(&"digest".to_string()));
        assert!(exceptions.contains(&"MissingAccount".to_string()));
    }
}
