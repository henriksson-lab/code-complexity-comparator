//! Generic metrics walker. Each language crate implements `LanguageSpec` to
//! classify tree-sitter nodes; the walker handles nesting, accumulation and
//! final metric computation uniformly across languages.

use crate::core::{
    classify_type, ArgExpr, BinaryOperatorSet, Call, CallSite, CmpOp, Constant, FunctionAnalysis,
    Halstead, Location, Metrics, Predicate, Signature, StructAnalysis, StructField, StructMetrics,
    Term, TypeRef,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeClass {
    None,
    /// Top-level function definition; start a new analysis scope.
    Function,
    /// if / elif. Increments if-depth and cyclomatic/cognitive.
    If,
    /// else branch (not else-if). Does not add to cyclomatic in classic sense.
    Else,
    /// while / for / do-while / loop / foreach.
    Loop,
    /// case / match arm / default; +1 cyclomatic per case.
    SwitchCase,
    /// && || short-circuit; +1 cyclomatic each.
    ShortCircuit,
    /// Ternary ? : ; +1 cyclomatic.
    Ternary,
    /// Rust `?` propagation. It is one conditional early-exit branch.
    TryPropagate,
    /// Function/method call.
    Call,
    /// Return statement.
    Return,
    /// C goto.
    Goto,
    /// Rust `unsafe { ... }` block.
    UnsafeBlock,
    /// Inline asm block.
    AsmBlock,
    /// Comment.
    Comment,
    /// Integer literal.
    IntLit,
    /// Floating point literal.
    FloatLit,
    /// String literal (possibly multi-part).
    StrLit,
    /// Character literal.
    CharLit,
    /// Boolean literal.
    BoolLit(bool),
    /// Binary or unary operator node (for Halstead).
    Operator,
    /// Identifier occurrence (for Halstead).
    Identifier,
    /// A { ... } block. Used to bump combined-nesting only when it contains
    /// further control-flow; we track combined via if/loop increments directly.
    Block,
    /// Keyword like `if`, `while`, `return` - counted as Halstead operator.
    Keyword,
}

pub trait LanguageSpec: Send + Sync {
    fn classify(&self, node: &Node, src: &[u8]) -> NodeClass;

    /// Extract the function's name from a Function-classified node.
    fn function_name(&self, node: Node, src: &[u8]) -> Option<String>;

    /// Extract the callee name from a Call-classified node.
    fn call_callee(&self, node: Node, src: &[u8]) -> Option<String>;

    /// Return the named child nodes that represent each positional argument
    /// of a Call-classified node, in source order. The walker lowers each
    /// returned node into an `ArgExpr` for cross-translation comparison.
    /// Default is empty: a language without coverage simply records call
    /// sites with no `args`, and downstream argument-flow checks degrade to
    /// "uncheckable" for that side. New languages can opt in incrementally.
    fn call_args<'tree>(&self, _node: Node<'tree>, _src: &[u8]) -> Vec<Node<'tree>> {
        Vec::new()
    }

    /// Extract parameter list + return type for a Function node.
    fn signature(&self, node: Node, src: &[u8]) -> Signature;

    /// Parse an Int literal's numeric value from its textual representation.
    fn parse_int(&self, text: &str) -> Option<i64> {
        parse_int_default(text)
    }

    /// Parse a Float literal.
    fn parse_float(&self, text: &str) -> Option<f64> {
        text.trim_end_matches(|c: char| {
            c == 'f' || c == 'F' || c == 'L' || c == 'l' || c == 'd' || c == 'D'
        })
        .parse()
        .ok()
    }

    /// Parse a string literal into its decoded content.
    fn parse_string(&self, text: &str) -> Option<String> {
        decode_string_default(text)
    }

    /// Extract the "original" name (e.g. from #[link_name] in Rust).
    fn original_name(&self, _node: Node, _src: &[u8]) -> Option<String> {
        None
    }

    /// Extract the enclosing class / impl-target / struct name for the
    /// function, if any. Walk the parent chain and return the nearest class-
    /// like container. Default `None` covers C, Fortran, and similar
    /// languages that don't model methods this way.
    fn enclosing_type(&self, _node: Node, _src: &[u8]) -> Option<String> {
        None
    }

    /// Additional per-language attributes to stash on the function record.
    fn attributes(&self, _node: Node, _src: &[u8]) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Node kinds that should be treated as an operator *string* for Halstead
    /// (e.g. "+", "-", "&&"). The walker uses the node's own text unless
    /// overridden.
    fn operator_text(&self, node: Node, src: &[u8]) -> Option<String> {
        node.utf8_text(src).ok().map(|s| s.to_string())
    }

    /// Returns `Some(kind)` if the node is a struct-like declaration to be
    /// recorded on the report (`"struct"`, `"class"`, `"union"`, `"record"`,
    /// `"derived_type"`). Default `None` suppresses struct extraction for
    /// languages that don't carry structs at the source level (Perl, R).
    fn struct_kind(&self, _node: &Node, _src: &[u8]) -> Option<&'static str> {
        None
    }

    /// Extract the declared name from a struct-like node.
    fn struct_name(&self, _node: Node, _src: &[u8]) -> Option<String> {
        None
    }

    /// Extract the (name, type) pairs for each field declared directly on
    /// the struct-like node. Type text is stored verbatim; classification
    /// happens in the walker.
    fn struct_fields(&self, _node: Node, _src: &[u8]) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Per-struct attribute bag (e.g. `repr`, `packed`, `visibility`).
    fn struct_attributes(&self, _node: Node, _src: &[u8]) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

fn parse_int_default(text: &str) -> Option<i64> {
    // Detect the radix prefix *first*, then consume only digits valid for
    // that radix. Everything after the first non-digit is the type suffix
    // (`u32`, `ULL`, `i64`, `usize`, …) and is discarded. The prior version
    // stripped trailing alphabetic characters before detecting the prefix,
    // which turned `0xFF` into `0` (the `x`, `F`, `F` were all stripped as
    // "suffix") and silently bucketed every hex literal as zero.
    let t = text.trim();
    let neg = t.starts_with('-');
    let t = if neg { &t[1..] } else { t };
    let (radix, body) = if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16u32, rest)
    } else if let Some(rest) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (2u32, rest)
    } else if let Some(rest) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        (8u32, rest)
    } else {
        (10u32, t)
    };
    let mut end = 0usize;
    for (i, ch) in body.char_indices() {
        if ch == '_' || ch == '\'' {
            end = i + ch.len_utf8();
            continue;
        }
        if ch.is_digit(radix) {
            end = i + ch.len_utf8();
            continue;
        }
        break;
    }
    let digits: String = body[..end]
        .chars()
        .filter(|c| *c != '_' && *c != '\'')
        .collect();
    if digits.is_empty() {
        return None;
    }
    // Parse as unsigned so constants like 0xFFFFFFFF round-trip without
    // tripping signed-overflow, then reinterpret as i64.
    let u = u64::from_str_radix(&digits, radix).ok()?;
    let v = u as i64;
    Some(if neg { v.wrapping_neg() } else { v })
}

fn decode_string_default(text: &str) -> Option<String> {
    // Accept a variety of prefixes/suffixes: u8"...", L"...", b"...", r"...",
    // ""..."" (C adjacent strings), R"delim(...)delim" etc. Greedy: find
    // content between outermost matching quotes; fall back to raw text.
    let t = text.trim();
    // handle raw rust: r"..." or r#"..."#
    if let Some(rest) = t.strip_prefix('r').or_else(|| {
        t.strip_prefix('b')
            .filter(|_| t.starts_with("br"))
            .map(|_| &t[2..])
    }) {
        if rest.starts_with('#') || rest.starts_with('"') {
            let hashes = rest.chars().take_while(|c| *c == '#').count();
            let after_hash = &rest[hashes..];
            if let Some(after_q) = after_hash.strip_prefix('"') {
                let close = format!("\"{}", "#".repeat(hashes));
                if let Some(idx) = after_q.rfind(&close) {
                    return Some(after_q[..idx].to_string());
                }
            }
        }
    }
    // generic double-quoted
    if let Some(start) = t.find('"') {
        if let Some(end_rel) = t[start + 1..].rfind('"') {
            let inner = &t[start + 1..start + 1 + end_rel];
            return Some(unescape_simple(inner));
        }
    }
    Some(t.to_string())
}

fn unescape_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('0') => out.push('\0'),
                Some(ch) => out.push(ch),
                None => break,
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub struct Acc {
    pub constants: Vec<Constant>,
    pub calls_map: HashMap<String, (u32, (u32, u32))>,
    pub call_sites: Vec<CallSite>,
    /// Parameter-name → 0-based index in the enclosing function's signature.
    /// Populated once at `analyze_function` entry; used by `lower_arg_expr`
    /// to recognise parameter forwarding at call sites.
    pub params: HashMap<String, u32>,
    pub types_used: HashSet<String>,
    pub operators: HashMap<String, u32>,
    pub operands: HashMap<String, u32>,
    pub binary_operators: BinaryOperatorSet,
    pub comment_lines: HashSet<u32>,
    pub code_lines: HashSet<u32>,
    pub asm_lines: HashSet<u32>,
    pub inputs: u32,
    pub outputs: u32,
    pub branches: u32,
    pub loops: u32,
    pub max_if: u32,
    pub max_loop: u32,
    pub max_comb: u32,
    pub calls_total: u32,
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub early_returns: u32,
    pub goto_count: u32,
    pub unsafe_blocks: u32,
    pub last_return_pos: Option<u32>,
    pub fn_end_byte: u32,
}

impl Acc {
    fn new(fn_node: Node) -> Self {
        Self {
            constants: Vec::new(),
            calls_map: HashMap::new(),
            call_sites: Vec::new(),
            params: HashMap::new(),
            types_used: HashSet::new(),
            operators: HashMap::new(),
            operands: HashMap::new(),
            binary_operators: BinaryOperatorSet::default(),
            comment_lines: HashSet::new(),
            code_lines: HashSet::new(),
            asm_lines: HashSet::new(),
            inputs: 0,
            outputs: 0,
            branches: 0,
            loops: 0,
            max_if: 0,
            max_loop: 0,
            max_comb: 0,
            calls_total: 0,
            cyclomatic: 1, // base
            cognitive: 0,
            early_returns: 0,
            goto_count: 0,
            unsafe_blocks: 0,
            last_return_pos: None,
            fn_end_byte: fn_node.end_byte() as u32,
        }
    }
}

pub fn analyze_function<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    path: &Path,
) -> Option<FunctionAnalysis> {
    let name = spec.function_name(node, src)?;
    let signature = spec.signature(node, src);
    let original_name = spec.original_name(node, src);
    let enclosing_type = spec.enclosing_type(node, src);
    let attributes = spec.attributes(node, src);

    let mut acc = Acc::new(node);
    acc.inputs = signature.inputs.len() as u32;
    acc.outputs = signature.outputs.len() as u32;
    for (i, p) in signature.inputs.iter().enumerate() {
        // Skip placeholder names ("_", empty) — those would alias every
        // ignored binding in the body and produce false `Param` matches.
        if !p.name.is_empty() && p.name != "_" {
            acc.params.insert(p.name.clone(), i as u32);
        }
    }

    // Walk the body (entire function node).
    let mut walk_ctx = WalkCtx::default();
    walk(spec, node, src, &mut acc, 0, 0, 0, 0, &mut walk_ctx);

    // Finalize: derive per-line tallies.
    let sr = node.start_position().row as u32;
    let er = node.end_position().row as u32;
    let total_lines = er.saturating_sub(sr) + 1;
    let loc_comments = acc.comment_lines.len() as u32;
    // "code" lines: any line containing a non-comment token
    let loc_code = acc.code_lines.difference(&acc.comment_lines).count() as u32;
    let loc_asm = acc.asm_lines.len() as u32;

    // Halstead
    let mut halstead = Halstead::default();
    halstead.n1 = acc.operators.len() as u32;
    halstead.n2 = acc.operands.len() as u32;
    halstead.big_n1 = acc.operators.values().sum::<u32>();
    halstead.big_n2 = acc.operands.values().sum::<u32>();
    halstead.compute();

    let calls: Vec<Call> = {
        let mut v: Vec<_> = acc
            .calls_map
            .iter()
            .map(|(k, (count, span))| Call {
                callee: k.clone(),
                count: *count,
                span: *span,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.callee.cmp(&b.callee)));
        v
    };

    let calls_unique = calls.len() as u32;
    let calls_total = acc.calls_total;

    let mut early_returns = acc.early_returns;
    if let Some(last_return_pos) = acc.last_return_pos {
        if is_trailing_return(src, last_return_pos, acc.fn_end_byte) {
            early_returns = early_returns.saturating_sub(1);
        }
    }

    let metrics = Metrics {
        loc_code,
        loc_comments,
        loc_asm,
        inputs: acc.inputs,
        outputs: acc.outputs,
        branches: acc.branches,
        loops: acc.loops,
        max_loop_nesting: acc.max_loop,
        max_if_nesting: acc.max_if,
        max_combined_nesting: acc.max_comb,
        calls_unique,
        calls_total,
        cyclomatic: acc.cyclomatic,
        cognitive: acc.cognitive,
        halstead,
        early_returns,
        goto_count: acc.goto_count,
        unsafe_blocks: acc.unsafe_blocks,
        binary_operators: acc.binary_operators,
    };

    let types_used: Vec<TypeRef> = {
        let mut v: Vec<_> = acc.types_used.into_iter().map(TypeRef::new).collect();
        v.sort_by(|a, b| a.text.cmp(&b.text));
        v
    };

    let _ = total_lines; // reserved for future density metrics

    Some(FunctionAnalysis {
        name,
        original_name,
        mangled: None,
        enclosing_type,
        location: Location {
            file: path.to_path_buf(),
            line_start: sr + 1,
            line_end: er + 1,
            col_start: node.start_position().column as u32,
            col_end: node.end_position().column as u32,
            byte_start: node.start_byte() as u32,
            byte_end: node.end_byte() as u32,
        },
        signature,
        metrics,
        constants: acc.constants,
        calls,
        call_sites: acc.call_sites,
        types_used,
        attributes,
    })
}

/// Per-walk mutable context. Bundling this saves us from threading a fresh
/// `&mut` parameter through every recursive call each time the IR grows
/// (today: guard stack + switch-value stack; tomorrow possibly loop guards
/// or per-branch fact tables).
#[derive(Default)]
pub struct WalkCtx {
    /// Path-condition guards from every enclosing `if` true-branch, the
    /// negation of every enclosing `if` else-branch, every enclosing
    /// `case` arm, etc. ANDed together at each call site to form the
    /// `CallSite.path_cond`.
    pub guard_stack: Vec<Predicate>,
    /// Switch / match values being compared against. When entering a
    /// `case` arm, the walker pops the top entry to build `Cmp(Eq, value,
    /// case_value)` and pushes that on `guard_stack`. Stack so nested
    /// switches (rare but legal) keep the right value paired with each
    /// case arm.
    pub switch_stack: Vec<Term>,
}

fn walk<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    acc: &mut Acc,
    if_d: u32,
    loop_d: u32,
    comb_d: u32,
    cog_nesting: u32,
    ctx: &mut WalkCtx,
) {
    let class = spec.classify(&node, src);

    // Per-line tagging. Every non-comment token contributes a code line; this
    // intentionally double-counts if a line has both a comment and code, but
    // the final tally removes comment-only lines from code via set difference.
    let sr = node.start_position().row as u32;
    let er = node.end_position().row as u32;

    match class {
        NodeClass::Comment => {
            for r in sr..=er {
                acc.comment_lines.insert(r);
            }
            return;
        }
        NodeClass::AsmBlock => {
            for r in sr..=er {
                acc.asm_lines.insert(r);
                acc.code_lines.insert(r);
            }
            // still recurse to pick up calls/constants inside
        }
        _ => {
            // Treat only leaf-ish nodes as contributing a line. Using every
            // node would flood; we only mark for identifier/literal/keyword/op.
            match class {
                NodeClass::Identifier
                | NodeClass::IntLit
                | NodeClass::FloatLit
                | NodeClass::StrLit
                | NodeClass::CharLit
                | NodeClass::BoolLit(_)
                | NodeClass::Operator
                | NodeClass::Keyword
                | NodeClass::TryPropagate
                | NodeClass::Return
                | NodeClass::Goto
                | NodeClass::Call => {
                    for r in sr..=er {
                        acc.code_lines.insert(r);
                    }
                }
                _ => {}
            }
        }
    }

    let mut new_if = if_d;
    let mut new_loop = loop_d;
    let mut new_comb = comb_d;
    let mut new_cog = cog_nesting;

    match class {
        NodeClass::If => {
            // `else if` must not bump nesting depth. Grammars model it
            // differently:
            //   - C/C++ wraps the chained if inside an `else_clause` node.
            //   - Java and many others put the nested `if_statement` directly
            //     as the `alternative` field of the parent if.
            //   - Rust does the same via `if_expression`.
            // Treat all three as a continuation, not a new level.
            let is_else_if = match node.parent() {
                Some(p) => {
                    matches!(p.kind(), "else_clause" | "else")
                        || (matches!(
                            p.kind(),
                            "if_statement" | "if_expression" | "if_let_expression"
                        ) && p
                            .child_by_field_name("alternative")
                            .map(|alt| alt.id() == node.id())
                            .unwrap_or(false))
                }
                None => false,
            };
            acc.branches += 1;
            acc.cyclomatic += 1;
            if is_else_if {
                acc.cognitive += 1;
            } else {
                new_if = if_d + 1;
                new_comb = comb_d + 1;
                acc.max_if = acc.max_if.max(new_if);
                acc.max_comb = acc.max_comb.max(new_comb);
                acc.cognitive += 1 + cog_nesting;
                new_cog = cog_nesting + 1;
            }
        }
        NodeClass::Else => {
            acc.cognitive += 1;
        }
        NodeClass::Loop => {
            acc.loops += 1;
            acc.cyclomatic += 1;
            new_loop = loop_d + 1;
            new_comb = comb_d + 1;
            acc.max_loop = acc.max_loop.max(new_loop);
            acc.max_comb = acc.max_comb.max(new_comb);
            acc.cognitive += 1 + cog_nesting;
            new_cog = cog_nesting + 1;
        }
        NodeClass::SwitchCase => {
            acc.branches += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1;
        }
        NodeClass::ShortCircuit => {
            acc.branches += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1;
            record_binary_operator(node, src, &mut acc.binary_operators);
        }
        NodeClass::Ternary => {
            acc.branches += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1 + cog_nesting;
        }
        NodeClass::TryPropagate => {
            acc.branches += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1 + cog_nesting;
            acc.early_returns += 1;
            *acc.operators.entry("?".to_string()).or_insert(0) += 1;
        }
        NodeClass::Return => {
            acc.last_return_pos = Some(node.end_byte() as u32);
            // increment: every return except the final trailing one is "early"
            // We count provisionally and correct at the end.
            acc.early_returns += 1;
        }
        NodeClass::Goto => {
            acc.goto_count += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1;
        }
        NodeClass::UnsafeBlock => {
            acc.unsafe_blocks += 1;
        }
        NodeClass::Call => {
            acc.calls_total += 1;
            if let Some(callee) = spec.call_callee(node, src) {
                let span = (node.start_byte() as u32, node.end_byte() as u32);
                let ent = acc.calls_map.entry(callee.clone()).or_insert((0, span));
                ent.0 += 1;
                let arg_nodes = spec.call_args(node, src);
                let args: Vec<ArgExpr> = arg_nodes
                    .into_iter()
                    .map(|a| lower_arg_expr(spec, a, src, &acc.params))
                    .collect();
                let path_cond = match ctx.guard_stack.len() {
                    0 => None,
                    1 => Some(ctx.guard_stack[0].clone().canonicalize()),
                    // Build the conjunction and let canonicalize flatten /
                    // sort / dedup, so two translations with the same set
                    // of guards in different nesting order produce the
                    // same shape.
                    _ => Some(
                        Predicate::And {
                            items: ctx.guard_stack.clone(),
                        }
                        .canonicalize(),
                    ),
                };
                acc.call_sites.push(CallSite {
                    callee,
                    span,
                    args,
                    in_loop: loop_d > 0,
                    path_cond,
                });
            }
        }
        NodeClass::IntLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_int(text) {
                    acc.constants.push(Constant::Int {
                        value: v,
                        text: text.to_string(),
                        span: (node.start_byte() as u32, node.end_byte() as u32),
                    });
                }
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::FloatLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_float(text) {
                    acc.constants.push(Constant::Float {
                        value: v,
                        text: text.to_string(),
                        span: (node.start_byte() as u32, node.end_byte() as u32),
                    });
                }
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::StrLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_string(text) {
                    acc.constants.push(Constant::String {
                        value: v,
                        span: (node.start_byte() as u32, node.end_byte() as u32),
                    });
                }
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::CharLit => {
            if let Ok(text) = node.utf8_text(src) {
                let v = text.trim_matches('\'').to_string();
                acc.constants.push(Constant::Char {
                    value: v,
                    span: (node.start_byte() as u32, node.end_byte() as u32),
                });
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::BoolLit(v) => {
            if let Ok(text) = node.utf8_text(src) {
                acc.constants.push(Constant::Bool {
                    value: v,
                    span: (node.start_byte() as u32, node.end_byte() as u32),
                });
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::Identifier => {
            if let Ok(text) = node.utf8_text(src) {
                *acc.operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
        NodeClass::Operator | NodeClass::Keyword => {
            if let Some(t) = spec.operator_text(node, src) {
                *acc.operators.entry(t).or_insert(0) += 1;
            }
            record_binary_operator(node, src, &mut acc.binary_operators);
        }
        _ => {}
    }

    // Custom subtree walks for nodes whose children are guarded differently.
    // All three forms — `if`, `?:` ternary, and `switch`/`match` — diverge
    // from the generic child-loop pattern; everything else falls through.
    //
    // Field names (`condition`, `consequence`, `alternative`, `value`) are
    // stable across tree-sitter-c, tree-sitter-cpp, and tree-sitter-rust —
    // we rely on that uniformity rather than adding language-specific
    // dispatch methods whose default impl would just look up the same
    // fields.
    //
    // `else if` chains "just work" without special-casing: the chained `if`
    // shows up as a child of `else_clause` (or as the parent's
    // `alternative` field), so the outer condition's negation is already
    // on the guard stack when the inner condition gets pushed.
    if matches!(class, NodeClass::If | NodeClass::Ternary) {
        walk_if_or_ternary(
            spec, node, src, acc, new_if, new_loop, new_comb, new_cog, ctx,
        );
        return;
    }

    // Switch / match: stash the value being switched on so case arms can
    // build per-case predicates. Push on entry, pop on exit; nested
    // switches stay paired correctly because of the stack discipline.
    if matches!(node.kind(), "switch_statement" | "match_expression") {
        let switch_term = node
            .child_by_field_name("condition")
            .or_else(|| node.child_by_field_name("value"))
            .map(|n| lower_term(spec, n, src, &acc.params));
        let pushed_switch = switch_term.is_some();
        if let Some(t) = switch_term {
            ctx.switch_stack.push(t);
        }
        // Generic child recursion below handles the body and case arms;
        // SwitchCase entries pull the top switch term to build their
        // per-case predicate. We don't `return` here — the regular child
        // loop is what we want.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if spec.classify(&child, src) == NodeClass::Function {
                continue;
            }
            walk(
                spec, child, src, acc, new_if, new_loop, new_comb, new_cog, ctx,
            );
        }
        if pushed_switch {
            ctx.switch_stack.pop();
        }
        return;
    }

    // Per-case-arm guard. When entering a `case_statement` (C) or
    // `match_arm` (Rust), build the predicate `switch_value == case_value`
    // (or Opaque for `default` / non-literal patterns) and push it for
    // the duration of the case body's recursion.
    let case_pred = if class == NodeClass::SwitchCase {
        Some(case_predicate(spec, node, src, &acc.params, ctx))
    } else {
        None
    };
    if let Some(p) = &case_pred {
        ctx.guard_stack.push(p.clone());
    }

    // Recurse over children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Don't recurse into nested function definitions.
        if spec.classify(&child, src) == NodeClass::Function {
            continue;
        }
        walk(
            spec, child, src, acc, new_if, new_loop, new_comb, new_cog, ctx,
        );
    }

    if case_pred.is_some() {
        ctx.guard_stack.pop();
    }
}

/// Custom-walk an `if` or ternary `?:` node: condition with the unchanged
/// stack, consequence with the predicate pushed, alternative with the
/// negation pushed. Both forms expose the same field names in tree-sitter
/// grammars we care about.
fn walk_if_or_ternary<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    acc: &mut Acc,
    if_d: u32,
    loop_d: u32,
    comb_d: u32,
    cog_nesting: u32,
    ctx: &mut WalkCtx,
) {
    let cond_node = node.child_by_field_name("condition");
    let pred = cond_node
        .map(|c| lower_predicate(spec, c, src, &acc.params).canonicalize())
        .unwrap_or(Predicate::Opaque {
            text: String::new(),
        });

    // Walk the condition itself with the *current* stack — the call we care
    // about hasn't been guarded by `pred` yet.
    if let Some(c) = cond_node {
        walk(spec, c, src, acc, if_d, loop_d, comb_d, cog_nesting, ctx);
    }

    if let Some(c) = node.child_by_field_name("consequence") {
        ctx.guard_stack.push(pred.clone());
        walk(spec, c, src, acc, if_d, loop_d, comb_d, cog_nesting, ctx);
        ctx.guard_stack.pop();
    }

    if let Some(a) = node.child_by_field_name("alternative") {
        let neg = Predicate::Not {
            item: Box::new(pred),
        }
        .canonicalize();
        ctx.guard_stack.push(neg);
        walk(spec, a, src, acc, if_d, loop_d, comb_d, cog_nesting, ctx);
        ctx.guard_stack.pop();
    }
}

/// Build the path-condition predicate for a case arm. Pulls the switched-on
/// term from the top of the switch stack and pairs it with the case's
/// `value` (C) or `pattern` (Rust). Returns `Opaque` when:
///
/// - There's no switch on the stack (shouldn't happen in well-formed code,
///   but defensive)
/// - The case has no `value`/`pattern` field (i.e. C `default:`) — we
///   *could* compute the negation of all sibling values, but that requires
///   walking back up which complicates the stack discipline; for MVP we
///   admit defeat and let `default:` arms be uncheckable.
/// - The pattern doesn't lower to a clean term (Rust ranges, OR-patterns,
///   struct destructuring, etc.) — `==` against a complex pattern is
///   misleading, so we go opaque rather than guess.
fn case_predicate<S: LanguageSpec>(
    spec: &S,
    case_node: Node,
    src: &[u8],
    params: &HashMap<String, u32>,
    ctx: &WalkCtx,
) -> Predicate {
    let Some(switch_term) = ctx.switch_stack.last() else {
        return Predicate::Opaque {
            text: case_node.utf8_text(src).unwrap_or("").trim().to_string(),
        };
    };
    // Check `pattern` (Rust `match_arm`) first: Rust's `match_arm` *also*
    // has a `value` field — the body after `=>` — and grabbing that would
    // turn the case predicate into a comparison against the body's text.
    // C's `case_statement` only has `value`, so the fallback covers it.
    let value_node = case_node
        .child_by_field_name("pattern")
        .or_else(|| case_node.child_by_field_name("value"));
    let Some(v) = value_node else {
        // C `default:` lands here. Mark uncheckable.
        return Predicate::Opaque {
            text: "<default>".to_string(),
        };
    };
    let case_term = lower_term(spec, v, src, params);
    if let Term::Opaque { text } = case_term {
        // Pattern wasn't a literal/identifier we recognise (Rust range
        // pattern, OR-pattern, struct pattern, …). Use a textual form
        // so the comparator at least sees that *something* gates the call,
        // but treats it as uncheckable for PathCondDrift.
        return Predicate::Opaque { text };
    }
    Predicate::Cmp {
        op: CmpOp::Eq,
        left: switch_term.clone(),
        right: case_term,
    }
    .canonicalize()
}

/// Compatibility hook retained for language analyzers that call it after
/// walking a report. Early-return finalization now happens in
/// `analyze_function`, where the source slice is still available and we can
/// distinguish a truly trailing final return from an early return inside a
/// `match`/`if` arm.
pub fn finalize_early_returns(analyses: &mut [FunctionAnalysis]) {
    let _ = analyses;
}

fn is_trailing_return(src: &[u8], return_end: u32, fn_end: u32) -> bool {
    let start = return_end as usize;
    let end = (fn_end as usize).min(src.len());
    if start >= end {
        return true;
    }
    src[start..end]
        .iter()
        .all(|b| b.is_ascii_whitespace() || matches!(*b, b';' | b'}' | b')'))
}

/// Tally a node's operator into the binary-operator-set fingerprint. Reads the
/// node's `operator` field (the symbol token) — a convention shared by the
/// `binary_expression` / `unary_expression` / `boolean_operator` nodes across
/// the tree-sitter grammars we target — and classifies it by symbol.
///
/// Prefix uses are distinguished by node kind so a unary `*` (deref), `&`
/// (address-of), `-` (negation) or `+` are *not* miscounted as binary
/// multiply / bitwise-and / subtract; only `!` and `~` are recorded from a
/// prefix context. Nodes with no `operator` field (e.g. Python's keyword-only
/// `not_operator`, plain assignments) contribute nothing.
fn record_binary_operator(node: Node, src: &[u8], set: &mut BinaryOperatorSet) {
    let kind = node.kind();
    let is_unary = kind.contains("unary")
        || matches!(
            kind,
            "pointer_expression"
                | "reference_expression"
                | "not_operator"
                | "preinc_expression"
                | "update_expression"
        );
    // Prefer the named `operator` field (C/C++/Java binary + unary, Rust
    // binary, Python operators). tree-sitter-rust models a *prefix* operator
    // as an anonymous leading token with no field, and Python's `not_operator`
    // is a bare `not` keyword — for those unary cases fall back to the first
    // child token.
    let op_node = node
        .child_by_field_name("operator")
        .or_else(|| if is_unary { node.child(0) } else { None });
    let Some(op_node) = op_node else {
        return;
    };
    let Ok(sym) = op_node.utf8_text(src) else {
        return;
    };
    set.record(sym.trim(), is_unary);
}

/// Lower a single argument node into an `ArgExpr`. Shallow on purpose: only
/// the four shapes we can reason about cleanly are recognised — parameter
/// forwarding, kinded literals, immediately-nested calls — and everything
/// else falls into `Opaque(text)` rather than risking a false equivalence.
/// Languages opt in by implementing `LanguageSpec::call_args`; the rest get
/// empty `args` and downstream comparators treat them as uncheckable.
pub fn lower_arg_expr<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    params: &HashMap<String, u32>,
) -> ArgExpr {
    let class = spec.classify(&node, src);
    let span = (node.start_byte() as u32, node.end_byte() as u32);
    match class {
        NodeClass::Identifier => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(idx) = params.get(text) {
                    return ArgExpr::Param { index: *idx };
                }
            }
        }
        NodeClass::IntLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_int(text) {
                    return ArgExpr::Const {
                        value: Constant::Int {
                            value: v,
                            text: text.to_string(),
                            span,
                        },
                    };
                }
            }
        }
        NodeClass::FloatLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_float(text) {
                    return ArgExpr::Const {
                        value: Constant::Float {
                            value: v,
                            text: text.to_string(),
                            span,
                        },
                    };
                }
            }
        }
        NodeClass::StrLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_string(text) {
                    return ArgExpr::Const {
                        value: Constant::String { value: v, span },
                    };
                }
            }
        }
        NodeClass::CharLit => {
            if let Ok(text) = node.utf8_text(src) {
                let v = text.trim_matches('\'').to_string();
                return ArgExpr::Const {
                    value: Constant::Char { value: v, span },
                };
            }
        }
        NodeClass::BoolLit(v) => {
            return ArgExpr::Const {
                value: Constant::Bool { value: v, span },
            };
        }
        NodeClass::Call => {
            if let Some(callee) = spec.call_callee(node, src) {
                return ArgExpr::NestedCall { callee };
            }
        }
        _ => {}
    }
    ArgExpr::Opaque {
        text: node.utf8_text(src).unwrap_or("").trim().to_string(),
    }
}

/// Lower a `Term` from an expression node — same shallow recognition as
/// `lower_arg_expr` but emitting the predicate-side `Term` enum. Locals
/// and unrecognised shapes go to `Opaque`; the caller treats `Opaque`
/// terms as a signal to mark the surrounding predicate uncheckable.
fn lower_term<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    params: &HashMap<String, u32>,
) -> Term {
    // Strip wrappers so the case-arm pattern lowering gets through to the
    // literal underneath. The set is small and grammar-stable:
    //
    // - `parenthesized_expression`  (C / Rust)
    // - `match_pattern` / `literal_pattern`  (Rust `match_arm` patterns —
    //   the literal `1` in `1 => ...` is wrapped one or two layers deep
    //   depending on tree-sitter-rust version)
    if matches!(
        node.kind(),
        "parenthesized_expression" | "match_pattern" | "literal_pattern"
    ) {
        if let Some(inner) = node.named_child(0) {
            return lower_term(spec, inner, src, params);
        }
    }
    let class = spec.classify(&node, src);
    let span = (node.start_byte() as u32, node.end_byte() as u32);
    match class {
        NodeClass::Identifier => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(idx) = params.get(text) {
                    return Term::Param { index: *idx };
                }
            }
        }
        NodeClass::IntLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_int(text) {
                    return Term::Const {
                        value: Constant::Int {
                            value: v,
                            text: text.to_string(),
                            span,
                        },
                    };
                }
            }
        }
        NodeClass::FloatLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_float(text) {
                    return Term::Const {
                        value: Constant::Float {
                            value: v,
                            text: text.to_string(),
                            span,
                        },
                    };
                }
            }
        }
        NodeClass::BoolLit(v) => {
            return Term::Const {
                value: Constant::Bool { value: v, span },
            };
        }
        NodeClass::StrLit => {
            if let Ok(text) = node.utf8_text(src) {
                if let Some(v) = spec.parse_string(text) {
                    return Term::Const {
                        value: Constant::String { value: v, span },
                    };
                }
            }
        }
        _ => {}
    }
    Term::Opaque {
        text: node.utf8_text(src).unwrap_or("").trim().to_string(),
    }
}

/// Lower an `if` (or boolean) condition node into a `Predicate`. Recognises:
///
/// - Comparisons: `binary_expression` whose `operator` field is one of
///   `<`, `<=`, `==`, `!=`, `>`, `>=`. Operands lowered via `lower_term`.
/// - Boolean conjunctions / disjunctions: `binary_expression` with `&&` /
///   `||`, recurses on both sides.
/// - Boolean negation: `unary_expression` with `!` / `not`, recurses on
///   the inner.
/// - Parenthesised expressions: descends through `parenthesized_expression`
///   transparently.
/// - Bool literals: `true` / `false`.
/// - A bare identifier or expression: `Truthy(term)` — preserves the term
///   without making any claim about its truth value.
///
/// Anything else falls into `Opaque(text)`, which the comparator treats as
/// "uncheckable" and skips rather than flagging.
fn lower_predicate<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    params: &HashMap<String, u32>,
) -> Predicate {
    // Descend through trivial wrappers. Both tree-sitter-c and
    // tree-sitter-rust use `parenthesized_expression`; if anyone adds
    // another it should be safe to add here.
    if matches!(node.kind(), "parenthesized_expression") {
        if let Some(inner) = node.named_child(0) {
            return lower_predicate(spec, inner, src, params);
        }
    }

    let class = spec.classify(&node, src);
    match class {
        NodeClass::BoolLit(true) => return Predicate::True,
        NodeClass::BoolLit(false) => return Predicate::False,
        _ => {}
    }

    // Boolean unary `!cond` — both grammars expose the inner via the
    // `argument` field on `unary_expression`. The classifier reports
    // unary as `Operator`, so we don't gate on class — we just check the
    // node kind.
    if matches!(node.kind(), "unary_expression") {
        if let Some(op) = node.child_by_field_name("operator") {
            let op_kind = op.kind();
            if op_kind == "!" {
                if let Some(inner) = node.child_by_field_name("argument") {
                    return Predicate::Not {
                        item: Box::new(lower_predicate(spec, inner, src, params)),
                    };
                }
            }
        }
    }

    // Binary expression: comparison or short-circuit boolean.
    if matches!(node.kind(), "binary_expression") {
        let op = node.child_by_field_name("operator");
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        if let (Some(op), Some(l), Some(r)) = (op, left, right) {
            let op_kind = op.kind();
            if let Some(cmp) = CmpOp::from_str(op_kind) {
                return Predicate::Cmp {
                    op: cmp,
                    left: lower_term(spec, l, src, params),
                    right: lower_term(spec, r, src, params),
                };
            }
            if op_kind == "&&" {
                return Predicate::And {
                    items: vec![
                        lower_predicate(spec, l, src, params),
                        lower_predicate(spec, r, src, params),
                    ],
                };
            }
            if op_kind == "||" {
                return Predicate::Or {
                    items: vec![
                        lower_predicate(spec, l, src, params),
                        lower_predicate(spec, r, src, params),
                    ],
                };
            }
        }
    }

    // Bare term used as a boolean (`if (flag)` / `if x { ... }`).
    let term = lower_term(spec, node, src, params);
    if matches!(term, Term::Opaque { .. }) {
        Predicate::Opaque {
            text: node.utf8_text(src).unwrap_or("").trim().to_string(),
        }
    } else {
        Predicate::Truthy { term }
    }
}

/// Utility for implementors: find the first named descendant with a given
/// kind and return its text.
pub fn find_kind_text<'a>(node: Node<'a>, kind: &str, src: &'a [u8]) -> Option<String> {
    if node.kind() == kind {
        return node.utf8_text(src).ok().map(|s| s.to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(t) = find_kind_text(child, kind, src) {
            return Some(t);
        }
    }
    None
}

/// Utility: walk an AST top-down and collect all nodes whose classifier reports
/// `NodeClass::Function`.
pub fn collect_functions<'a, S: LanguageSpec>(
    spec: &S,
    root: Node<'a>,
    src: &[u8],
    out: &mut Vec<Node<'a>>,
) {
    if spec.classify(&root, src) == NodeClass::Function {
        out.push(root);
        // Continue so we find nested fns (e.g. Rust inner fns).
    }
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        collect_functions(spec, child, src, out);
    }
}

/// Walk the AST top-down and collect every struct-like node that the spec
/// reports as such, producing a `StructAnalysis` per node. Matching uses
/// `LanguageSpec::struct_kind` so a language can choose which grammar nodes
/// count (e.g. `struct_item`, `class_declaration`, `derived_type_definition`).
pub fn collect_structs<S: LanguageSpec>(
    spec: &S,
    root: Node,
    src: &[u8],
    path: &Path,
    out: &mut Vec<StructAnalysis>,
) {
    walk_structs(spec, root, src, path, out);
}

fn walk_structs<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    path: &Path,
    out: &mut Vec<StructAnalysis>,
) {
    if let Some(kind) = spec.struct_kind(&node, src) {
        if let Some(sa) = analyze_struct(spec, node, src, path, kind) {
            out.push(sa);
        }
        // Recurse: nested structs inside classes (Java inner classes, Rust
        // nested structs) should still be picked up.
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_structs(spec, child, src, path, out);
    }
}

fn analyze_struct<S: LanguageSpec>(
    spec: &S,
    node: Node,
    src: &[u8],
    path: &Path,
    kind: &'static str,
) -> Option<StructAnalysis> {
    let name = spec.struct_name(node, src)?;
    let raw_fields = spec.struct_fields(node, src);
    let fields: Vec<StructField> = raw_fields
        .into_iter()
        .map(|(name, ty)| {
            let category = classify_type(&ty);
            StructField {
                name,
                ty: TypeRef::new(ty),
                category,
            }
        })
        .collect();
    let metrics = StructMetrics::from_fields(&fields);
    let attributes = spec.struct_attributes(node, src);
    let sr = node.start_position().row as u32;
    let er = node.end_position().row as u32;
    Some(StructAnalysis {
        name,
        kind: kind.to_string(),
        location: Location {
            file: path.to_path_buf(),
            line_start: sr + 1,
            line_end: er + 1,
            col_start: node.start_position().column as u32,
            col_end: node.end_position().column as u32,
            byte_start: node.start_byte() as u32,
            byte_end: node.end_byte() as u32,
        },
        fields,
        metrics,
        attributes,
    })
}

#[cfg(test)]
mod parse_int_tests {
    use super::parse_int_default;

    #[test]
    fn decimal_round_trip() {
        assert_eq!(parse_int_default("0"), Some(0));
        assert_eq!(parse_int_default("1"), Some(1));
        assert_eq!(parse_int_default("255"), Some(255));
        assert_eq!(parse_int_default("1024"), Some(1024));
        assert_eq!(parse_int_default("-7"), Some(-7));
    }

    #[test]
    fn hex_no_longer_collapses_to_zero() {
        // The original bug: every hex literal returned Some(0) because the
        // suffix-stripping pass ate `x` and the hex digits.
        assert_eq!(parse_int_default("0xFF"), Some(255));
        assert_eq!(parse_int_default("0xff"), Some(255));
        assert_eq!(parse_int_default("0x0F"), Some(15));
        assert_eq!(parse_int_default("0xFFFF"), Some(0xFFFF));
        assert_eq!(parse_int_default("0xffffffff"), Some(0xFFFFFFFFi64));
        assert_eq!(parse_int_default("0XABCDEF"), Some(0xABCDEF));
        assert_eq!(parse_int_default("0x40"), Some(64));
    }

    #[test]
    fn binary_and_octal() {
        assert_eq!(parse_int_default("0b1010"), Some(10));
        assert_eq!(parse_int_default("0B11"), Some(3));
        assert_eq!(parse_int_default("0o17"), Some(15));
        assert_eq!(parse_int_default("0O20"), Some(16));
    }

    #[test]
    fn rust_type_suffixes_stripped() {
        assert_eq!(parse_int_default("1024usize"), Some(1024));
        assert_eq!(parse_int_default("0xFFu32"), Some(255));
        assert_eq!(parse_int_default("0x40u8"), Some(64));
        assert_eq!(parse_int_default("100i64"), Some(100));
        assert_eq!(parse_int_default("0b101_010u16"), Some(0b101010));
    }

    #[test]
    fn c_type_suffixes_stripped() {
        assert_eq!(parse_int_default("0xFFFFFFFFu"), Some(0xFFFFFFFFi64));
        assert_eq!(parse_int_default("0xFFULL"), Some(255));
        assert_eq!(parse_int_default("100L"), Some(100));
        assert_eq!(parse_int_default("100UL"), Some(100));
        assert_eq!(parse_int_default("0x1LL"), Some(1));
    }

    #[test]
    fn separators_stripped() {
        assert_eq!(parse_int_default("1_000_000"), Some(1_000_000));
        assert_eq!(parse_int_default("0xFF_FF"), Some(0xFFFF));
        // C++14 single-quote digit separator
        assert_eq!(parse_int_default("100'000"), Some(100_000));
    }

    #[test]
    fn negative_hex() {
        assert_eq!(parse_int_default("-0xFF"), Some(-255));
    }

    #[test]
    fn signed_overflow_wraps_into_i64() {
        // 0x8000000000000000 is i64::MIN as a u64; we round-trip through u64
        // and reinterpret, so this should not return None.
        assert_eq!(parse_int_default("0x8000000000000000"), Some(i64::MIN));
    }

    #[test]
    fn rejects_pure_garbage() {
        assert_eq!(parse_int_default("xyz"), None);
        assert_eq!(parse_int_default("0x"), None);
    }
}
