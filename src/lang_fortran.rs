use anyhow::{anyhow, Result};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{analyze_function, collect_functions, finalize_early_returns, LanguageSpec, NodeClass};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct FortranAnalyzer;

impl FortranAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for FortranAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for FortranAnalyzer {
    fn language(&self) -> Language { Language::Fortran }

    fn extensions(&self) -> &[&'static str] { &["f", "f90", "f95", "f03", "f08", "for", "ftn"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_fortran::LANGUAGE.into())
            .map_err(|e| anyhow!("set language fortran: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Fortran, path.to_path_buf(), hash_source(src));

        let spec = FortranSpec;
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

struct FortranSpec;

impl LanguageSpec for FortranSpec {
    fn classify(&self, node: &Node, src: &[u8]) -> NodeClass {
        match node.kind() {
            "function" | "subroutine" | "module_procedure" => NodeClass::Function,
            "if_statement" | "arithmetic_if_statement" | "where_statement" => NodeClass::If,
            // elseif and elsewhere are flat decisions; the grammar emits them
            // as distinct sibling clauses of the outer if/where.
            "elseif_clause" | "elsewhere_clause" => NodeClass::SwitchCase,
            "else_clause" => NodeClass::Else,
            "do_loop_statement" | "do_label_statement" | "while_statement"
            | "forall_statement" | "concurrent_statement" => NodeClass::Loop,
            // select case / select rank / select type — each case arm is a
            // decision point.
            "case_statement" => NodeClass::SwitchCase,
            // select_case_statement itself isn't an if — the case_statement
            // children carry the cost.
            "conditional_expression" => NodeClass::Ternary,
            "call_expression" | "subroutine_call" | "defined_io_procedure" => NodeClass::Call,
            "stop_statement" => NodeClass::Return,
            "keyword_statement" => {
                // `return`, `cycle`, `exit`, `continue`. Treat return as Return
                // and cycle/exit as Goto (loop-exit jumps).
                let text = node.utf8_text(src).unwrap_or("").trim().to_ascii_lowercase();
                if text.starts_with("return") {
                    NodeClass::Return
                } else if text.starts_with("cycle") || text.starts_with("exit") {
                    NodeClass::Goto
                } else {
                    NodeClass::None
                }
            }
            "comment" => NodeClass::Comment,
            "number_literal" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.contains('.') || text.to_ascii_lowercase().contains('e')
                    || text.to_ascii_lowercase().contains('d')
                {
                    NodeClass::FloatLit
                } else {
                    NodeClass::IntLit
                }
            }
            "complex_literal" => NodeClass::FloatLit,
            "string_literal" | "hollerith_constant" => NodeClass::StrLit,
            "boolean_literal" => {
                let text = node.utf8_text(src).unwrap_or("").to_ascii_lowercase();
                NodeClass::BoolLit(text.contains("true") || text.contains(".t."))
            }
            "identifier" | "name" | "type_name" | "method_name" | "module_name" => {
                NodeClass::Identifier
            }
            "logical_expression" => {
                // `.and.` / `.or.` short-circuit in Fortran.
                NodeClass::ShortCircuit
            }
            "binary_expression" | "relational_expression" | "math_expression"
            | "unary_expression" | "concatenation_expression" | "assignment_statement"
            | "assignment" => NodeClass::Operator,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // `function` / `subroutine` contain a `function_statement` /
        // `subroutine_statement` child whose `name` field is the declared name.
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            match c.kind() {
                "function_statement" | "subroutine_statement" | "module_procedure_statement" => {
                    if let Some(n) = c.child_by_field_name("name") {
                        if let Ok(t) = n.utf8_text(src) {
                            return Some(t.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        match node.kind() {
            "call_expression" => node
                .child_by_field_name("function")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.trim().to_string()),
            "subroutine_call" => {
                let s = node.child_by_field_name("subroutine")?;
                s.utf8_text(src).ok().map(|t| t.trim().to_string())
            }
            _ => None,
        }
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // Find the *_statement child with the name/parameters/type fields.
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            if matches!(
                c.kind(),
                "function_statement" | "subroutine_statement" | "module_procedure_statement"
            ) {
                if let Some(ty) = c.child_by_field_name("type") {
                    if let Ok(t) = ty.utf8_text(src) {
                        sig.outputs.push(TypeRef::new(t.trim()));
                    }
                }
                if let Some(params) = c.child_by_field_name("parameters") {
                    let mut cur2 = params.walk();
                    for p in params.children(&mut cur2) {
                        if p.kind() == "identifier" || p.kind() == "name" {
                            if let Ok(t) = p.utf8_text(src) {
                                sig.inputs.push(Param {
                                    name: t.to_string(),
                                    ty: TypeRef::new(""),
                                });
                            }
                        }
                    }
                }
                break;
            }
        }
        sig
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            if matches!(
                c.kind(),
                "function_statement" | "subroutine_statement" | "module_procedure_statement"
            ) {
                let mut cur2 = c.walk();
                for cc in c.children(&mut cur2) {
                    match cc.kind() {
                        "procedure_qualifier" | "procedure_attributes" => {
                            if let Ok(t) = cc.utf8_text(src) {
                                attrs.insert("qualifier".into(), t.trim().to_string());
                            }
                        }
                        "language_binding" => {
                            if let Ok(t) = cc.utf8_text(src) {
                                attrs.insert("binding".into(), t.trim().to_string());
                            }
                        }
                        _ => {}
                    }
                }
                break;
            }
        }
        if node.kind() == "subroutine" {
            attrs.insert("kind".into(), "subroutine".into());
        }
        attrs
    }
}
