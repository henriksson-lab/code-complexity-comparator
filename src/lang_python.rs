use anyhow::{anyhow, Result};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{analyze_function, collect_functions, finalize_early_returns, LanguageSpec, NodeClass};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct PythonAnalyzer;

impl PythonAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for PythonAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for PythonAnalyzer {
    fn language(&self) -> Language { Language::Python }

    fn extensions(&self) -> &[&'static str] { &["py"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| anyhow!("set language python: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Python, path.to_path_buf(), hash_source(src));

        let spec = PythonSpec;
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

struct PythonSpec;

impl LanguageSpec for PythonSpec {
    fn classify(&self, node: &Node, _src: &[u8]) -> NodeClass {
        match node.kind() {
            "function_definition" => NodeClass::Function,
            // Python puts elif as a sibling of the if body, not a nested if.
            // Count as a decision point, but not a new nesting level.
            "if_statement" => NodeClass::If,
            "elif_clause" => NodeClass::SwitchCase,
            "else_clause" => NodeClass::Else,
            "while_statement" | "for_statement" => NodeClass::Loop,
            // `match` / `case` (Python 3.10+) and `except`/`finally` arms.
            "case_clause" | "except_clause" | "except_group_clause" => NodeClass::SwitchCase,
            "conditional_expression" => NodeClass::Ternary,
            "call" => NodeClass::Call,
            "return_statement" | "raise_statement" | "yield" => NodeClass::Return,
            "comment" => NodeClass::Comment,
            "integer" => NodeClass::IntLit,
            "float" => NodeClass::FloatLit,
            "string" | "concatenated_string" => NodeClass::StrLit,
            "true" => NodeClass::BoolLit(true),
            "false" => NodeClass::BoolLit(false),
            "identifier" => NodeClass::Identifier,
            "boolean_operator" => NodeClass::ShortCircuit,
            "binary_operator" | "unary_operator" | "comparison_operator"
            | "assignment" | "augmented_assignment" | "not_operator" => NodeClass::Operator,
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
        let f = node.child_by_field_name("function")?;
        match f.kind() {
            // `obj.method()` — attribute access.
            "attribute" => f
                .child_by_field_name("attribute")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string()),
            // `pkg.mod.foo()` — nested attribute access; grab the last segment.
            _ => {
                let text = f.utf8_text(src).ok()?;
                Some(text.rsplit('.').next().unwrap_or(text).trim().to_string())
            }
        }
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // Return type annotation.
        if let Some(rt) = node.child_by_field_name("return_type") {
            if let Ok(t) = rt.utf8_text(src) {
                sig.outputs.push(TypeRef::new(t.trim()));
            }
        }
        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.children(&mut cursor) {
                let (name, ty) = match p.kind() {
                    "identifier" => (p.utf8_text(src).unwrap_or("_").to_string(), String::new()),
                    "typed_parameter" => {
                        // Pattern: `name: Type`
                        let name = p
                            .child(0)
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        let ty = p
                            .child_by_field_name("type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        (name, ty)
                    }
                    "default_parameter" => {
                        let name = p
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        (name, String::new())
                    }
                    "typed_default_parameter" => {
                        let name = p
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("_")
                            .to_string();
                        let ty = p
                            .child_by_field_name("type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        (name, ty)
                    }
                    "list_splat_pattern" | "dictionary_splat_pattern" => {
                        let text = p.utf8_text(src).unwrap_or("_").to_string();
                        (text, String::new())
                    }
                    _ => continue,
                };
                sig.inputs.push(Param {
                    name,
                    ty: TypeRef::new(ty),
                });
            }
        }
        sig
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        // Async functions: the `async` keyword appears as a preceding token.
        if let Ok(text) = node.utf8_text(src) {
            if text.trim_start().starts_with("async ") {
                attrs.insert("async".into(), "true".into());
            }
        }
        // Decorators: the function is inside a `decorated_definition` node;
        // walk the parent and collect decorator texts.
        if let Some(parent) = node.parent() {
            if parent.kind() == "decorated_definition" {
                let mut cursor = parent.walk();
                let mut decorators = Vec::new();
                for c in parent.children(&mut cursor) {
                    if c.kind() == "decorator" {
                        if let Ok(t) = c.utf8_text(src) {
                            decorators.push(t.trim().to_string());
                        }
                    }
                }
                if !decorators.is_empty() {
                    attrs.insert("decorators".into(), decorators.join(" "));
                }
            }
        }
        attrs
    }
}
