use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{
    analyze_function, collect_functions, collect_structs, finalize_early_returns, LanguageSpec,
    NodeClass,
};
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct RustAnalyzer;

impl RustAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RustAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for RustAnalyzer {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn extensions(&self) -> &[&'static str] {
        &["rs"]
    }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| anyhow!("set language rust: {}", e))?;
        let tree = parser
            .parse(src, None)
            .ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Rust, path.to_path_buf(), hash_source(src));

        let spec = RustSpec;
        let mut fns = Vec::new();
        collect_functions(&spec, tree.root_node(), src_bytes, &mut fns);
        for n in fns {
            if is_test_function(n, src_bytes) {
                continue;
            }
            if let Some(fa) = analyze_function(&spec, n, src_bytes, path) {
                report.functions.push(fa);
            }
        }
        finalize_early_returns(&mut report.functions);
        collect_structs(
            &spec,
            tree.root_node(),
            src_bytes,
            path,
            &mut report.structs,
        );
        Ok(report)
    }
}

struct RustSpec;

impl LanguageSpec for RustSpec {
    fn classify(&self, node: &Node, _src: &[u8]) -> NodeClass {
        match node.kind() {
            "function_item" | "function_signature_item" => NodeClass::Function,
            "if_expression" | "if_let_expression" => NodeClass::If,
            "else_clause" => NodeClass::Else,
            "while_expression" | "while_let_expression" | "loop_expression" | "for_expression" => {
                NodeClass::Loop
            }
            "match_arm" => NodeClass::SwitchCase,
            "call_expression" | "method_call_expression" | "macro_invocation" => NodeClass::Call,
            "return_expression" => NodeClass::Return,
            "unsafe_block" => NodeClass::UnsafeBlock,
            "line_comment" | "block_comment" => NodeClass::Comment,
            "integer_literal" => NodeClass::IntLit,
            "float_literal" => NodeClass::FloatLit,
            "string_literal"
            | "raw_string_literal"
            | "byte_string_literal"
            | "raw_byte_string_literal" => NodeClass::StrLit,
            "char_literal" => NodeClass::CharLit,
            "boolean_literal" => {
                // determine true/false from the token
                if let Some(ch) = node.child(0) {
                    if ch.kind() == "true" {
                        return NodeClass::BoolLit(true);
                    }
                }
                NodeClass::BoolLit(false)
            }
            "identifier"
            | "type_identifier"
            | "field_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier" => NodeClass::Identifier,
            "binary_expression" => {
                // Detect && / ||
                if let Some(op) = node.child_by_field_name("operator") {
                    match op.kind() {
                        "&&" | "||" => return NodeClass::ShortCircuit,
                        _ => {}
                    }
                }
                NodeClass::Operator
            }
            "unary_expression"
            | "compound_assignment_expr"
            | "assignment_expression"
            | "reference_expression" => NodeClass::Operator,
            "try_expression" => NodeClass::TryPropagate,
            "block" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn call_args<'tree>(&self, node: Node<'tree>, _src: &[u8]) -> Vec<Node<'tree>> {
        // For method calls the receiver is prepended as arg 0 so the call
        // shape lines up with the equivalent free-function call on the other
        // side: `x.exp()` ↔ `exp(x)`, `profile.is_local()` ↔
        // `p7_profile_IsLocal(profile)`. `self.foo()` then lowers to
        // `Param(0)` in arg 0, which is also what the C-style equivalent
        // produces — so forwarding-skew analysis sees them as consistent.
        // `macro_invocation` is left empty: the token tree inside `name!(...)`
        // doesn't have a stable positional shape we can rely on across
        // declarative-macro authors, and most macro args are not parameter
        // forwarding anyway.
        match node.kind() {
            "call_expression" => {
                let mut out = Vec::new();
                // `obj.field_ptr()` parses as call_expression where the
                // function is a field_expression. Treat the value side as
                // the receiver-arg, mirroring the method_call_expression
                // case below.
                if let Some(f) = node.child_by_field_name("function") {
                    if f.kind() == "field_expression" {
                        if let Some(rec) = f.child_by_field_name("value") {
                            out.push(rec);
                        }
                    }
                }
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    out.extend(args.children(&mut cursor).filter(|c| c.is_named()));
                }
                out
            }
            "method_call_expression" => {
                let mut out = Vec::new();
                if let Some(rec) = node.child_by_field_name("receiver") {
                    out.push(rec);
                }
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    out.extend(args.children(&mut cursor).filter(|c| c.is_named()));
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        match node.kind() {
            "call_expression" => {
                let f = node.child_by_field_name("function")?;
                match f.kind() {
                    // `"s".into()` and friends are `call_expression` where the
                    // function is a field_expression. Use just the field name.
                    "field_expression" => f
                        .child_by_field_name("field")
                        .and_then(|n| n.utf8_text(src).ok())
                        .map(|s| s.to_string()),
                    // `foo::bar::baz()` -> `baz`
                    "scoped_identifier" => {
                        let text = f.utf8_text(src).ok()?;
                        Some(
                            strip_generics(text)
                                .rsplit("::")
                                .next()
                                .unwrap_or("")
                                .to_string(),
                        )
                    }
                    _ => {
                        let text = f.utf8_text(src).ok()?;
                        // Collapse whitespace/newlines that appear when the
                        // callee spans multiple lines.
                        let cleaned: String = strip_generics(text)
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        Some(cleaned)
                    }
                }
            }
            "method_call_expression" => {
                let m = node.child_by_field_name("method")?;
                m.utf8_text(src).ok().map(|s| s.to_string())
            }
            "macro_invocation" => {
                let m = node.child_by_field_name("macro")?;
                m.utf8_text(src).ok().map(|s| format!("{}!", s))
            }
            _ => None,
        }
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // parameters
        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.children(&mut cursor) {
                match p.kind() {
                    "parameter" => {
                        let ty = p
                            .child_by_field_name("type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let name = p
                            .child_by_field_name("pattern")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        sig.inputs.push(Param {
                            name,
                            ty: TypeRef::new(ty),
                        });
                    }
                    "self_parameter" => {
                        sig.inputs.push(Param {
                            name: "self".to_string(),
                            ty: TypeRef::new(p.utf8_text(src).unwrap_or("self").to_string()),
                        });
                    }
                    _ => {}
                }
            }
        }
        // return type
        if let Some(rt) = node.child_by_field_name("return_type") {
            if let Ok(t) = rt.utf8_text(src) {
                sig.outputs.push(TypeRef::new(t.trim()));
            }
        }
        sig
    }

    fn enclosing_type(&self, node: Node, src: &[u8]) -> Option<String> {
        // Walk up until we hit an `impl_item`. The `type` field of that node
        // is always the target type, whether the impl is inherent
        // (`impl Cluster { ... }`) or for a trait
        // (`impl Display for Strand { ... }` -> `Strand`). The `trait` field,
        // if present, is the trait being implemented — intentionally ignored
        // so that trait-method fns live under the concrete type, which is
        // what users reach for when thinking "which class does this belong
        // to?".
        let mut cur = node.parent();
        while let Some(p) = cur {
            if p.kind() == "impl_item" {
                return p
                    .child_by_field_name("type")
                    .and_then(|n| n.utf8_text(src).ok())
                    .map(|s| s.trim().to_string());
            }
            cur = p.parent();
        }
        None
    }

    fn original_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // Look for preceding attribute_item siblings on the parent (mod/impl/root).
        // In tree-sitter-rust, attributes are children prior to the function_item
        // in the same parent; find them via node.prev_named_sibling() chain.
        let mut cur = node.prev_named_sibling();
        let mut link_name = None;
        let mut has_no_mangle = false;
        while let Some(n) = cur {
            if n.kind() == "attribute_item" || n.kind() == "inner_attribute_item" {
                if let Ok(text) = n.utf8_text(src) {
                    if text.contains("no_mangle") {
                        has_no_mangle = true;
                    }
                    if let Some(start) = text.find("link_name") {
                        let rest = &text[start..];
                        if let Some(q1) = rest.find('"') {
                            let after = &rest[q1 + 1..];
                            if let Some(q2) = after.find('"') {
                                link_name = Some(after[..q2].to_string());
                            }
                        }
                    }
                }
                cur = n.prev_named_sibling();
            } else {
                break;
            }
        }
        if let Some(ln) = link_name {
            return Some(ln);
        }
        if has_no_mangle {
            // For #[no_mangle], the original (C) symbol is the function's own name.
            return self.function_name(node, src);
        }
        original_name_from_doc_comment(node, src)
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        // modifiers field or preceding tokens
        if let Ok(text) = node.utf8_text(src) {
            let head = text.split('{').next().unwrap_or("");
            for kw in ["pub", "async", "unsafe", "const", "extern"] {
                if head.split_whitespace().any(|t| t == kw) {
                    attrs.insert(kw.to_string(), "true".into());
                }
            }
        }
        // walk preceding attribute_items for misc markers
        let mut cur = node.prev_named_sibling();
        while let Some(n) = cur {
            if n.kind() == "attribute_item" || n.kind() == "inner_attribute_item" {
                if let Ok(text) = n.utf8_text(src) {
                    if text.contains("no_mangle") {
                        attrs.insert("no_mangle".into(), "true".into());
                    }
                    if text.contains("inline") {
                        attrs.insert("inline".into(), "true".into());
                    }
                    if text.contains("cfg") && !text.contains("cfg_attr") {
                        attrs.insert("cfg".into(), text.to_string());
                    }
                }
                cur = n.prev_named_sibling();
            } else {
                break;
            }
        }
        attrs
    }

    fn struct_kind(&self, node: &Node, _src: &[u8]) -> Option<&'static str> {
        match node.kind() {
            "struct_item" => Some("struct"),
            "union_item" => Some("union"),
            _ => None,
        }
    }

    fn struct_name(&self, node: Node, src: &[u8]) -> Option<String> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn struct_fields(&self, node: Node, src: &[u8]) -> Vec<(String, String)> {
        // Struct body can be a `field_declaration_list` (named fields),
        // `ordered_field_declaration_list` (tuple struct), or absent (unit
        // struct). Union bodies always carry `field_declaration_list`.
        let body = node.child_by_field_name("body");
        let Some(body) = body else { return Vec::new() };
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for c in body.children(&mut cursor) {
            match c.kind() {
                "field_declaration" => {
                    let name = c
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("_")
                        .to_string();
                    let ty = c
                        .child_by_field_name("type")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    out.push((name, ty));
                }
                "ordered_field_declaration" => {
                    // Tuple-struct positional field: no name, just a type.
                    let ty = c
                        .child_by_field_name("type")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    out.push((format!("_{}", out.len()), ty));
                }
                _ => {}
            }
        }
        out
    }

    fn struct_attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        if let Ok(text) = node.utf8_text(src) {
            let head = text.split('{').next().unwrap_or("");
            if head.split_whitespace().any(|t| t == "pub") {
                attrs.insert("pub".into(), "true".into());
            }
        }
        let mut cur = node.prev_named_sibling();
        while let Some(n) = cur {
            if n.kind() == "attribute_item" || n.kind() == "inner_attribute_item" {
                if let Ok(text) = n.utf8_text(src) {
                    if text.contains("repr") {
                        attrs.insert("repr".into(), text.trim().to_string());
                    }
                    if text.contains("derive") {
                        attrs.insert("derive".into(), text.trim().to_string());
                    }
                }
                cur = n.prev_named_sibling();
            } else {
                break;
            }
        }
        attrs
    }
}

fn strip_generics(s: &str) -> &str {
    if let Some(idx) = s.find("::<") {
        &s[..idx]
    } else if let Some(idx) = s.find('<') {
        &s[..idx]
    } else {
        s
    }
}

fn is_test_function(node: Node, src: &[u8]) -> bool {
    let mut cur = node.prev_named_sibling();
    while let Some(n) = cur {
        match n.kind() {
            "attribute_item" | "inner_attribute_item" => {
                if n.utf8_text(src).is_ok_and(is_test_attribute) {
                    return true;
                }
                cur = n.prev_named_sibling();
            }
            _ => break,
        }
    }
    false
}

fn is_test_attribute(text: &str) -> bool {
    let body = text
        .trim()
        .trim_start_matches("#!")
        .trim_start_matches('#')
        .trim();
    let body = body
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(body)
        .trim();
    let path = body.split(['(', ' ', '=']).next().unwrap_or("").trim();
    path == "test" || path.ends_with("::test")
}

fn original_name_from_doc_comment(node: Node, src: &[u8]) -> Option<String> {
    let src = std::str::from_utf8(src).ok()?;
    let lines = src.lines().collect::<Vec<_>>();
    let mut row = node.start_position().row;
    let mut doc = Vec::new();
    while row > 0 {
        row -= 1;
        let line = lines.get(row)?.trim();
        if line.starts_with("#[") || line.starts_with("#![") {
            continue;
        }
        if let Some(text) = line
            .strip_prefix("///")
            .or_else(|| line.strip_prefix("//!"))
        {
            doc.push(text.trim());
            continue;
        }
        if line.starts_with("/**") || line.starts_with("/*!") || line.starts_with('*') {
            doc.push(
                line.trim_start_matches("/**")
                    .trim_start_matches("/*!")
                    .trim_start_matches('*')
                    .trim_end_matches("*/")
                    .trim(),
            );
            continue;
        }
        break;
    }
    for line in doc.into_iter().rev() {
        if let Some(name) = cpp_name_from_doc_line(line) {
            return Some(name);
        }
    }
    None
}

fn cpp_name_from_doc_line(line: &str) -> Option<String> {
    let marker = "Matches C++";
    let start = line.find(marker)? + marker.len();
    let rest = line[start..].trim_start();
    let candidate = if let Some(rest) = rest.strip_prefix('`') {
        rest.split('`').next().unwrap_or("")
    } else {
        rest.split_whitespace().next().unwrap_or("")
    };
    let name = candidate
        .split('(')
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c == '`' || c == '.' || c == ',' || c == ';' || c == ':');
    if name.is_empty() || !name.chars().any(|c| c.is_ascii_alphabetic() || c == '_') {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::LanguageAnalyzer;
    use std::path::Path;

    #[test]
    fn rust_try_expression_counts_as_conditional_early_exit() {
        let src = r#"
fn helper() -> Option<i32> { Some(1) }

fn qmark() -> Option<i32> {
    let x = helper()?;
    Some(x + 1)
}
"#;
        let report = RustAnalyzer::new()
            .analyze_source(src, Path::new("qmark.rs"))
            .unwrap();
        let qmark = report.functions.iter().find(|f| f.name == "qmark").unwrap();
        assert_eq!(qmark.metrics.branches, 1);
        assert_eq!(qmark.metrics.cyclomatic, 2);
        assert_eq!(qmark.metrics.cognitive, 1);
        assert_eq!(qmark.metrics.early_returns, 1);
        assert_eq!(qmark.metrics.calls_total, 2);
    }

    #[test]
    fn rust_test_functions_are_not_reported() {
        let src = r#"
fn real() {}

#[test]
fn unit_test() {}

#[tokio::test]
async fn async_test() {}

#[cfg(test)]
fn cfg_only_helper() {}
"#;
        let report = RustAnalyzer::new()
            .analyze_source(src, Path::new("tests.rs"))
            .unwrap();
        let names = report
            .functions
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["real", "cfg_only_helper"]);
    }

    #[test]
    fn rust_doc_comment_matches_cpp_sets_original_name() {
        let src = r#"
impl SamFormat {
    /// Matches C++ `SamFormat::print_match(const HspContext&)`.
    #[inline]
    pub fn print_match(&self) {}
}

/// Matches C++ free_function(int).
pub fn free_function_rs() {}
"#;
        let report = RustAnalyzer::new()
            .analyze_source(src, Path::new("doc.rs"))
            .unwrap();
        let method = report
            .functions
            .iter()
            .find(|f| f.name == "print_match")
            .unwrap();
        assert_eq!(
            method.original_name.as_deref(),
            Some("SamFormat::print_match")
        );
        let free = report
            .functions
            .iter()
            .find(|f| f.name == "free_function_rs")
            .unwrap();
        assert_eq!(free.original_name.as_deref(), Some("free_function"));
    }
}
