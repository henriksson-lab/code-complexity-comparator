use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    C,
    Cpp,
    Rust,
    Java,
    Python,
    R,
    Perl,
    Fortran,
    Unknown,
}

impl Language {
    pub fn from_ext(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "rs" => Language::Rust,
            "java" => Language::Java,
            "py" => Language::Python,
            "r" => Language::R,
            "pl" | "pm" | "t" => Language::Perl,
            "f" | "f90" | "f95" | "f03" | "f08" | "for" | "ftn" => Language::Fortran,
            _ => Language::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::Python => "python",
            Language::R => "r",
            Language::Perl => "perl",
            Language::Fortran => "fortran",
            Language::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub language: Language,
    pub source_file: PathBuf,
    pub source_hash: String,
    pub functions: Vec<FunctionAnalysis>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<StructAnalysis>,
}

impl Report {
    pub fn new(language: Language, source_file: PathBuf, source_hash: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language,
            source_file,
            source_hash,
            functions: Vec::new(),
            structs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionAnalysis {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangled: Option<String>,
    /// Enclosing class / impl-target / struct. For Rust, the type the
    /// containing `impl` block is written against (trait name is ignored —
    /// `impl Display for Strand` yields `Strand`). For Python, the
    /// containing `class` name. `None` for free functions, nested closures,
    /// or languages that don't model methods this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing_type: Option<String>,
    pub location: Location,
    pub signature: Signature,
    pub metrics: Metrics,
    pub constants: Vec<Constant>,
    pub calls: Vec<Call>,
    /// Per-call-site detail. Each `Call` aggregates by callee name; this list
    /// keeps one entry per actual call expression in the body, with argument
    /// expressions lowered into `ArgExpr` for cross-translation comparison.
    /// Empty on `schema_version == 1` reports loaded from disk; populated by
    /// the walker on every fresh analysis (within the limits of each
    /// language's `LanguageSpec::call_args` coverage).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSite>,
    pub types_used: Vec<TypeRef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub file: PathBuf,
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Signature {
    pub inputs: Vec<Param>,
    pub outputs: Vec<TypeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub text: String,
}

impl TypeRef {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub loc_code: u32,
    pub loc_comments: u32,
    pub loc_asm: u32,
    pub inputs: u32,
    pub outputs: u32,
    pub branches: u32,
    pub loops: u32,
    pub max_loop_nesting: u32,
    pub max_if_nesting: u32,
    pub max_combined_nesting: u32,
    pub calls_unique: u32,
    pub calls_total: u32,
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub halstead: Halstead,
    pub early_returns: u32,
    pub goto_count: u32,
    pub unsafe_blocks: u32,
    /// Per-operator occurrence counts for the arithmetic / shift / bitwise /
    /// logical binary operators — see [`BinaryOperatorSet`]. A compact
    /// language-neutral fingerprint of the *kind* of work a function does
    /// (arithmetic-heavy vs. bit-twiddling vs. boolean-logic-heavy), used as
    /// an extra family of similarity dimensions by the deviation comparator.
    /// `#[serde(default)]` so pre-v4 reports on disk still load.
    #[serde(default)]
    pub binary_operators: BinaryOperatorSet,
}

/// Occurrence counts for the "binary operator set": the arithmetic operators
/// (`+ - * / %`), the shifts (`<< >>`), the bitwise operators (`& | ^ ~`),
/// and the logical operators (`&& || !`). The two prefix operators `~` and
/// `!` are included even though they are syntactically unary — they round out
/// the bitwise / logical families the user cares about.
///
/// Counts are by *symbol*, taken from each binary/unary expression node's
/// `operator` field. This means syntactic dual-use symbols are bucketed by
/// the form they take: a unary deref `*p` or address-of `&x` is **not**
/// counted (only `~` / `!` are recorded from unary contexts), while a binary
/// `a & b` lands in `bit_and` regardless of whether the language treats it as
/// bitwise (C/Rust) or element-wise-logical (R). Best-effort and
/// language-neutral, consistent with the rest of the walker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryOperatorSet {
    pub add: u32,       // +
    pub sub: u32,       // -
    pub mul: u32,       // *
    pub div: u32,       // /
    pub rem: u32,       // %
    pub shl: u32,       // <<
    pub shr: u32,       // >>
    pub bit_and: u32,   // &
    pub bit_or: u32,    // |
    pub bit_xor: u32,   // ^
    pub bit_not: u32,   // ~  (unary)
    pub logic_and: u32, // && / and / .and.
    pub logic_or: u32,  // || / or / .or.
    pub logic_not: u32, // ! / not / .not.  (unary)
}

impl BinaryOperatorSet {
    /// Tally one operator occurrence by its symbol. `is_unary` distinguishes a
    /// prefix use (deref/negation/address-of/logical-not/bitwise-not) from a
    /// binary use, so that `*` / `&` / `-` / `+` in prefix position are
    /// ignored rather than miscounted as multiply / bitwise-and / subtract.
    /// Word forms (`and`/`or`/`not`) and Fortran's dotted forms (`.and.` etc.)
    /// are accepted so the symbolic-grammar assumption degrades gracefully.
    pub fn record(&mut self, sym: &str, is_unary: bool) {
        if is_unary {
            match sym {
                "!" | "not" | ".not." => self.logic_not += 1,
                "~" => self.bit_not += 1,
                _ => {}
            }
            return;
        }
        match sym {
            "+" => self.add += 1,
            "-" => self.sub += 1,
            "*" => self.mul += 1,
            "/" => self.div += 1,
            "%" => self.rem += 1,
            "<<" => self.shl += 1,
            ">>" => self.shr += 1,
            "&" => self.bit_and += 1,
            "|" => self.bit_or += 1,
            "^" => self.bit_xor += 1,
            "&&" | "and" | ".and." => self.logic_and += 1,
            "||" | "or" | ".or." => self.logic_or += 1,
            "!" | "not" | ".not." => self.logic_not += 1,
            "~" => self.bit_not += 1,
            _ => {}
        }
    }

    /// Labelled counts in a stable order. Keys are prefixed `op_` so they sit
    /// in their own namespace when folded into the comparator's metric vector.
    pub fn counts(&self) -> [(&'static str, u32); 14] {
        [
            ("op_add", self.add),
            ("op_sub", self.sub),
            ("op_mul", self.mul),
            ("op_div", self.div),
            ("op_rem", self.rem),
            ("op_shl", self.shl),
            ("op_shr", self.shr),
            ("op_bit_and", self.bit_and),
            ("op_bit_or", self.bit_or),
            ("op_bit_xor", self.bit_xor),
            ("op_bit_not", self.bit_not),
            ("op_logic_and", self.logic_and),
            ("op_logic_or", self.logic_or),
            ("op_logic_not", self.logic_not),
        ]
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Halstead {
    pub n1: u32,
    pub n2: u32,
    pub big_n1: u32,
    pub big_n2: u32,
    pub volume: f64,
    pub difficulty: f64,
}

impl Halstead {
    pub fn compute(&mut self) {
        let n = (self.n1 + self.n2) as f64;
        let big_n = (self.big_n1 + self.big_n2) as f64;
        if n > 0.0 {
            self.volume = big_n * n.log2();
        }
        if self.n2 > 0 {
            self.difficulty = (self.n1 as f64 / 2.0) * (self.big_n2 as f64 / self.n2 as f64);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Constant {
    Int {
        value: i64,
        text: String,
        span: (u32, u32),
    },
    Float {
        value: f64,
        text: String,
        span: (u32, u32),
    },
    String {
        value: String,
        span: (u32, u32),
    },
    Char {
        value: String,
        span: (u32, u32),
    },
    Bool {
        value: bool,
        span: (u32, u32),
    },
}

impl Constant {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Constant::Int { .. } => "int",
            Constant::Float { .. } => "float",
            Constant::String { .. } => "string",
            Constant::Char { .. } => "char",
            Constant::Bool { .. } => "bool",
        }
    }

    pub fn equivalent_to(&self, other: &Constant) -> bool {
        match (self, other) {
            (Constant::Int { value: a, .. }, Constant::Int { value: b, .. }) => a == b,
            (Constant::Float { value: a, .. }, Constant::Float { value: b, .. }) => {
                (a - b).abs() < 1e-12
            }
            (Constant::String { value: a, .. }, Constant::String { value: b, .. }) => a == b,
            (Constant::Char { value: a, .. }, Constant::Char { value: b, .. }) => a == b,
            (Constant::Bool { value: a, .. }, Constant::Bool { value: b, .. }) => a == b,
            _ => false,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Constant::Int { value, text, .. } => format!("{} ({})", value, text),
            Constant::Float { value, text, .. } => format!("{} ({})", value, text),
            Constant::String { value, .. } => format!("\"{}\"", value.escape_debug()),
            Constant::Char { value, .. } => format!("'{}'", value),
            Constant::Bool { value, .. } => format!("{}", value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub callee: String,
    pub count: u32,
    pub span: (u32, u32),
}

/// One physical call expression in a function body. Aggregated `Call`
/// records group these by `callee`, but cross-translation analyses
/// (forwarding skew, const drift, path conditions) need the per-site detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    pub callee: String,
    pub span: (u32, u32),
    pub args: Vec<ArgExpr>,
    /// `true` iff at least one enclosing loop (any kind) wraps this call.
    /// Cheap signal that helps explain `count > 1` aggregations and flags
    /// translation cases where one side moved a call into / out of a loop.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_loop: bool,
    /// Conjoined predicates from every enclosing `if` true-branch (and the
    /// negation of `if` conditions whose else-branch contains this call),
    /// up to the function root. `None` when the call is unconditional in
    /// the function body, or when the language analyzer hasn't lowered a
    /// useful predicate (the path falls back to `Opaque` rather than
    /// emitting a misleading `True`).
    ///
    /// Walker only tracks `if`/`else` directly enclosing the call site —
    /// switch arms, loop conditions, ternaries, and dataflow-induced
    /// implications are intentionally **not** modelled. This is high-recall
    /// flagging, not a verifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_cond: Option<Predicate>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Argument expression at a call site, lowered into a small enum so we can
/// reason about parameter forwarding (`Param`), literal arguments (`Const`),
/// and immediately-nested calls (`NestedCall`) across translations. Anything
/// not recognised falls into `Opaque` with the original textual form — this
/// is intentional: we'd rather flag "uncheckable" than over-claim equivalence.
///
/// Future extensions (Local, Field, Index, Ref) slot in here without breaking
/// the wire format because serde's `tag = "kind"` lets readers ignore unknown
/// variants only when both sides are upgraded; new variants thus require a
/// schema bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ArgExpr {
    /// The N-th formal parameter of the enclosing function, forwarded as-is.
    Param { index: u32 },
    /// A literal: int / float / string / char / bool. Reuses `Constant` so
    /// the existing kinded-equivalence helper applies unchanged.
    Const { value: Constant },
    /// A call expression nested directly in argument position: `f(g(x))`.
    /// Only the immediate callee name is captured — deeper structure goes to
    /// `Opaque` to keep the IR shallow.
    NestedCall { callee: String },
    /// Anything else (binary expressions, field accesses, locals, casts,
    /// macro invocations, …). Stores the original textual form so a
    /// downstream comparator can still do best-effort textual diffs.
    Opaque { text: String },
}

/// Comparison operator. `flip()` swaps operands (`a < b` ≡ `b > a`),
/// `negate()` swaps the truth value (`!(a < b)` ≡ `a >= b`). Used for
/// canonicalising path conditions before structural comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn negate(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
        }
    }

    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }

    pub fn from_str(s: &str) -> Option<CmpOp> {
        match s {
            "<" => Some(CmpOp::Lt),
            "<=" => Some(CmpOp::Le),
            "==" => Some(CmpOp::Eq),
            "!=" => Some(CmpOp::Ne),
            ">" => Some(CmpOp::Gt),
            ">=" => Some(CmpOp::Ge),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// Atomic term in a predicate: a parameter reference, a literal, a field
/// access, or an opaque textual fallback. Same shape as `ArgExpr` minus
/// `NestedCall` (function calls inside if-conditions are uncommon and we'd
/// rather be conservative than guess at their truth value).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Term {
    Param { index: u32 },
    Const { value: Constant },
    Field { base: Box<Term>, name: String },
    Opaque { text: String },
}

impl Term {
    /// Cheap structural ordering used to canonicalise comparisons —
    /// `Const < Param < Field < Opaque`. The canonicaliser always puts
    /// the lower-ranked side on the left and flips the operator
    /// accordingly, so `x < 0` and `0 > x` both collapse to the same
    /// structural form (`0 > x`). Choice of which side is "left" is
    /// arbitrary; what matters is that two equivalent comparisons land
    /// on the same shape.
    fn order_rank(&self) -> u8 {
        match self {
            Term::Const { .. } => 0,
            Term::Param { .. } => 1,
            Term::Field { .. } => 2,
            Term::Opaque { .. } => 3,
        }
    }

    pub fn equivalent_to(&self, other: &Term) -> bool {
        match (self, other) {
            (Term::Param { index: a }, Term::Param { index: b }) => a == b,
            (Term::Const { value: a }, Term::Const { value: b }) => a.equivalent_to(b),
            (Term::Field { base: ab, name: an }, Term::Field { base: bb, name: bn }) => {
                an == bn && ab.equivalent_to(bb)
            }
            (Term::Opaque { text: a }, Term::Opaque { text: b }) => a == b,
            _ => false,
        }
    }
}

/// Path-condition predicate. Boolean structure is shallow: `And`/`Or`/`Not`
/// over `Cmp`s and `Truthy`s. No constraint solving — we compare
/// structurally after a short canonicalisation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Predicate {
    Cmp {
        op: CmpOp,
        left: Term,
        right: Term,
    },
    And {
        items: Vec<Predicate>,
    },
    Or {
        items: Vec<Predicate>,
    },
    Not {
        item: Box<Predicate>,
    },
    /// `if (x)` where `x` is a non-comparison expression — we record the
    /// term but make no claim about its truth value.
    Truthy {
        term: Term,
    },
    True,
    False,
    /// Anything we couldn't lower into the structured form. Comparisons
    /// involving an `Opaque` predicate are reported as "uncheckable" rather
    /// than as a `PathCondDrift` finding.
    Opaque {
        text: String,
    },
}

impl Predicate {
    /// Apply the local canonicalisations that turn `0 > x` and `x < 0` into
    /// the same structural form. Runs once on each predicate at lowering
    /// time; arg-flow comparison then uses `equivalent_to`.
    ///
    /// Specifically:
    /// - `Cmp` with the simpler side on the right flips to put it on the
    ///   left (then `Const < Param < Field < Opaque`).
    /// - `Not(Cmp)` collapses into `Cmp` with the negated operator.
    /// - `Not(Not(p))` collapses to `p`.
    /// - `And`/`Or` are flattened (nested `And` inside `And` lifts its
    ///   children up), deduplicated by `equivalent_to`, and sorted by a
    ///   stable structural key. This lets two translations that captured
    ///   the same set of guards in *different nesting order* collide
    ///   structurally — e.g. `if (a) if (b) f()` vs `if (b) if (a) f()`.
    /// - Trivial folds: `And([])` → `True`, `And([x])` → `x`, `Or([])` →
    ///   `False`, `Or([x])` → `x`. Same for boolean identities under Not.
    pub fn canonicalize(self) -> Predicate {
        match self {
            Predicate::Cmp { op, left, right } => {
                if right.order_rank() < left.order_rank() {
                    Predicate::Cmp {
                        op: op.flip(),
                        left: right,
                        right: left,
                    }
                } else {
                    Predicate::Cmp { op, left, right }
                }
            }
            Predicate::Not { item } => match *item {
                Predicate::Cmp { op, left, right } => Predicate::Cmp {
                    op: op.negate(),
                    left,
                    right,
                }
                .canonicalize(),
                Predicate::Not { item: inner } => inner.canonicalize(),
                Predicate::True => Predicate::False,
                Predicate::False => Predicate::True,
                other => Predicate::Not {
                    item: Box::new(other.canonicalize()),
                },
            },
            Predicate::And { items } => canonicalize_conjunction(items, /* and = */ true),
            Predicate::Or { items } => canonicalize_conjunction(items, /* and = */ false),
            other => other,
        }
    }

    /// Stable structural key used to sort children of `And`/`Or`. Built from
    /// the canonical text form so two equivalent predicates produce the
    /// same key. Cheap; only called from canonicalisation.
    fn sort_key(&self) -> String {
        format_predicate_for_sort(self)
    }

    /// Structural equivalence after canonicalisation. Float comparisons in
    /// `Term::Const` use `Constant::equivalent_to`'s tolerance. Two
    /// predicates with any `Opaque` leaf are *not* considered equivalent —
    /// the comparator reports such pairs as uncheckable rather than risking
    /// a false-equal from textual coincidence.
    pub fn equivalent_to(&self, other: &Predicate) -> bool {
        match (self, other) {
            (
                Predicate::Cmp {
                    op: a_op,
                    left: al,
                    right: ar,
                },
                Predicate::Cmp {
                    op: b_op,
                    left: bl,
                    right: br,
                },
            ) => a_op == b_op && al.equivalent_to(bl) && ar.equivalent_to(br),
            (Predicate::And { items: a }, Predicate::And { items: b })
            | (Predicate::Or { items: a }, Predicate::Or { items: b }) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equivalent_to(y))
            }
            (Predicate::Not { item: a }, Predicate::Not { item: b }) => a.equivalent_to(b),
            (Predicate::Truthy { term: a }, Predicate::Truthy { term: b }) => a.equivalent_to(b),
            (Predicate::True, Predicate::True) | (Predicate::False, Predicate::False) => true,
            // Opaque is intentionally never equivalent — even to itself with
            // the same text — because callers treat presence of any Opaque
            // as a signal to skip the finding ("uncheckable"). Two textually
            // identical opaque predicates can still hide semantic
            // differences (different variable bindings, side effects, etc.).
            (Predicate::Opaque { .. }, _) | (_, Predicate::Opaque { .. }) => false,
            _ => false,
        }
    }

    /// Walk the tree and return `true` if any leaf is `Opaque`. Comparators
    /// use this to mark a pair as uncheckable rather than producing a noisy
    /// "predicates don't match" finding.
    pub fn has_opaque(&self) -> bool {
        match self {
            Predicate::Opaque { .. } => true,
            Predicate::Cmp { left, right, .. } => left.has_opaque() || right.has_opaque(),
            Predicate::Truthy { term } => term.has_opaque(),
            Predicate::And { items } | Predicate::Or { items } => {
                items.iter().any(|p| p.has_opaque())
            }
            Predicate::Not { item } => item.has_opaque(),
            Predicate::True | Predicate::False => false,
        }
    }
}

impl Term {
    pub fn has_opaque(&self) -> bool {
        match self {
            Term::Opaque { .. } => true,
            Term::Field { base, .. } => base.has_opaque(),
            Term::Param { .. } | Term::Const { .. } => false,
        }
    }
}

fn canonicalize_conjunction(items: Vec<Predicate>, is_and: bool) -> Predicate {
    // Recurse, then flatten same-kind children up one level. `And(a, And(b, c))`
    // becomes `And(a, b, c)` — important because two translations may build
    // the same conjunction with different nesting (one introduces a helper if,
    // the other inlines it).
    let mut flat: Vec<Predicate> = Vec::with_capacity(items.len());
    for p in items {
        match p.canonicalize() {
            Predicate::And { items: inner } if is_and => flat.extend(inner),
            Predicate::Or { items: inner } if !is_and => flat.extend(inner),
            other => flat.push(other),
        }
    }

    // Dedup by structural equivalence. O(n²) but n is the path-condition
    // depth — typically < 5 — so cheaper than going through a HashSet.
    let mut deduped: Vec<Predicate> = Vec::with_capacity(flat.len());
    for p in flat {
        if !deduped.iter().any(|q: &Predicate| q.equivalent_to(&p)) {
            // We dedupe even when one side is `Opaque` (which `equivalent_to`
            // reports as non-equal) — fall back to a textual check so two
            // identical opaque children at least don't both clutter the
            // canonical form.
            if let Predicate::Opaque { text: pt } = &p {
                if deduped
                    .iter()
                    .any(|q| matches!(q, Predicate::Opaque { text } if text == pt))
                {
                    continue;
                }
            }
            deduped.push(p);
        }
    }

    // Sort for canonical order — same set of guards in different orders
    // should produce the same form. Stable key by `format_predicate_for_sort`.
    deduped.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    // Trivial folds.
    if is_and {
        // Drop redundant `True` conjuncts; an explicit `False` short-circuits.
        deduped.retain(|p| !matches!(p, Predicate::True));
        if deduped.iter().any(|p| matches!(p, Predicate::False)) {
            return Predicate::False;
        }
        match deduped.len() {
            0 => Predicate::True,
            1 => deduped.into_iter().next().unwrap(),
            _ => Predicate::And { items: deduped },
        }
    } else {
        deduped.retain(|p| !matches!(p, Predicate::False));
        if deduped.iter().any(|p| matches!(p, Predicate::True)) {
            return Predicate::True;
        }
        match deduped.len() {
            0 => Predicate::False,
            1 => deduped.into_iter().next().unwrap(),
            _ => Predicate::Or { items: deduped },
        }
    }
}

fn format_predicate_for_sort(p: &Predicate) -> String {
    match p {
        Predicate::Cmp { op, left, right } => format!(
            "cmp({},{},{})",
            op.as_str(),
            format_term_for_sort(left),
            format_term_for_sort(right)
        ),
        Predicate::And { items } => format!(
            "and({})",
            items
                .iter()
                .map(format_predicate_for_sort)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Predicate::Or { items } => format!(
            "or({})",
            items
                .iter()
                .map(format_predicate_for_sort)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Predicate::Not { item } => format!("not({})", format_predicate_for_sort(item)),
        Predicate::Truthy { term } => format!("truthy({})", format_term_for_sort(term)),
        Predicate::True => "true".to_string(),
        Predicate::False => "false".to_string(),
        Predicate::Opaque { text } => format!("opaque({})", text),
    }
}

fn format_term_for_sort(t: &Term) -> String {
    match t {
        Term::Param { index } => format!("p{}", index),
        Term::Const { value } => format!("c({})", value.display()),
        Term::Field { base, name } => format!("{}.{}", format_term_for_sort(base), name),
        Term::Opaque { text } => format!("o({})", text),
    }
}

/// A struct / class / record declaration. Fields are modelled as typed
/// variables so that cross-language comparisons can count them by type
/// category the same way functions compare by metric counts.
///
/// The `kind` string records the underlying construct: `"struct"`, `"class"`,
/// `"union"`, `"record"`, `"derived_type"`. Matching is name-based across
/// kinds, which lets a Rust `struct` pair with a C `struct` or a Python
/// `class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructAnalysis {
    pub name: String,
    pub kind: String,
    pub location: Location,
    pub fields: Vec<StructField>,
    pub metrics: StructMetrics,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub ty: TypeRef,
    pub category: TypeCategory,
}

/// Language-neutral bucket for a field type. `Other` catches user-defined
/// types that aren't classifiable from the textual type alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeCategory {
    Int,
    Float,
    Bool,
    Char,
    String,
    Pointer,
    Array,
    Collection,
    Other,
}

impl TypeCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            TypeCategory::Int => "int",
            TypeCategory::Float => "float",
            TypeCategory::Bool => "bool",
            TypeCategory::Char => "char",
            TypeCategory::String => "string",
            TypeCategory::Pointer => "pointer",
            TypeCategory::Array => "array",
            TypeCategory::Collection => "collection",
            TypeCategory::Other => "other",
        }
    }

    pub fn all() -> &'static [TypeCategory] {
        &[
            TypeCategory::Int,
            TypeCategory::Float,
            TypeCategory::Bool,
            TypeCategory::Char,
            TypeCategory::String,
            TypeCategory::Pointer,
            TypeCategory::Array,
            TypeCategory::Collection,
            TypeCategory::Other,
        ]
    }
}

/// Per-struct metrics mirroring the shape of `Metrics` for functions. The
/// per-category counts are the primary comparison features — two structs
/// that hold the same numeric counts across int/float/pointer/… are shaped
/// the same even if the declared type names diverge across languages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructMetrics {
    pub field_count: u32,
    pub int_count: u32,
    pub float_count: u32,
    pub bool_count: u32,
    pub char_count: u32,
    pub string_count: u32,
    pub pointer_count: u32,
    pub array_count: u32,
    pub collection_count: u32,
    pub other_count: u32,
}

impl StructMetrics {
    pub fn from_fields(fields: &[StructField]) -> Self {
        let mut m = StructMetrics::default();
        m.field_count = fields.len() as u32;
        for f in fields {
            match f.category {
                TypeCategory::Int => m.int_count += 1,
                TypeCategory::Float => m.float_count += 1,
                TypeCategory::Bool => m.bool_count += 1,
                TypeCategory::Char => m.char_count += 1,
                TypeCategory::String => m.string_count += 1,
                TypeCategory::Pointer => m.pointer_count += 1,
                TypeCategory::Array => m.array_count += 1,
                TypeCategory::Collection => m.collection_count += 1,
                TypeCategory::Other => m.other_count += 1,
            }
        }
        m
    }
}

/// Heuristic classifier for a textual type. Operates on the trimmed type
/// string — it has no access to a type environment, so this is a best-effort
/// bucketing shared by Rust, C/C++, Java, Python, Fortran. Ordering matters:
/// pointer/reference markers are checked before primitive name matching so
/// that `*const c_char` classifies as a pointer/string, not as `char`.
pub fn classify_type(ty: &str) -> TypeCategory {
    let t = ty.trim();
    if t.is_empty() {
        return TypeCategory::Other;
    }
    let lower = t.to_ascii_lowercase();

    // String-ish — check before pointer, since `*const c_char` / `char *`
    // should land in `String` rather than `Pointer`.
    if is_string_type(&lower) {
        return TypeCategory::String;
    }

    // Array: `[T; N]`, `T[N]`, `T[]`, Fortran `dimension(...)`.
    if is_array_type(t) {
        return TypeCategory::Array;
    }

    // Collection: Vec<T>, HashMap<...>, std::vector<T>, List<T>, dict, set
    if is_collection_type(&lower) {
        return TypeCategory::Collection;
    }

    // Pointer / reference
    if is_pointer_type(t, &lower) {
        return TypeCategory::Pointer;
    }

    // Primitives
    if is_bool_type(&lower) {
        return TypeCategory::Bool;
    }
    if is_char_type(&lower) {
        return TypeCategory::Char;
    }
    if is_float_type(&lower) {
        return TypeCategory::Float;
    }
    if is_int_type(&lower) {
        return TypeCategory::Int;
    }
    TypeCategory::Other
}

fn is_string_type(lower: &str) -> bool {
    // Match common string-ish shapes across languages. We require fairly
    // precise tokens to avoid classifying e.g. `string_view_iterator` as
    // string.
    let trimmed = lower.trim_start_matches('&').trim();
    matches!(
        trimmed,
        "string"
            | "str"
            | "&str"
            | "&'static str"
            | "cstr"
            | "cstring"
            | "osstring"
            | "osstr"
            | "pathbuf"
            | "path"
            | "charsequence"
    ) || trimmed.starts_with("string")
        || trimmed.starts_with("std::string")
        || trimmed.ends_with("::string")
        || trimmed.contains("char *")
        || trimmed.contains("char*")
        || trimmed.contains("c_char")
        || trimmed.contains("character(len")
        || trimmed.starts_with("character*")
}

fn is_array_type(t: &str) -> bool {
    let trimmed = t.trim();
    // Rust: `[T; N]` or `[T]`
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return true;
    }
    // C / Java: `T[N]` / `T[]`
    if trimmed.ends_with(']') {
        return true;
    }
    // Fortran: `dimension(...)`
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("dimension(") || lower.contains(", dimension")
}

fn is_collection_type(lower: &str) -> bool {
    // Matches Rust `Vec<T>`, `HashMap<..>`, `BTreeMap`, `HashSet`;
    // C++ `std::vector<..>`, `std::map`, `std::set`, `std::unordered_*`;
    // Java `List<..>`, `ArrayList`, `Map`, `Set`; Python `list`, `dict`,
    // `set`, `tuple`.
    for prefix in [
        "vec<",
        "vec::",
        "hashmap<",
        "btreemap<",
        "hashset<",
        "btreeset<",
        "vecdeque<",
        "linkedlist<",
        "std::vector",
        "std::map",
        "std::set",
        "std::list",
        "std::deque",
        "std::array",
        "std::unordered_map",
        "std::unordered_set",
        "list<",
        "map<",
        "set<",
        "arraylist<",
        "hashtable<",
        "iterable<",
        "collection<",
    ] {
        if lower.starts_with(prefix) || lower.contains(&format!(" {}", prefix)) {
            return true;
        }
    }
    matches!(
        lower.as_ref(),
        "list"
            | "dict"
            | "set"
            | "tuple"
            | "frozenset"
            | "map"
            | "vector"
            | "arraylist"
            | "hashmap"
            | "btreemap"
    )
}

fn is_pointer_type(t: &str, lower: &str) -> bool {
    // C / C++: trailing *, `T *`, `T**`. Rust: raw pointers `*const T` /
    // `*mut T`, references `&T` / `&mut T`, smart pointers `Box<..>`,
    // `Rc<..>`, `Arc<..>`, `&'a T`, `Option<&T>`.
    let trimmed = t.trim();
    if trimmed.contains('*') {
        return true;
    }
    if trimmed.starts_with('&') {
        return true;
    }
    for prefix in [
        "box<", "rc<", "arc<", "refcell<", "cell<", "nonnull<", "weak<",
    ] {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn is_bool_type(lower: &str) -> bool {
    matches!(lower, "bool" | "boolean" | "logical" | "_bool")
}

fn is_char_type(lower: &str) -> bool {
    matches!(
        lower,
        "char" | "c_char" | "u_char" | "wchar_t" | "char16_t" | "char32_t" | "character"
    )
}

fn is_float_type(lower: &str) -> bool {
    matches!(
        lower,
        "f32"
            | "f64"
            | "float"
            | "double"
            | "long double"
            | "real"
            | "real4"
            | "real8"
            | "real16"
            | "float32"
            | "float64"
    ) || lower.starts_with("real(")
        || lower.starts_with("complex(")
}

fn is_int_type(lower: &str) -> bool {
    matches!(
        lower,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "int"
            | "uint"
            | "short"
            | "long"
            | "byte"
            | "signed"
            | "unsigned"
            | "long long"
            | "unsigned int"
            | "unsigned short"
            | "unsigned long"
            | "unsigned long long"
            | "signed int"
            | "signed short"
            | "signed long"
            | "size_t"
            | "ssize_t"
            | "ptrdiff_t"
            | "intptr_t"
            | "uintptr_t"
            | "int8_t"
            | "int16_t"
            | "int32_t"
            | "int64_t"
            | "uint8_t"
            | "uint16_t"
            | "uint32_t"
            | "uint64_t"
            | "c_int"
            | "c_uint"
            | "c_short"
            | "c_ushort"
            | "c_long"
            | "c_ulong"
            | "c_longlong"
            | "c_ulonglong"
            | "c_size_t"
            | "c_ssize_t"
            | "integer"
            | "integer*1"
            | "integer*2"
            | "integer*4"
            | "integer*8"
    ) || lower.starts_with("integer(")
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Msg(String),
}

#[cfg(test)]
mod schema_compat_tests {
    use super::*;

    fn cmp_p(op: CmpOp, idx: u32, k: i64) -> Predicate {
        Predicate::Cmp {
            op,
            left: Term::Param { index: idx },
            right: Term::Const {
                value: Constant::Int {
                    value: k,
                    text: k.to_string(),
                    span: (0, 0),
                },
            },
        }
    }

    #[test]
    fn conjunction_order_independent_after_canonicalize() {
        // `a && b` and `b && a` should canonicalise to the same form, so
        // two translations that build the same set of guards in different
        // nesting order compare equal under `equivalent_to`.
        let p1 = Predicate::And {
            items: vec![cmp_p(CmpOp::Lt, 0, 5), cmp_p(CmpOp::Gt, 1, 0)],
        }
        .canonicalize();
        let p2 = Predicate::And {
            items: vec![cmp_p(CmpOp::Gt, 1, 0), cmp_p(CmpOp::Lt, 0, 5)],
        }
        .canonicalize();
        assert!(
            p1.equivalent_to(&p2),
            "differently-ordered conjunctions should canonicalise equal:\n  p1={:?}\n  p2={:?}",
            p1,
            p2
        );
    }

    #[test]
    fn nested_and_flattens() {
        // `And(a, And(b, c))` → `And(a, b, c)` — important because the
        // path_cond builder always wraps the guard stack in a flat And,
        // but a translation could nest helper ifs and end up with an
        // Or-then-And shape that flattens differently.
        let inner = Predicate::And {
            items: vec![cmp_p(CmpOp::Lt, 0, 5), cmp_p(CmpOp::Gt, 1, 0)],
        };
        let outer = Predicate::And {
            items: vec![cmp_p(CmpOp::Eq, 2, 7), inner],
        }
        .canonicalize();
        match outer {
            Predicate::And { items } => assert_eq!(items.len(), 3),
            other => panic!("expected flat And of 3, got {:?}", other),
        }
    }

    #[test]
    fn duplicate_conjuncts_dedup() {
        // `if (x>0) if (x>0) f()` would push the same guard twice. Rare in
        // hand-written code, but defensive — we shouldn't double-count.
        let p = Predicate::And {
            items: vec![cmp_p(CmpOp::Gt, 0, 0), cmp_p(CmpOp::Gt, 0, 0)],
        }
        .canonicalize();
        // After dedup + single-item collapse, this should reduce to the
        // bare comparison.
        assert!(matches!(p, Predicate::Cmp { .. }));
    }

    #[test]
    fn trivial_folds() {
        let t = Predicate::And { items: vec![] }.canonicalize();
        assert!(matches!(t, Predicate::True));
        let f = Predicate::Or { items: vec![] }.canonicalize();
        assert!(matches!(f, Predicate::False));
        // And with True conjunct drops it.
        let p = Predicate::And {
            items: vec![Predicate::True, cmp_p(CmpOp::Eq, 0, 5)],
        }
        .canonicalize();
        assert!(matches!(p, Predicate::Cmp { .. }));
    }

    #[test]
    fn v1_function_analysis_loads_with_empty_call_sites() {
        // A schema-v1 FunctionAnalysis lacks the `call_sites` field. We rely
        // on `#[serde(default)]` so that on-disk reports from before the
        // schema bump keep deserialising into the v2 type. Regression guard
        // for accidentally dropping the default attribute.
        let json = r#"{
            "name": "old_fn",
            "location": {
                "file": "x.c", "line_start": 1, "line_end": 1,
                "col_start": 0, "col_end": 0,
                "byte_start": 0, "byte_end": 0
            },
            "signature": {"inputs": [], "outputs": []},
            "metrics": {
                "loc_code": 0, "loc_comments": 0, "loc_asm": 0,
                "inputs": 0, "outputs": 0, "branches": 0, "loops": 0,
                "max_loop_nesting": 0, "max_if_nesting": 0,
                "max_combined_nesting": 0, "calls_unique": 0, "calls_total": 0,
                "cyclomatic": 1, "cognitive": 0,
                "halstead": {"n1": 0, "n2": 0, "big_n1": 0, "big_n2": 0,
                             "volume": 0.0, "difficulty": 0.0},
                "early_returns": 0, "goto_count": 0, "unsafe_blocks": 0
            },
            "constants": [],
            "calls": [],
            "types_used": []
        }"#;
        let fa: FunctionAnalysis = serde_json::from_str(json).expect("v1 deser");
        assert!(fa.call_sites.is_empty());
    }
}

pub fn hash_source(src: &str) -> String {
    // Lightweight non-crypto hash so reports can detect staleness without
    // pulling in a crypto dep. Deterministic across runs.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in src.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}
