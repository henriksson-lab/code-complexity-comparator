use anyhow::{anyhow, Result};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{analyze_function, collect_functions, finalize_early_returns, LanguageSpec, NodeClass};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct RAnalyzer;

impl RAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for RAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for RAnalyzer {
    fn language(&self) -> Language { Language::R }

    fn extensions(&self) -> &[&'static str] { &["r", "R"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .map_err(|e| anyhow!("set language r: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::R, path.to_path_buf(), hash_source(src));

        let spec = RSpec;
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

struct RSpec;

impl LanguageSpec for RSpec {
    fn classify(&self, node: &Node, src: &[u8]) -> NodeClass {
        match node.kind() {
            "function_definition" => NodeClass::Function,
            "if_statement" => NodeClass::If,
            "for_statement" | "while_statement" | "repeat_statement" => NodeClass::Loop,
            "call" => {
                // R has no dedicated switch; `switch(x, case=..., case=...)` is
                // a plain call. Count it like a call here (cyclomatic counted
                // per-arm would require inspecting the arguments; we don't).
                NodeClass::Call
            }
            "return" => NodeClass::Return,
            "comment" => NodeClass::Comment,
            "integer" => NodeClass::IntLit,
            "float" | "complex" | "inf" | "nan" => NodeClass::FloatLit,
            "string" => NodeClass::StrLit,
            "true" => NodeClass::BoolLit(true),
            "false" => NodeClass::BoolLit(false),
            "identifier" => NodeClass::Identifier,
            "binary_operator" => {
                // R uses binary_operator for &&, ||, |, & and arithmetic.
                // The operator child holds the token.
                if let Some(op) = node.child_by_field_name("operator") {
                    if let Ok(op_text) = op.utf8_text(src) {
                        match op_text.trim() {
                            "&&" | "||" | "|" | "&" => return NodeClass::ShortCircuit,
                            _ => {}
                        }
                    }
                }
                NodeClass::Operator
            }
            "unary_operator" | "extract_operator" | "namespace_operator" => NodeClass::Operator,
            "braced_expression" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // R functions are anonymous. The name is the LHS of the enclosing
        // assignment: `name <- function(x) { ... }` or `name = function(...)`.
        // Walk up skipping parenthesized_expressions and grab the identifier.
        let mut cur = node.parent()?;
        loop {
            match cur.kind() {
                "binary_operator" => {
                    let op_text = cur
                        .child_by_field_name("operator")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("");
                    if matches!(op_text.trim(), "<-" | "<<-" | "=" | "->" | "->>") {
                        // Determine which side the function sits on.
                        let lhs = cur.child_by_field_name("lhs");
                        let rhs = cur.child_by_field_name("rhs");
                        let (name_side, fn_side) = match op_text.trim() {
                            "->" | "->>" => (rhs, lhs), // inverted: `fn -> name`
                            _ => (lhs, rhs),
                        };
                        if let Some(fs) = fn_side {
                            if fs.id() != node.id() {
                                return None;
                            }
                        }
                        if let Some(n) = name_side {
                            return Some(text_of(n, src));
                        }
                    }
                    return None;
                }
                "parenthesized_expression" | "braced_expression" => {
                    cur = cur.parent()?;
                }
                _ => return None,
            }
        }
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        let f = node.child_by_field_name("function")?;
        // Return the last identifier segment of the callee; strip namespace prefix.
        let text = f.utf8_text(src).ok()?;
        let last = text.rsplit("::").next().unwrap_or(text);
        let last = last.rsplit('$').next().unwrap_or(last);
        Some(last.trim().to_string())
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // R functions: parameters field holds `parameters` node containing
        // `parameter` children. Each parameter has an identifier name and
        // optionally a default value; types aren't annotated in base R.
        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.children(&mut cursor) {
                if p.kind() == "parameter" {
                    let name = p
                        .child_by_field_name("name")
                        .map(|n| text_of(n, src))
                        .unwrap_or_else(|| "_".to_string());
                    sig.inputs.push(Param {
                        name,
                        ty: TypeRef::new(""),
                    });
                }
            }
        }
        // R has no explicit return type annotation.
        sig
    }

    fn attributes(&self, _node: Node, _src: &[u8]) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

fn text_of(n: Node, src: &[u8]) -> String {
    n.utf8_text(src).unwrap_or("").trim().to_string()
}
