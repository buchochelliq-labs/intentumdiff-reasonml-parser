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

/// Maximum number of nested `{ ... }` declaration blocks projected as child items.
///
/// Block projection recurses (`project_items` -> `item_node` -> `body_node`), and
/// tree serialisation and drop recurse once per tree level too, so the nesting
/// depth must never be input-controlled. Each projected block adds four JSON
/// nesting levels (item object, its children array, body object, its children
/// array); 24 blocks keeps the emitted tree well under serde_json's default
/// 128-level deserialisation limit that hosts apply. A block nested deeper than
/// this is kept as a single body leaf whose label is the canonical rendering of
/// its whole token run, so its source facts still reach the label and hash.
const MAX_BLOCK_DEPTH: usize = 24;

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
    let tokens = tokenize(source);
    let children = project_items(&tokens, "0", 0);
    let total_lines = source.lines().count().saturating_sub(1) as u32;

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

/// Lexical class of a ReasonML token. Comments and whitespace never become tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    /// Identifiers, keywords, numbers, type variables, string and char literals.
    Word,
    /// Runs of operator characters (`+`, `=>`, `|`, `::`, `++`, ...).
    Op,
    /// `,`, `;`, `.` and any other single punctuation character.
    Punct,
    Open,
    Close,
}

/// One source token. `text` is the exact source spelling (string literals keep
/// their inner bytes verbatim); positions are 0-based rows and UTF-8 byte columns.
#[derive(Clone, Debug)]
struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
}

const OP_CHARS: &str = "!$%&*+-/:<=>?@^|~#";

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '\'' || !ch.is_ascii()
}

/// Lexical scanner that understands ReasonML comments (`/* */`, nested, and `//`),
/// string literals (`"..."` with escapes and `{id|...|id}` quoted strings) and char
/// literals, so comment markers inside literals are content, never comments.
fn tokenize(source: &str) -> Vec<Token<'_>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    let mut line = 0u32;
    let mut line_start = 0usize;

    // Advance over `source[from..to]`, updating the line bookkeeping.
    fn advance(source: &str, from: usize, to: usize, line: &mut u32, line_start: &mut usize) {
        for (offset, byte) in source.as_bytes()[from..to].iter().enumerate() {
            if *byte == b'\n' {
                *line += 1;
                *line_start = from + offset + 1;
            }
        }
    }

    while pos < bytes.len() {
        let rest = &source[pos..];
        let ch = match rest.chars().next() {
            Some(ch) => ch,
            None => break,
        };
        let start_line = line;
        let start_col = (pos - line_start) as u32;

        if ch.is_whitespace() {
            let next = pos + ch.len_utf8();
            advance(source, pos, next, &mut line, &mut line_start);
            pos = next;
            continue;
        }
        if rest.starts_with("/*") {
            let end = block_comment_end(source, pos);
            advance(source, pos, end, &mut line, &mut line_start);
            pos = end;
            continue;
        }
        if rest.starts_with("//") {
            pos += rest.find('\n').unwrap_or(rest.len());
            continue;
        }

        let (kind, end) = if ch == '"' {
            (TokenKind::Word, string_end(source, pos))
        } else if let Some(end) = quoted_string_end(source, pos) {
            (TokenKind::Word, end)
        } else if let Some(end) = char_literal_end(source, pos) {
            (TokenKind::Word, end)
        } else if is_word_char(ch) {
            let starts_numeric = ch.is_ascii_digit();
            let len = rest
                .char_indices()
                .take_while(|(_, c)| is_word_char(*c) || (starts_numeric && *c == '.'))
                .map(|(idx, c)| idx + c.len_utf8())
                .last()
                .unwrap_or(ch.len_utf8());
            (TokenKind::Word, pos + len)
        } else if matches!(ch, '(' | '[' | '{') {
            (TokenKind::Open, pos + 1)
        } else if matches!(ch, ')' | ']' | '}') {
            (TokenKind::Close, pos + 1)
        } else if OP_CHARS.contains(ch) {
            let mut end = pos;
            while end < bytes.len() {
                let tail = &source[end..];
                if end > pos && (tail.starts_with("/*") || tail.starts_with("//")) {
                    break;
                }
                match tail.chars().next() {
                    Some(c) if OP_CHARS.contains(c) => end += c.len_utf8(),
                    _ => break,
                }
            }
            (TokenKind::Op, end)
        } else {
            (TokenKind::Punct, pos + ch.len_utf8())
        };

        advance(source, pos, end, &mut line, &mut line_start);
        let end_col = (end - line_start) as u32;
        tokens.push(Token {
            kind,
            text: &source[pos..end],
            line: start_line,
            col: start_col,
            end_line: line,
            end_col,
        });
        pos = end;
    }
    tokens
}

/// End (exclusive) of a possibly nested `/* ... */` comment; EOF if unterminated.
fn block_comment_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut pos = start;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"/*") {
            depth += 1;
            pos += 2;
        } else if bytes[pos..].starts_with(b"*/") {
            depth -= 1;
            pos += 2;
            if depth == 0 {
                return pos;
            }
        } else {
            pos += 1;
        }
    }
    bytes.len()
}

/// End (exclusive) of a `"..."` string starting at `start`; EOF if unterminated.
fn string_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut pos = start + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\\' => pos += 2,
            b'"' => return pos + 1,
            _ => pos += 1,
        }
    }
    bytes.len()
}

/// `{|...|}` / `{js|...|js}` quoted strings.
fn quoted_string_end(source: &str, start: usize) -> Option<usize> {
    let rest = source[start..].strip_prefix('{')?;
    let id_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
        .count();
    if rest.as_bytes().get(id_len) != Some(&b'|') {
        return None;
    }
    let body_start = start + 1 + id_len + 1;
    let closing = format!("|{}}}", &rest[..id_len]);
    Some(
        source[body_start..]
            .find(&closing)
            .map_or(source.len(), |idx| body_start + idx + closing.len()),
    )
}

/// `'x'`, `'\n'`, `'\''`, `'\065'`; a lone `'a` is a type variable, not a literal.
fn char_literal_end(source: &str, start: usize) -> Option<usize> {
    let rest = source[start..].strip_prefix('\'')?;
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        let escaped = rest[1..].chars().next()?;
        let from = 1 + escaped.len_utf8();
        let close = rest[from..].find('\'')?;
        if close > 3 {
            return None;
        }
        return Some(start + 1 + from + close + 1);
    }
    if first == '\'' {
        return None;
    }
    let after = first.len_utf8();
    (rest[after..].starts_with('\'')).then_some(start + 1 + after + 1)
}

/// Canonical spelling of a token run: insignificant layout is dropped and a single
/// space is placed only where readability or token separation needs it, so
/// `1+2` and `1 + 2` project to the same semantic label.
fn render(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    let mut prev: Option<&Token<'_>> = None;
    for token in tokens {
        if let Some(prev) = prev {
            let space = match (prev.kind, token.kind) {
                (TokenKind::Op, _) | (_, TokenKind::Op) => true,
                (TokenKind::Word | TokenKind::Close, TokenKind::Word) => true,
                (TokenKind::Punct, _) => matches!(prev.text, "," | ";"),
                _ => false,
            };
            if space {
                out.push(' ');
            }
        }
        out.push_str(token.text);
        prev = Some(token);
    }
    out
}

/// Split a token run into `;`-terminated items at bracket depth zero.
fn split_items<'t, 'a>(tokens: &'t [Token<'a>]) -> Vec<&'t [Token<'a>]> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Open => depth += 1,
            TokenKind::Close => depth = depth.saturating_sub(1),
            TokenKind::Punct if depth == 0 && token.text == ";" => {
                if idx > start {
                    items.push(&tokens[start..idx]);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        items.push(&tokens[start..]);
    }
    items
}

/// Project `tokens` as items of a block nested `depth` levels deep (0 = file).
fn project_items(tokens: &[Token<'_>], parent_id: &str, depth: usize) -> Vec<SemanticNode> {
    split_items(tokens)
        .into_iter()
        .enumerate()
        .map(|(index, item)| item_node(&format!("{parent_id}.{index}"), item, depth))
        .collect()
}

fn is_identifier(token: &Token<'_>) -> bool {
    token.kind == TokenKind::Word
        && token
            .text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic() || ch == '_' || ch == '\'')
}

fn is_word(token: Option<&Token<'_>>, text: &str) -> bool {
    token.is_some_and(|t| t.kind == TokenKind::Word && t.text == text)
}

/// Classified declaration: node type, body node type and the index of the name token.
fn classify(item: &[Token<'_>]) -> Option<(&'static str, &'static str, usize)> {
    let first = item.first()?;
    if first.kind != TokenKind::Word {
        return None;
    }
    let (node_type, body_type, name_idx) = match first.text {
        "module" if is_word(item.get(1), "type") => ("module_type", "module_body", 2),
        "module" => {
            let colon = item
                .get(2)
                .is_some_and(|t| t.kind == TokenKind::Op && t.text == ":");
            let node_type = if colon { "module_signature" } else { "module" };
            (node_type, "module_body", 1)
        }
        "open" => ("open", "open_body", 1),
        "include" => ("include", "include_body", 1),
        "type" if is_word(item.get(1), "nonrec") => ("type", "type_body", 2),
        "type" => ("type", "type_body", 1),
        "let" if is_word(item.get(1), "rec") => ("recursive_value", "value_body", 2),
        "let" => {
            let upper = item
                .get(1)
                .and_then(|t| t.text.chars().next())
                .is_some_and(char::is_uppercase);
            let node_type = if upper { "component" } else { "value" };
            (node_type, "value_body", 1)
        }
        "external" => ("external", "external_signature", 1),
        "exception" => ("exception", "exception_body", 1),
        _ => return None,
    };
    is_identifier(item.get(name_idx)?).then_some((node_type, body_type, name_idx))
}

fn item_node(id: &str, item: &[Token<'_>], depth: usize) -> SemanticNode {
    let Some((node_type, body_type, name_idx)) = classify(item) else {
        return span_node(id, "reasonml_statement", &render(item), item, Vec::new());
    };

    // Module paths (`open Belt.Array`) are part of the declared name.
    let mut name_end = name_idx + 1;
    if matches!(node_type, "open" | "include" | "module") {
        while name_end + 1 < item.len()
            && item[name_end].kind == TokenKind::Punct
            && item[name_end].text == "."
            && item[name_end + 1].kind == TokenKind::Word
        {
            name_end += 2;
        }
    }
    let name_tokens = &item[name_idx..name_end];
    let label = render(name_tokens).trim_start_matches('\'').to_string();

    // Everything after the name is semantic payload: parameters, signatures,
    // aliases, constructors and right-hand sides. Only the defining `=` is dropped.
    let mut rest = &item[name_end..];
    if rest
        .first()
        .is_some_and(|t| t.kind == TokenKind::Op && t.text == "=")
    {
        rest = &rest[1..];
    }
    let mut children = Vec::new();
    if !rest.is_empty() {
        let body_id = if body_type == "value_body" {
            format!("{id}.rhs")
        } else {
            format!("{id}.body")
        };
        children.push(body_node(&body_id, body_type, rest, depth));
    }

    let name_span = (
        name_tokens[0].line,
        name_tokens[0].col,
        name_tokens[name_tokens.len() - 1].end_line,
        name_tokens[name_tokens.len() - 1].end_col,
    );
    build_node(id, node_type, &label, name_span, children)
}

/// A body ending in a `{ ... }` block keeps its header as the label and projects the
/// block's items as children, so edits inside modules and functions stay granular.
/// `depth` is the nesting depth of the item owning this body; once it reaches
/// [`MAX_BLOCK_DEPTH`] the block is not descended into and the body stays a leaf.
fn body_node(id: &str, body_type: &str, tokens: &[Token<'_>], depth: usize) -> SemanticNode {
    if depth >= MAX_BLOCK_DEPTH {
        return span_node(id, body_type, &render(tokens), tokens, Vec::new());
    }
    if let Some(open_idx) = trailing_block_start(tokens) {
        let inner = &tokens[open_idx + 1..tokens.len() - 1];
        let header = &tokens[..open_idx];
        let label = if header.is_empty() {
            "{}".to_string()
        } else {
            format!("{} {{}}", render(header))
        };
        let children = project_items(inner, id, depth + 1);
        return span_node(id, body_type, &label, tokens, children);
    }
    span_node(id, body_type, &render(tokens), tokens, Vec::new())
}

fn trailing_block_start(tokens: &[Token<'_>]) -> Option<usize> {
    let last = tokens.last()?;
    if last.kind != TokenKind::Close || last.text != "}" {
        return None;
    }
    let mut depth = 0usize;
    for idx in (0..tokens.len()).rev() {
        match tokens[idx].kind {
            TokenKind::Close => depth += 1,
            TokenKind::Open => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    // `{|`-style records are not blocks; only a real `{` qualifies.
                    return (tokens[idx].text == "{").then_some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn span_node(
    id: &str,
    node_type: &str,
    label: &str,
    tokens: &[Token<'_>],
    children: Vec<SemanticNode>,
) -> SemanticNode {
    let first = &tokens[0];
    let last = &tokens[tokens.len() - 1];
    build_node(
        id,
        node_type,
        label,
        (first.line, first.col, last.end_line, last.end_col),
        children,
    )
}

fn build_node(
    id: &str,
    node_type: &str,
    label: &str,
    span: (u32, u32, u32, u32),
    children: Vec<SemanticNode>,
) -> SemanticNode {
    let hash = stable_hash(node_type, label, &children);
    SemanticNodeBuilder::new(id, node_type, label, span.0, span.1, span.2, span.3, hash)
        .children(children)
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
    #[test]
    fn value_body_uses_actual_source_columns() {
        let before = "let title = \"old\";\\n";
        let after = "let title = \"new\";\\n";

        let before_json: serde_json::Value =
            serde_json::from_str(&parse_reasonml(before)).expect("before JSON");
        let after_json: serde_json::Value =
            serde_json::from_str(&parse_reasonml(after)).expect("after JSON");

        let before_rhs = &before_json["children"][0]["children"][0];
        let after_rhs = &after_json["children"][0]["children"][0];

        assert_eq!(before_rhs["node_type"], "value_body");
        assert_eq!(before_rhs["position"]["start_col"], 12);
        assert_eq!(after_rhs["position"]["start_col"], 12);
        assert!(
            before_rhs["position"]["end_col"].as_u64().unwrap()
                > before_rhs["position"]["start_col"].as_u64().unwrap()
        );
    }

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
        intentumdiff_plugin_sdk::testing::assert_root_node_type(
            &parsed,
            ROOT_NODE_TYPE,
            LANGUAGE_ID,
        );
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

    #[test]
    fn value_labels_use_identifier_byte_columns_after_indentation() {
        // Previously the labelled value span covered "  let apply_dis..."
        // instead of the identifier. Its end column was truncated too.
        let source = "  let applyDiscount = 42;\n  let formatReceipt = 3;\n";
        let parsed: serde_json::Value = serde_json::from_str(&parse_reasonml(source)).unwrap();
        let bindings = parsed["children"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
        for (binding, label) in bindings.iter().zip(["applyDiscount", "formatReceipt"]) {
            assert_eq!(binding["label"], label);
            assert_eq!(binding["position"]["start_col"].as_u64(), Some(6));
            assert_eq!(
                binding["position"]["end_col"].as_u64(),
                Some(6 + label.len() as u64)
            );
        }
    }

    fn tree(source: &str) -> SemanticNode {
        serde_json::from_str(&parse_reasonml(source)).expect("valid tree JSON")
    }

    fn assert_changed(old: &str, new: &str) {
        assert_ne!(
            tree(old).structural_hash,
            tree(new).structural_hash,
            "expected a semantic change between {old:?} and {new:?}"
        );
    }

    fn assert_preserved(old: &str, new: &str) {
        assert_eq!(
            tree(old).structural_hash,
            tree(new).structural_hash,
            "expected preservation between {old:?} and {new:?}"
        );
    }

    // Issue #7: declaration bodies were dropped, so these were certified as style.
    #[test]
    fn type_body_change_is_semantic() {
        assert_changed("type t = int;\n", "type t = string;\n");
        assert_changed("type answer = Yes;\n", "type answer = Yes | No;\n");
        assert_changed(
            "type answer =\n  | Yes;\n",
            "type answer =\n  | Yes\n  | No;\n",
        );
        let root = tree("type answer = Yes | No;\n");
        let decl = &root.children[0];
        assert_eq!(decl.node_type, "type");
        assert_eq!(decl.label, "answer");
        let body = &decl.children[0];
        assert_eq!(body.node_type, "type_body");
        assert_eq!(body.label, "Yes | No");
        assert_eq!(body.position.start_line, 0);
        assert_eq!(body.position.start_col, 14);
        assert_eq!(body.position.end_col, 22);
    }

    #[test]
    fn module_alias_and_signature_changes_are_semantic() {
        assert_changed("module M = A;\n", "module M = B;\n");
        assert_changed("module M: S = A;\n", "module M: T = A;\n");
        assert_changed(
            "external f: int => int = \"f\";\n",
            "external f: int => string = \"f\";\n",
        );
        assert_changed("exception E(int);\n", "exception E(string);\n");
        let root = tree("module M = B;\n");
        assert_eq!(root.children[0].label, "M");
        assert_eq!(root.children[0].children[0].node_type, "module_body");
        assert_eq!(root.children[0].children[0].label, "B");
    }

    #[test]
    fn multiline_module_body_items_are_children() {
        let old = "module M = {\n  let a = 1;\n  let b = 2;\n};\n";
        let new = "module M = {\n  let a = 1;\n  let b = 3;\n};\n";
        assert_changed(old, new);
        let root = tree(new);
        let body = &root.children[0].children[0];
        assert_eq!(body.children.len(), 2);
        assert_eq!(body.children[1].label, "b");
        assert_eq!(body.children[1].position.start_line, 2);
        assert_eq!(body.children[1].position.start_col, 6);
        assert_eq!(body.children[1].children[0].label, "3");
    }

    #[test]
    fn comment_markers_inside_strings_are_content() {
        assert_changed("let x = \"/*safe\";\n", "let x = \"/*danger\";\n");
        assert_changed("let x = \"// a\";\n", "let x = \"// b\";\n");
        assert_changed("let x = {|/* a|};\n", "let x = {|/* b|};\n");
        assert_changed("let c = '/';\nlet x = 1;\n", "let c = '/';\nlet x = 2;\n");
        let root = tree("let x = \"/*safe\";\n");
        assert_eq!(root.children[0].children[0].label, "\"/*safe\"");
    }

    #[test]
    fn real_comments_are_still_ignored() {
        let base = "let answer = () => 1;\nprint_int(answer());\n";
        assert_preserved(
            base,
            "/* baseline comment */\nlet answer = () => 1;\nprint_int(answer());\n",
        );
        assert_preserved(
            base,
            "let answer = () => /* inline */ 1;\nprint_int(answer()); // trailing\n",
        );
        assert_preserved(
            base,
            "/* outer /* nested */ still */\nlet answer = () => 1;\nprint_int(answer());\n",
        );
        assert_preserved(base, base);
    }

    // Issue #8: raw RHS spelling made `1+2` and `1 + 2` differ.
    #[test]
    fn let_rhs_spacing_is_preservation() {
        let old = "let x=1+2;\nprint_int(x);\n";
        let new = "let x = 1 + 2;\nprint_int(x);\n";
        assert_preserved(old, new);
        assert_preserved(old, "let x =\n  1\n  + 2;\nprint_int( x );\n");
        let root = tree(old);
        let rhs = &root.children[0].children[0];
        assert_eq!(rhs.label, "1 + 2");
        // Spans stay exact source spans: `1+2` at bytes 6..9 of row 0.
        assert_eq!(rhs.position.start_col, 6);
        assert_eq!(rhs.position.end_col, 9);
        // Numeric and operator mutations remain meaningful.
        assert_changed(old, "let x = 1 + 3;\nprint_int(x);\n");
        assert_changed(old, "let x = 1 - 2;\nprint_int(x);\n");
        assert_changed("print_int(6 + 2);\n", "print_int(6 - 2);\n");
        // Spacing inside string literals is content.
        assert_changed("let s = \"a b\";\n", "let s = \"a  b\";\n");
    }

    // Issue #10: block projection recursed once per nested block with no limit,
    // so deeply nested modules/values overflowed the stack and aborted the parser.
    fn nested_modules(levels: usize, value: &str) -> String {
        let mut source = String::new();
        for level in 0..levels {
            source.push_str(&format!("module M{level} = {{\n"));
        }
        source.push_str(&format!("let x = {value};\n"));
        for _ in 0..levels {
            source.push_str("};\n");
        }
        source
    }

    fn nested_values(levels: usize, value: &str) -> String {
        let mut source = String::new();
        for level in 0..levels {
            source.push_str(&format!("let v{level} = () => {{ "));
        }
        source.push_str(value);
        for _ in 0..levels {
            source.push_str(" };");
        }
        source.push('\n');
        source
    }

    /// JSON nesting depth, computed iteratively so the check itself is bounded.
    fn json_depth(value: &serde_json::Value) -> usize {
        let mut max = 0;
        let mut stack = vec![(value, 1usize)];
        while let Some((value, depth)) = stack.pop() {
            max = max.max(depth);
            match value {
                serde_json::Value::Object(map) => {
                    stack.extend(map.values().map(|v| (v, depth + 1)))
                }
                serde_json::Value::Array(items) => {
                    stack.extend(items.iter().map(|v| (v, depth + 1)))
                }
                _ => {}
            }
        }
        max
    }

    /// Parse on a 1 MiB stack, as a constrained host would, and check the result is
    /// a tree (not an error) that hosts can read with serde_json's default limit.
    fn parse_on_small_stack(source: String) -> serde_json::Value {
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let parsed = parse_reasonml(&source);
                intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
                let value: serde_json::Value =
                    serde_json::from_str(&parsed).expect("default-limit JSON parse");
                assert!(value.get("error").is_none(), "unexpected error envelope");
                assert!(json_depth(&value) < 128, "tree too deep for hosts");
                value
            })
            .expect("spawn parser thread")
            .join()
            .expect("parser thread must not crash")
    }

    #[test]
    fn nested_blocks_at_the_limit_stay_granular() {
        let source = nested_modules(MAX_BLOCK_DEPTH, "1");
        let root = parse_on_small_stack(source.clone());
        let mut node = &root["children"][0];
        for level in 0..MAX_BLOCK_DEPTH {
            assert_eq!(node["node_type"], "module");
            assert_eq!(node["label"], format!("M{level}"));
            let body = &node["children"][0];
            assert_eq!(body["node_type"], "module_body");
            assert_eq!(body["label"], "{}");
            node = &body["children"][0];
        }
        assert_eq!(node["node_type"], "value");
        assert_eq!(node["label"], "x");
        assert_eq!(node["children"][0]["label"], "1");
        assert_changed(&source, &nested_modules(MAX_BLOCK_DEPTH, "2"));
        assert_preserved(&source, &source.replace("\n", "\n  "));
    }

    #[test]
    fn deeply_nested_blocks_are_bounded_on_a_small_stack() {
        for levels in [MAX_BLOCK_DEPTH + 1, 3_000, 20_000, 100_000] {
            let root = parse_on_small_stack(nested_modules(levels, "1"));
            let mut node = &root["children"][0];
            for _ in 0..MAX_BLOCK_DEPTH {
                node = &node["children"][0]["children"][0];
            }
            assert_eq!(node["label"], format!("M{MAX_BLOCK_DEPTH}"));
            let leaf = &node["children"][0];
            assert_eq!(leaf["node_type"], "module_body");
            assert!(leaf["children"].as_array().is_none_or(|c| c.is_empty()));
            let label = leaf["label"].as_str().unwrap();
            assert!(label.starts_with('{') && label.contains("let x = 1;"));
            // Spans stay exact: the leaf ends on the last `}` of that module.
            assert_eq!(
                leaf["position"]["end_line"].as_u64(),
                Some((2 * levels - MAX_BLOCK_DEPTH) as u64)
            );

            parse_on_small_stack(nested_values(levels, "1"));
        }
        // Facts beyond the projection limit still reach labels and hashes.
        let levels = 100_000;
        let (old, new) = (nested_modules(levels, "1"), nested_modules(levels, "2"));
        let (old_hash, new_hash) = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || (tree(&old).structural_hash, tree(&new).structural_hash))
            .unwrap()
            .join()
            .unwrap();
        assert_ne!(old_hash, new_hash);
    }

    #[test]
    fn unterminated_input_is_bounded() {
        for source in [
            "let answer = (\n",
            "let s = \"open\n",
            "/* open\nlet x = 1;\n",
            "let c = '",
            "}}};;",
        ] {
            intentumdiff_plugin_sdk::testing::assert_valid_json(
                &parse_reasonml(source),
                LANGUAGE_ID,
            );
        }
    }
}
