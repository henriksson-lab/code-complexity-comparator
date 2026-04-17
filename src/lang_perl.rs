use anyhow::{anyhow, Result};
use crate::analyzer::LanguageAnalyzer;
use crate::core::{hash_source, Language, Param, Report, Signature, TypeRef};
use crate::walker::{analyze_function, collect_functions, finalize_early_returns, LanguageSpec, NodeClass};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct PerlAnalyzer;

impl PerlAnalyzer {
    pub fn new() -> Self { Self }
}

impl Default for PerlAnalyzer {
    fn default() -> Self { Self::new() }
}

impl LanguageAnalyzer for PerlAnalyzer {
    fn language(&self) -> Language { Language::Perl }

    fn extensions(&self) -> &[&'static str] { &["pl", "pm", "t"] }

    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_perl_next::LANGUAGE.into())
            .map_err(|e| anyhow!("set language perl: {}", e))?;
        let tree = parser.parse(src, None).ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let mut report = Report::new(Language::Perl, path.to_path_buf(), hash_source(src));

        let spec = PerlSpec;
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

struct PerlSpec;

impl LanguageSpec for PerlSpec {
    fn classify(&self, node: &Node, src: &[u8]) -> NodeClass {
        match node.kind() {
            "subroutine_declaration_statement" | "method_declaration_statement" => {
                NodeClass::Function
            }
            "conditional_statement" | "postfix_conditional_expression" | "conditional_expression" => {
                // conditional_statement covers if/unless; postfix/ternary are
                // single decision points too.
                if node.kind() == "conditional_expression" {
                    NodeClass::Ternary
                } else {
                    NodeClass::If
                }
            }
            // elsif is its own node (not a nested if_statement) — flat decision.
            "elsif" => NodeClass::SwitchCase,
            "else" => NodeClass::Else,
            "loop_statement" | "for_statement" | "cstyle_for_statement"
            | "postfix_loop_expression" | "postfix_for_expression" => NodeClass::Loop,
            // try/catch: each `catch` clause would be +1, but tree-sitter-perl
            // represents the whole try_statement as one node — count as a
            // single decision for now.
            "try_statement" => NodeClass::SwitchCase,
            "function_call_expression" | "method_call_expression"
            | "coderef_call_expression" | "ambiguous_function_call_expression"
            | "func0op_call_expression" | "func1op_call_expression" => NodeClass::Call,
            "return_expression" => NodeClass::Return,
            // last/next/redo are loop-exit jumps. Treat as Goto (they break
            // linear flow like gotos).
            "loopex_expression" => NodeClass::Goto,
            "goto_expression" => NodeClass::Goto,
            "comment" | "pod" => NodeClass::Comment,
            "number" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.contains('.') || text.contains('e') || text.contains('E') {
                    NodeClass::FloatLit
                } else {
                    NodeClass::IntLit
                }
            }
            "string_literal" | "interpolated_string_literal" | "command_string"
            | "heredoc_content" | "quoted_word_list" => NodeClass::StrLit,
            "boolean" => {
                let text = node.utf8_text(src).unwrap_or("");
                NodeClass::BoolLit(text.trim() == "true")
            }
            "identifier" | "bareword" | "varname" | "scalar" | "array" | "hash" => {
                NodeClass::Identifier
            }
            "binary_expression" | "equality_expression" | "relational_expression" => {
                // Check operator for && / || / //
                if let Some(op) = node.child_by_field_name("operator") {
                    if let Ok(op_text) = op.utf8_text(src) {
                        match op_text.trim() {
                            "&&" | "||" | "//" => return NodeClass::ShortCircuit,
                            _ => {}
                        }
                    }
                }
                NodeClass::Operator
            }
            "lowprec_logical_expression" => NodeClass::ShortCircuit,
            "unary_expression" | "assignment_expression" | "postinc_expression"
            | "preinc_expression" => NodeClass::Operator,
            "block" => NodeClass::Block,
            _ => NodeClass::None,
        }
    }

    fn function_name(&self, node: Node, src: &[u8]) -> Option<String> {
        // Both subroutine_declaration_statement and method_declaration_statement
        // have a required `name` field of type `bareword`.
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string())
    }

    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String> {
        match node.kind() {
            "method_call_expression" => node
                .child_by_field_name("method")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string()),
            _ => {
                // function_call_expression / func0op / func1op / ambiguous:
                // all have a `function` field. Strip package prefix.
                let f = node.child_by_field_name("function")?;
                let text = f.utf8_text(src).ok()?;
                let last = text.rsplit("::").next().unwrap_or(text);
                Some(last.trim_start_matches('&').trim().to_string())
            }
        }
    }

    fn signature(&self, node: Node, src: &[u8]) -> Signature {
        let mut sig = Signature::default();
        // Perl's `signature` child holds typed parameters for `sub foo($x, $y)`
        // declarations. Perl has no return-type annotations.
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            if c.kind() == "signature" {
                let mut cur2 = c.walk();
                for p in c.children(&mut cur2) {
                    match p.kind() {
                        "mandatory_parameter" | "optional_parameter" | "named_parameter"
                        | "slurpy_parameter" => {
                            let text = p.utf8_text(src).unwrap_or("").trim().to_string();
                            // Pick the first $/@/% variable token as the name.
                            let name = text
                                .split(|c: char| c == ' ' || c == '=' || c == ',')
                                .find(|t| t.starts_with('$') || t.starts_with('@') || t.starts_with('%'))
                                .unwrap_or(&text)
                                .to_string();
                            sig.inputs.push(Param {
                                name,
                                ty: TypeRef::new(""),
                            });
                        }
                        _ => {}
                    }
                }
                break;
            }
        }
        sig
    }

    fn attributes(&self, node: Node, src: &[u8]) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        // `my sub name { ... }` — lexical subroutine.
        if let Some(lex) = node.child_by_field_name("lexical") {
            if lex.utf8_text(src).unwrap_or("").trim() == "my" {
                attrs.insert("lexical".into(), "true".into());
            }
        }
        // Attributes like `sub foo :prototype(...) { }` or `:method`.
        if let Some(al) = node.child_by_field_name("attributes") {
            if let Ok(t) = al.utf8_text(src) {
                attrs.insert("attrs".into(), t.trim().to_string());
            }
        }
        if node.kind() == "method_declaration_statement" {
            attrs.insert("method".into(), "true".into());
        }
        attrs
    }
}
