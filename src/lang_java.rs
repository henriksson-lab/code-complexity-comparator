use anyhow::{anyhow, Result};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{
    analyze_function, collect_functions, collect_structs, finalize_early_returns, LanguageSpec,
    NodeClass,
};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct JavaAnalyzer;

impl JavaAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for JavaAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for JavaAnalyzer {
    fn language(&self) -> Language { Language::Java }

    fn extensions(&self) -> &[&'static str] { &["java"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|e| anyhow!("set language java: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Java, path.to_path_buf(), hash_source(src));

        let spec = JavaSpec;
        let mut fns = Vec::new();
        collect_functions(&spec, tree.root_node(), src_bytes, &mut fns);
        for n in fns {
            if let Some(fa) = analyze_function(&spec, n, src_bytes, path) {
                report.functions.push(fa);
            }
        }
        finalize_early_returns(&mut report.functions);
        collect_structs(&spec, tree.root_node(), src_bytes, path, &mut report.structs);
        Ok(report)
    }
}

struct JavaSpec;

impl LanguageSpec for JavaSpec {
    fn classify(&self, node: &Node, _src: &[u8]) -> NodeClass {
        match node.kind() {
            "method_declaration" | "constructor_declaration" => NodeClass::Function,
            "if_statement" => NodeClass::If,
            "while_statement" | "do_statement" | "for_statement" | "enhanced_for_statement" => NodeClass::Loop,
            // Each case/default label + each catch clause is a decision point.
            "switch_label" | "switch_rule" | "catch_clause" => NodeClass::SwitchCase,
            "ternary_expression" => NodeClass::Ternary,
            "method_invocation" | "object_creation_expression" | "explicit_constructor_invocation" => {
                NodeClass::Call
            }
            "return_statement" => NodeClass::Return,
            "throw_statement" => NodeClass::Return,
            "line_comment" | "block_comment" => NodeClass::Comment,
            "decimal_integer_literal" | "hex_integer_literal"
            | "octal_integer_literal" | "binary_integer_literal" => NodeClass::IntLit,
            "decimal_floating_point_literal" | "hex_floating_point_literal" => NodeClass::FloatLit,
            "string_literal" | "text_block" => NodeClass::StrLit,
            "character_literal" => NodeClass::CharLit,
            "true" => NodeClass::BoolLit(true),
            "false" => NodeClass::BoolLit(false),
            "identifier" | "type_identifier" => NodeClass::Identifier,
            "binary_expression" => {
                if let Some(op) = node.child_by_field_name("operator") {
                    match op.kind() {
                        "&&" | "||" => return NodeClass::ShortCircuit,
                        _ => {}
                    }
                }
                NodeClass::Operator
            }
            "unary_expression" | "update_expression" | "assignment_expression" | "cast_expression"
            | "instanceof_expression" => NodeClass::Operator,
            "block" | "constructor_body" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // method_declaration: field "name"
        // constructor_declaration: field "name"
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        match node.kind() {
            "method_invocation" => {
                // field "name" is the method, "object" is the receiver.
                if let Some(n) = node.child_by_field_name("name") {
                    return n.utf8_text(src).ok().map(|s| s.to_string());
                }
                None
            }
            "object_creation_expression" => {
                // `new Foo(...)` - use the constructed type.
                node.child_by_field_name("type")
                    .and_then(|n| n.utf8_text(src).ok())
                    .map(|s| format!("new {}", s.trim()))
            }
            "explicit_constructor_invocation" => {
                // super(...) / this(...)
                let text = node.utf8_text(src).ok()?;
                let head: String = text
                    .chars()
                    .take_while(|c| *c != '(' && !c.is_whitespace())
                    .collect();
                Some(head)
            }
            _ => None,
        }
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // Return type: only method_declaration has "type"; constructors don't.
        if let Some(ty) = node.child_by_field_name("type") {
            if let Ok(t) = ty.utf8_text(src) {
                sig.outputs.push(TypeRef::new(t.trim()));
            }
        }
        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.children(&mut cursor) {
                match p.kind() {
                    "formal_parameter" | "spread_parameter" | "receiver_parameter" => {
                        let ty = p
                            .child_by_field_name("type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let name = p
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        sig.inputs.push(Param { name, ty: TypeRef::new(ty) });
                    }
                    _ => {}
                }
            }
        }
        sig
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        if let Some(mods) = node.child_by_field_name("modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                match m.kind() {
                    "public" | "private" | "protected" | "static" | "final" | "abstract"
                    | "synchronized" | "native" | "strictfp" | "default" | "transient" | "volatile" => {
                        attrs.insert(m.kind().to_string(), "true".into());
                    }
                    "annotation" | "marker_annotation" => {
                        if let Ok(t) = m.utf8_text(src) {
                            // Store the last annotation text under "annotation".
                            // Multiple annotations get concatenated.
                            let entry: &mut String =
                                attrs.entry("annotation".to_string()).or_default();
                            if !entry.is_empty() {
                                entry.push(' ');
                            }
                            entry.push_str(t.trim());
                        }
                    }
                    _ => {}
                }
            }
        }
        // `throws` clause
        if let Some(th) = node.child_by_field_name("throws") {
            if let Ok(t) = th.utf8_text(src) {
                attrs.insert("throws".into(), t.trim().to_string());
            }
        }
        attrs
    }

    fn struct_kind(&self, node: &Node, _src: &[u8]) -> Option<&'static str> {
        match node.kind() {
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "record_declaration" => Some("record"),
            "enum_declaration" => Some("enum"),
            _ => None,
        }
    }

    fn struct_name(&self, node: Node, src: &[u8]) -> Option<String> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn struct_fields(&self, node: Node, src: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        // `record_declaration` has its fields directly in a `parameters` node.
        if node.kind() == "record_declaration" {
            if let Some(params) = node.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for p in params.children(&mut cursor) {
                    if p.kind() == "formal_parameter" {
                        let ty = p
                            .child_by_field_name("type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let name = p
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        out.push((name, ty));
                    }
                }
            }
            return out;
        }
        let body = match node.child_by_field_name("body") {
            Some(b) => b,
            None => return out,
        };
        let mut cursor = body.walk();
        for c in body.children(&mut cursor) {
            if c.kind() != "field_declaration" {
                continue;
            }
            let ty = c
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(src).ok())
                .unwrap_or("")
                .trim()
                .to_string();
            // `field_declaration` can emit one or more `variable_declarator`
            // children (Java allows `int a, b;`).
            let mut dcur = c.walk();
            for cc in c.children(&mut dcur) {
                if cc.kind() == "variable_declarator" {
                    let name = cc
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("_")
                        .to_string();
                    // Check for `[]` suffix attached to the declarator.
                    let raw = cc.utf8_text(src).unwrap_or("");
                    let ty_final = if raw.contains("[]") {
                        format!("{}[]", ty)
                    } else {
                        ty.clone()
                    };
                    out.push((name, ty_final));
                }
            }
        }
        out
    }

    fn struct_attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        if let Some(mods) = node.child_by_field_name("modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                match m.kind() {
                    "public" | "private" | "protected" | "static" | "final" | "abstract" => {
                        attrs.insert(m.kind().to_string(), "true".into());
                    }
                    "annotation" | "marker_annotation" => {
                        if let Ok(t) = m.utf8_text(src) {
                            attrs.insert("annotation".into(), t.trim().to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        attrs
    }
}
