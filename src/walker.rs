//! Generic metrics walker. Each language crate implements `LanguageSpec` to
//! classify tree-sitter nodes; the walker handles nesting, accumulation and
//! final metric computation uniformly across languages.

use crate::core::{Call, Constant, FunctionAnalysis, Halstead, Location, Metrics, Signature, TypeRef};
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

    /// Extract parameter list + return type for a Function node.
    fn signature(&self, node: Node, src: &[u8]) -> Signature;

    /// Parse an Int literal's numeric value from its textual representation.
    fn parse_int(&self, text: &str) -> Option<i64> {
        parse_int_default(text)
    }

    /// Parse a Float literal.
    fn parse_float(&self, text: &str) -> Option<f64> {
        text.trim_end_matches(|c: char| c == 'f' || c == 'F' || c == 'L' || c == 'l' || c == 'd' || c == 'D')
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
    let digits: String = body[..end].chars().filter(|c| *c != '_' && *c != '\'').collect();
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
    if let Some(rest) = t.strip_prefix('r').or_else(|| t.strip_prefix('b').filter(|_| t.starts_with("br")).map(|_| &t[2..])) {
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
    pub types_used: HashSet<String>,
    pub operators: HashMap<String, u32>,
    pub operands: HashMap<String, u32>,
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
            types_used: HashSet::new(),
            operators: HashMap::new(),
            operands: HashMap::new(),
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
    let attributes = spec.attributes(node, src);

    let mut acc = Acc::new(node);
    acc.inputs = signature.inputs.len() as u32;
    acc.outputs = signature.outputs.len() as u32;

    // Walk the body (entire function node).
    walk(spec, node, src, &mut acc, 0, 0, 0, 0);

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
        early_returns: acc.early_returns,
        goto_count: acc.goto_count,
        unsafe_blocks: acc.unsafe_blocks,
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
        types_used,
        attributes,
    })
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
        }
        NodeClass::Ternary => {
            acc.branches += 1;
            acc.cyclomatic += 1;
            acc.cognitive += 1 + cog_nesting;
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
                let ent = acc.calls_map.entry(callee).or_insert((0, span));
                ent.0 += 1;
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
        }
        _ => {}
    }

    // Recurse over children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Don't recurse into nested function definitions.
        if spec.classify(&child, src) == NodeClass::Function {
            continue;
        }
        walk(spec, child, src, acc, new_if, new_loop, new_comb, new_cog);
    }
}

/// Post-process: the last `return` in a function is not an "early return".
pub fn finalize_early_returns(analyses: &mut [FunctionAnalysis]) {
    for fa in analyses.iter_mut() {
        if fa.metrics.early_returns > 0 {
            fa.metrics.early_returns = fa.metrics.early_returns.saturating_sub(1);
        }
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
        assert_eq!(
            parse_int_default("0x8000000000000000"),
            Some(i64::MIN)
        );
    }

    #[test]
    fn rejects_pure_garbage() {
        assert_eq!(parse_int_default("xyz"), None);
        assert_eq!(parse_int_default("0x"), None);
    }
}
