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

pub struct CAnalyzer {
    cpp: bool,
}

impl CAnalyzer {
    pub fn c() -> Self {
        Self { cpp: false }
    }
    pub fn cpp() -> Self {
        Self { cpp: true }
    }
}

impl LanguageAnalyzer for CAnalyzer {
    fn language(&self) -> Language {
        if self.cpp {
            Language::Cpp
        } else {
            Language::C
        }
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
        let tree = parser
            .parse(src, None)
            .ok_or_else(|| anyhow!("parse failed"))?;
        let src_bytes = src.as_bytes();
        let lang = self.language();
        let mut report = Report::new(lang, path.to_path_buf(), hash_source(src));

        let spec = CSpec { cpp: self.cpp };
        let mut fns = Vec::new();
        collect_functions(&spec, tree.root_node(), src_bytes, &mut fns);
        for n in fns {
            if let Some(fa) = analyze_function(&spec, n, src_bytes, path) {
                if is_c_artifact_function(&fa) {
                    continue;
                }
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
            "for_statement" | "while_statement" | "do_statement" | "for_range_loop" => {
                NodeClass::Loop
            }
            "case_statement" => NodeClass::SwitchCase,
            "conditional_expression" => NodeClass::Ternary,
            "call_expression" => NodeClass::Call,
            "return_statement" => NodeClass::Return,
            "goto_statement" => NodeClass::Goto,
            "gnu_asm_expression" | "asm_statement" => NodeClass::AsmBlock,
            "comment" => NodeClass::Comment,
            "number_literal" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.contains('.')
                    || text.contains('e')
                    || text.contains('E')
                    || text.contains('p')
                    || text.contains('P')
                {
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
            "unary_expression"
            | "update_expression"
            | "pointer_expression"
            | "assignment_expression" => NodeClass::Operator,
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
            "field_expression" => f
                .child_by_field_name("field")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string()),
            _ => f.utf8_text(src).ok().map(|s| s.to_string()),
        };
        text.map(|s| s.trim().to_string())
    }

    fn call_args<'tree>(&self, node: Node<'tree>, _src: &[u8]) -> Vec<Node<'tree>> {
        // tree-sitter-c / tree-sitter-cpp wrap the positional argument list
        // of a `call_expression` in an `argument_list` node. Every named
        // child of that list is one positional argument; punctuation tokens
        // are unnamed and naturally skipped.
        let Some(args) = node.child_by_field_name("arguments") else {
            return Vec::new();
        };
        let mut cursor = args.walk();
        args.children(&mut cursor)
            .filter(|c| c.is_named())
            .collect()
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

fn is_c_artifact_function(fa: &crate::core::FunctionAnalysis) -> bool {
    matches!(
        fa.name.as_str(),
        "if" | "while" | "switch" | "do" | "void" | "NULL"
    ) || fa
        .signature
        .outputs
        .iter()
        .any(|out| matches!(out.text.as_str(), "ALL_MEMBERS" | "H5_ATTR_UNUSED"))
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
        "pointer_declarator"
        | "reference_declarator"
        | "parenthesized_declarator"
        | "abstract_pointer_declarator" => {
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

#[cfg(test)]
mod call_site_tests {
    use super::*;
    use crate::core::{ArgExpr, Constant};

    fn analyze(src: &str) -> Vec<crate::core::CallSite> {
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        report
            .functions
            .into_iter()
            .flat_map(|f| f.call_sites)
            .collect()
    }

    #[test]
    fn filters_c_syntax_artifact_function_names() {
        let src = r#"
            int real(void) { return 1; }
            int if(void) { return 2; }
            int while(void) { return 3; }
            int switch(void) { return 4; }
            int do(void) { return 5; }
            int void(void) { return 6; }
            int NULL(void) { return 7; }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let names: Vec<_> = report.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn filters_macro_loop_and_attribute_artifacts() {
        let src = r#"
            int real(void) { return 1; }
            ALL_MEMBERS(mt) { return 2; }
            herr_t H5CX_set_loc(hid_t
            #ifndef H5_HAVE_PARALLEL
                H5_ATTR_UNUSED
            #endif
                loc_id) { return 0; }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let names: Vec<_> = report.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"real"),
            "real function should remain: {names:?}"
        );
        assert!(
            !names.contains(&"mt") && !names.contains(&"loc_id"),
            "macro/attribute artifacts should be filtered: {names:?}"
        );
    }

    #[test]
    fn captures_param_const_and_nested_call() {
        // `caller(int x)` forwards x, passes a literal 7, and nests g(0).
        // Each shape exercises a distinct ArgExpr variant.
        let src = r#"
            int g(int z) { return z; }
            int f(int a, int b) { return a + b; }
            int caller(int x) {
                return f(x, 7) + f(g(0), x);
            }
        "#;
        let sites = analyze(src);
        // Only the call sites inside `caller` are interesting; g/f bodies
        // contribute no calls of their own.
        let caller_sites: Vec<_> = sites
            .iter()
            .filter(|s| s.callee == "f" || s.callee == "g")
            .collect();
        assert_eq!(
            caller_sites.len(),
            3,
            "expected f, f, g; got {:?}",
            caller_sites
        );

        // Find the first f(x, 7) call site.
        let f_with_literal = caller_sites
            .iter()
            .find(|s| {
                s.callee == "f"
                    && matches!(
                        s.args.get(1),
                        Some(ArgExpr::Const {
                            value: Constant::Int { value: 7, .. }
                        })
                    )
            })
            .expect("f(x, 7) site");
        assert!(matches!(
            f_with_literal.args.first(),
            Some(ArgExpr::Param { index: 0 })
        ));

        // f(g(0), x) — first arg is NestedCall, second is Param.
        let f_with_nested = caller_sites
            .iter()
            .find(|s| s.callee == "f" && matches!(s.args.first(), Some(ArgExpr::NestedCall { .. })))
            .expect("f(g(0), x) site");
        if let Some(ArgExpr::NestedCall { callee }) = f_with_nested.args.first() {
            assert_eq!(callee, "g");
        }
        assert!(matches!(
            f_with_nested.args.get(1),
            Some(ArgExpr::Param { index: 0 })
        ));
    }

    #[test]
    fn in_loop_flag_set_inside_for() {
        let src = r#"
            void g(int z);
            void caller(int n) {
                g(0);
                for (int i = 0; i < n; i++) {
                    g(i);
                }
            }
        "#;
        let sites = analyze(src);
        let g_sites: Vec<_> = sites.iter().filter(|s| s.callee == "g").collect();
        // Two call sites total. The first has in_loop=false, the second true.
        // Ordering matches lexical order because the walker descends in
        // child order.
        assert_eq!(g_sites.len(), 2);
        assert!(!g_sites[0].in_loop);
        assert!(g_sites[1].in_loop);
    }

    #[test]
    fn path_cond_captures_if_else_with_negation() {
        // `if (x < 0) g(...) else g(...)` — both branches' calls should
        // get a path_cond, with the else branch's predicate being the
        // negation of the if's condition. After canonicalisation the two
        // forms differ only by operator (flipped + negated), confirming
        // the guard stack flips correctly.
        let src = r#"
            int g(int z);
            int caller(int x) {
                if (x < 0) {
                    return g(5);
                } else {
                    return g(7);
                }
            }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let caller = report
            .functions
            .iter()
            .find(|f| f.name == "caller")
            .expect("caller fn");
        let g_sites: Vec<_> = caller
            .call_sites
            .iter()
            .filter(|s| s.callee == "g")
            .collect();
        assert_eq!(g_sites.len(), 2);

        // Both should have a path_cond — neither is unconditional.
        let pc0 = g_sites[0].path_cond.as_ref().expect("if-branch path cond");
        let pc1 = g_sites[1]
            .path_cond
            .as_ref()
            .expect("else-branch path cond");

        // The if-branch and else-branch predicates should be each other's
        // negation under canonicalisation: same operands, complementary ops.
        use crate::core::{Predicate, Term};
        let (op0, l0, r0) = match pc0 {
            Predicate::Cmp { op, left, right } => (*op, left, right),
            other => panic!("expected Cmp, got {:?}", other),
        };
        let (op1, l1, r1) = match pc1 {
            Predicate::Cmp { op, left, right } => (*op, left, right),
            other => panic!("expected Cmp, got {:?}", other),
        };
        assert_eq!(op0, op0.negate().negate(), "sanity");
        assert_eq!(op1, op0.negate(), "else branch should negate the op");
        // Operand structures match.
        assert!(matches!(l0, Term::Const { .. }) && matches!(l1, Term::Const { .. }));
        assert!(matches!(r0, Term::Param { index: 0 }));
        assert!(matches!(r1, Term::Param { index: 0 }));
    }

    #[test]
    fn ternary_path_cond_captured() {
        // `x < 0 ? g(5) : g(7)` — same shape as if/else, both `g` calls
        // should land with complementary path conditions.
        let src = r#"
            int g(int z);
            int caller(int x) {
                return x < 0 ? g(5) : g(7);
            }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let caller = report
            .functions
            .iter()
            .find(|f| f.name == "caller")
            .expect("caller fn");
        let g_sites: Vec<_> = caller
            .call_sites
            .iter()
            .filter(|s| s.callee == "g")
            .collect();
        assert_eq!(g_sites.len(), 2);
        let pc0 = g_sites[0]
            .path_cond
            .as_ref()
            .expect("ternary then-branch path");
        let pc1 = g_sites[1]
            .path_cond
            .as_ref()
            .expect("ternary else-branch path");

        use crate::core::Predicate;
        let (op0, op1) = match (pc0, pc1) {
            (Predicate::Cmp { op: a, .. }, Predicate::Cmp { op: b, .. }) => (*a, *b),
            other => panic!("expected Cmp/Cmp, got {:?}", other),
        };
        assert_eq!(
            op1,
            op0.negate(),
            "ternary alternative should negate the op"
        );
    }

    #[test]
    fn switch_case_path_cond_captures_eq() {
        // Each case arm's call should be guarded by `switch_value == case_value`.
        // `default:` yields an opaque predicate — uncheckable, but the call
        // does still get a path_cond (Opaque) so the user can see something
        // gated it.
        let src = r#"
            int g(int z);
            int caller(int x) {
                switch (x) {
                    case 1: return g(10);
                    case 2: return g(20);
                    default: return g(99);
                }
            }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let caller = report
            .functions
            .iter()
            .find(|f| f.name == "caller")
            .expect("caller fn");
        let g_sites: Vec<_> = caller
            .call_sites
            .iter()
            .filter(|s| s.callee == "g")
            .collect();
        assert_eq!(g_sites.len(), 3);

        use crate::core::{CmpOp, Constant, Predicate, Term};
        // case 1 → `x == 1` (after canonicalisation: Const-on-left)
        match &g_sites[0].path_cond {
            Some(Predicate::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            }) => {
                assert!(matches!(
                    left,
                    Term::Const {
                        value: Constant::Int { value: 1, .. }
                    }
                ));
                assert!(matches!(right, Term::Param { index: 0 }));
            }
            other => panic!("case 1 expected Eq, got {:?}", other),
        }
        // case 2 → `x == 2`
        match &g_sites[1].path_cond {
            Some(Predicate::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            }) => {
                assert!(matches!(
                    left,
                    Term::Const {
                        value: Constant::Int { value: 2, .. }
                    }
                ));
                assert!(matches!(right, Term::Param { index: 0 }));
            }
            other => panic!("case 2 expected Eq, got {:?}", other),
        }
        // default → opaque (uncheckable)
        assert!(matches!(
            &g_sites[2].path_cond,
            Some(Predicate::Opaque { .. })
        ));
    }

    #[test]
    fn nested_if_conjunctions_are_canonicalized() {
        // Nested ifs produce a multi-element And. The walker's path_cond
        // already canonicalises, so the order of conjuncts is determined by
        // structural sort — not by source nesting. The same set of guards
        // expressed in any order should produce the same form.
        let src = r#"
            int g(int z);
            int caller(int x, int y) {
                if (x > 0) {
                    if (y < 5) {
                        g(0);
                    }
                }
                return 0;
            }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let caller = report
            .functions
            .iter()
            .find(|f| f.name == "caller")
            .expect("caller fn");
        let g_site = caller
            .call_sites
            .iter()
            .find(|s| s.callee == "g")
            .expect("g call site");
        let pc = g_site.path_cond.as_ref().expect("nested-guards path cond");
        // Should be an And of 2 Cmps. Could be flat or single after dedup;
        // we expect 2 conjuncts here.
        use crate::core::Predicate;
        match pc {
            Predicate::And { items } => {
                assert_eq!(items.len(), 2, "expected 2 conjuncts, got {:?}", items);
                for item in items {
                    assert!(matches!(item, Predicate::Cmp { .. }));
                }
            }
            other => panic!("expected And, got {:?}", other),
        }
    }

    #[test]
    fn binary_operator_set_counts_by_symbol() {
        // Exercises every family: arithmetic, shift, bitwise, logical, plus
        // the prefix `!` / `~`. Crucially, the unary `*p` (deref) and `&x`
        // (address-of) must NOT inflate mul / bit_and.
        let src = r#"
            int f(int a, int b, int *p) {
                int s = a + b - a * b / b % 2;
                int sh = (a << 1) >> 1;
                int bw = (a & b) | (a ^ b);
                int nb = ~a;
                int *q = &b;
                int d = *p;
                if (a && b || !a) {
                    return s + sh + bw + nb + d + *q;
                }
                return 0;
            }
        "#;
        let report = CAnalyzer::c()
            .analyze_source(src, std::path::Path::new("t.c"))
            .expect("parse");
        let f = report
            .functions
            .iter()
            .find(|f| f.name == "f")
            .expect("fn f");
        let ops = &f.metrics.binary_operators;
        assert_eq!(ops.add, 6, "a+b, then 5 in the s+sh+bw+nb+d+*q sum");
        assert_eq!(ops.sub, 1);
        assert_eq!(ops.mul, 1, "binary * only; deref *p / *q excluded");
        assert_eq!(ops.div, 1);
        assert_eq!(ops.rem, 1);
        assert_eq!(ops.shl, 1);
        assert_eq!(ops.shr, 1);
        assert_eq!(ops.bit_and, 1, "binary & only; address-of &b excluded");
        assert_eq!(ops.bit_or, 1);
        assert_eq!(ops.bit_xor, 1);
        assert_eq!(ops.bit_not, 1);
        assert_eq!(ops.logic_and, 1);
        assert_eq!(ops.logic_or, 1);
        assert_eq!(ops.logic_not, 1);
    }

    #[test]
    fn unrecognised_arg_falls_to_opaque() {
        // `a + b` is a binary expression — no current ArgExpr variant for
        // arithmetic, so it must land in Opaque with the textual form.
        let src = r#"
            int g(int z) { return z; }
            int caller(int a, int b) { return g(a + b); }
        "#;
        let sites = analyze(src);
        let g_site = sites.iter().find(|s| s.callee == "g").expect("g site");
        match g_site.args.first() {
            Some(ArgExpr::Opaque { text }) => {
                assert!(text.contains("a") && text.contains("b") && text.contains('+'));
            }
            other => panic!("expected Opaque, got {:?}", other),
        }
    }
}
