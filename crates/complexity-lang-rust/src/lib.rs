use anyhow::{anyhow, Result};
use complexity_analyzer::walker::{analyze_function, collect_functions, finalize_early_returns, LanguageSpec, NodeClass};
use complexity_analyzer::LanguageAnalyzer;
use complexity_core::{hash_source, Language, Param, Report, Signature, TypeRef};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct RustAnalyzer;

impl RustAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for RustAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for RustAnalyzer {
    fn language(&self) -> Language { Language::Rust }

    fn extensions(&self) -> &[&'static str] { &["rs"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::language())
            .map_err(|e| anyhow!("set language rust: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Rust, path.to_path_buf(), hash_source(src));

        let spec = RustSpec;
        let mut fns = Vec::new();
        collect_functions(&spec, tree.root_node(), src_bytes, &mut fns);
        for n in fns {
            if let Some(fa) = analyze_function(&spec, n, src_bytes, path) {
                report.functions.push(fa);
            }
        }
        finalize_early_returns(&mut report.functions);
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
            "while_expression" | "while_let_expression" | "loop_expression" | "for_expression" => NodeClass::Loop,
            "match_arm" => NodeClass::SwitchCase,
            "call_expression" | "method_call_expression" | "macro_invocation" => NodeClass::Call,
            "return_expression" => NodeClass::Return,
            "unsafe_block" => NodeClass::UnsafeBlock,
            "line_comment" | "block_comment" => NodeClass::Comment,
            "integer_literal" => NodeClass::IntLit,
            "float_literal" => NodeClass::FloatLit,
            "string_literal" | "raw_string_literal" | "byte_string_literal" | "raw_byte_string_literal" => NodeClass::StrLit,
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
            "identifier" | "type_identifier" | "field_identifier" | "scoped_identifier" | "scoped_type_identifier" => {
                NodeClass::Identifier
            }
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
            "unary_expression" | "compound_assignment_expr" | "assignment_expression" | "reference_expression"
            | "try_expression" => NodeClass::Operator,
            "block" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        match node.kind() {
            "call_expression" => {
                let f = node.child_by_field_name("function")?;
                // strip generic args and paths to last segment
                let text = f.utf8_text(src).ok()?;
                Some(strip_generics(text).to_string())
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
                        sig.inputs.push(Param { name, ty: TypeRef::new(ty) });
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
        None
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
