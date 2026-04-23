use anyhow::{anyhow, Result};
use crate::walker::{
    analyze_function, collect_functions, collect_structs, finalize_early_returns, LanguageSpec,
    NodeClass,
};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct CAnalyzer {
    cpp: bool,
}

impl CAnalyzer {
    pub fn c() -> Self { Self { cpp: false } }
    pub fn cpp() -> Self { Self { cpp: true } }
}

impl LanguageAnalyzer for CAnalyzer {
    fn language(&self) -> Language {
        if self.cpp { Language::Cpp } else { Language::C }
    }

    fn extensions(&self) -> &[&'static str] {
        if self.cpp {
            &["cc", "cpp", "cxx", "hpp", "hh", "hxx"]
        } else {
            &["c", "h"]
        }
    }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        if self.cpp {
            parser
                .set_language(&tree_sitter_cpp::LANGUAGE.into())
                .map_err(|e| anyhow!("set language cpp: {}", e))?;
        } else {
            parser
                .set_language(&tree_sitter_c::LANGUAGE.into())
                .map_err(|e| anyhow!("set language c: {}", e))?;
        }
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let lang = self.language();
        let mut report = Report::new(lang, path.to_path_buf(), hash_source(src));

        let spec = CSpec { cpp: self.cpp };
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

struct CSpec {
    #[allow(dead_code)]
    cpp: bool,
}

impl LanguageSpec for CSpec {
    fn classify(&self, node: &Node, src: &[u8]) -> NodeClass {
        match node.kind() {
            "function_definition" => NodeClass::Function,
            "if_statement" => NodeClass::If,
            "else_clause" => NodeClass::Else,
            "for_statement" | "while_statement" | "do_statement" | "for_range_loop" => NodeClass::Loop,
            "case_statement" => NodeClass::SwitchCase,
            "conditional_expression" => NodeClass::Ternary,
            "call_expression" => NodeClass::Call,
            "return_statement" => NodeClass::Return,
            "goto_statement" => NodeClass::Goto,
            "gnu_asm_expression" | "asm_statement" => NodeClass::AsmBlock,
            "comment" => NodeClass::Comment,
            "number_literal" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.contains('.') || text.contains('e') || text.contains('E') || text.contains('p') || text.contains('P') {
                    NodeClass::FloatLit
                } else {
                    NodeClass::IntLit
                }
            }
            "string_literal" | "concatenated_string" | "raw_string_literal" => NodeClass::StrLit,
            "char_literal" => NodeClass::CharLit,
            "true" => NodeClass::BoolLit(true),
            "false" => NodeClass::BoolLit(false),
            "identifier" | "field_identifier" | "type_identifier" => NodeClass::Identifier,
            "logical_expression" => NodeClass::ShortCircuit,
            "binary_expression" => {
                if let Some(op) = node.child_by_field_name("operator") {
                    match op.kind() {
                        "&&" | "||" => return NodeClass::ShortCircuit,
                        _ => {}
                    }
                }
                NodeClass::Operator
            }
            "unary_expression" | "update_expression" | "pointer_expression" | "assignment_expression" => {
                NodeClass::Operator
            }
            "compound_statement" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // function_definition -> declarator -> (function_declarator -> identifier | ptr wraps)
        let decl = node.child_by_field_name("declarator")?;
        extract_declarator_name(decl, src)
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        let f = node.child_by_field_name("function")?;
        // strip casts and field accesses
        let text = match f.kind() {
            "field_expression" => {
                f.child_by_field_name("field").and_then(|n| n.utf8_text(src).ok()).map(|s| s.to_string())
            }
            _ => f.utf8_text(src).ok().map(|s| s.to_string()),
        };
        text.map(|s| s.trim().to_string())
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // return type: field "type" on the function_definition
        if let Some(ty) = node.child_by_field_name("type") {
            if let Ok(t) = ty.utf8_text(src) {
                sig.outputs.push(TypeRef::new(t.trim()));
            }
        }
        // parameters: function_declarator's parameters
        if let Some(decl) = node.child_by_field_name("declarator") {
            if let Some(fd) = find_function_declarator(decl) {
                if let Some(params) = fd.child_by_field_name("parameters") {
                    let mut cursor = params.walk();
                    for p in params.children(&mut cursor) {
                        if p.kind() == "parameter_declaration" {
                            let ty = p
                                .child_by_field_name("type")
                                .and_then(|n| n.utf8_text(src).ok())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            let name = p
                                .child_by_field_name("declarator")
                                .and_then(|d| extract_declarator_name(d, src))
                                .unwrap_or_else(|| "_".to_string());
                            if !ty.is_empty() || !name.is_empty() {
                                sig.inputs.push(Param {
                                    name,
                                    ty: TypeRef::new(ty),
                                });
                            }
                        }
                    }
                }
            }
        }
        sig
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        // Detect `static`, `inline`, `extern` modifiers on function_definition.
        let text = node.utf8_text(src).unwrap_or("");
        let head = text.split('{').next().unwrap_or("");
        for kw in ["static", "inline", "extern", "_Noreturn", "__attribute__"] {
            if head.contains(kw) {
                attrs.insert(kw.trim_matches('_').to_string(), "true".into());
            }
        }
        attrs
    }

    fn struct_kind(&self, node: &Node, _src: &[u8]) -> Option<&'static str> {
        match node.kind() {
            // tree-sitter-c wraps a top-level declaration in `declaration`,
            // but `struct_specifier` / `union_specifier` is also the
            // definition node when it carries a `body`. We only care about
            // definitions (those with a body), not forward declarations.
            "struct_specifier" if has_body(*node) => Some("struct"),
            "union_specifier" if has_body(*node) => Some("union"),
            // C++ `class Foo { ... };` — tree-sitter-cpp emits `class_specifier`.
            "class_specifier" if has_body(*node) => Some("class"),
            _ => None,
        }
    }

    fn struct_name(&self, node: Node, src: &[u8]) -> Option<String> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn struct_fields(&self, node: Node, src: &[u8]) -> Vec<(String, String)> {
        let body = match node.child_by_field_name("body") {
            Some(b) => b,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
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
            // A single `field_declaration` can declare multiple fields
            // (`int a, b, c;`). Each shows up as a separate declarator
            // sibling on the `field_declaration` node.
            let mut dcur = c.walk();
            let mut emitted = false;
            for cc in c.children(&mut dcur) {
                if cc.kind().ends_with("declarator") || cc.kind() == "field_identifier" {
                    if let Some(name) = extract_field_declarator_name(cc, src) {
                        let ty_final = augment_type_from_declarator(&ty, cc, src);
                        out.push((name, ty_final));
                        emitted = true;
                    }
                }
            }
            if !emitted && !ty.is_empty() {
                // Anonymous (e.g. anonymous union/struct inside). Skip —
                // caller can still see the outer struct's field_count.
            }
        }
        out
    }

    fn struct_attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        let text = node.utf8_text(src).unwrap_or("");
        if text.contains("__packed__") || text.contains("__attribute__((packed))") {
            attrs.insert("packed".into(), "true".into());
        }
        attrs
    }
}

fn has_body(node: Node) -> bool {
    node.child_by_field_name("body").is_some()
}

fn extract_field_declarator_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "field_identifier" | "identifier" => node.utf8_text(src).ok().map(|s| s.to_string()),
        "pointer_declarator"
        | "array_declarator"
        | "function_declarator"
        | "parenthesized_declarator"
        | "reference_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_field_declarator_name(d, src)),
        _ => {
            let mut cur = node.walk();
            for c in node.children(&mut cur) {
                if let Some(n) = extract_field_declarator_name(c, src) {
                    return Some(n);
                }
            }
            None
        }
    }
}

/// Re-derive the effective field type from the declarator. A declaration
/// like `int *p;` has `type = "int"` and a `pointer_declarator`, so we
/// prepend a `*` so the classifier buckets the field as Pointer rather than
/// Int. Same for `int a[10]` → treat as Array.
fn augment_type_from_declarator(base: &str, node: Node, _src: &[u8]) -> String {
    match node.kind() {
        "pointer_declarator" => format!("{} *", base),
        "array_declarator" => format!("{}[]", base),
        "reference_declarator" => format!("{} &", base),
        _ => base.to_string(),
    }
}

fn extract_declarator_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" => node.utf8_text(src).ok().map(|s| s.to_string()),
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_declarator_name(d, src)),
        "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" | "abstract_pointer_declarator" => {
            if let Some(d) = node.child_by_field_name("declarator") {
                return extract_declarator_name(d, src);
            }
            let mut cursor = node.walk();
            let child = node.children(&mut cursor).find(|c| c.is_named());
            child.and_then(|d| extract_declarator_name(d, src))
        }
        "init_declarator" | "array_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_declarator_name(d, src)),
        "qualified_identifier" | "destructor_name" | "operator_name" => {
            node.utf8_text(src).ok().map(|s| s.to_string())
        }
        _ => {
            // fall back: search any identifier child
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if let Some(n) = extract_declarator_name(c, src) {
                    return Some(n);
                }
            }
            None
        }
    }
}

fn find_function_declarator(node: Node) -> Option<Node> {
    if node.kind() == "function_declarator" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if let Some(n) = find_function_declarator(c) {
            return Some(n);
        }
    }
    None
}
