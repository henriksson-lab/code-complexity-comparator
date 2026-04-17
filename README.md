# Code Complexity Comparator

Static complexity analysis and cross-language comparison for C/C++, Rust, and (future) Java, R, Python. Built to catch bugs and incomplete translations between an original codebase and its Rust port.

## Design: `complexity-cmp`

### Workspace layout (cargo workspace)

```
complexity-core/      shared types, JSON schema, versioning
complexity-analyzer/  LanguageAnalyzer trait + registry
complexity-lang-c/    C/C++ (tree-sitter-c, tree-sitter-cpp)
complexity-lang-rust/ Rust (syn OR tree-sitter-rust)
complexity-lang-java/ future
complexity-lang-py/   future
complexity-lang-r/    future
complexity-compare/   matching, deviation, diffing
complexity-predict/   linear + heuristic model
complexity-cli/       binary
```

One crate per language keeps the matrix open: adding Java means implementing one trait.

### Parsing

Use **tree-sitter** as the uniform backbone — same visitor shape across languages, Rust bindings, robust to partial code. Keep **libclang** as an optional backend for C/C++ when macro-accurate results are needed (feature-gated). Rust-side, `syn` gives better type info than tree-sitter for the Rust analyzer specifically.

### Core data model (in `complexity-core`)

```rust
struct Report {
    schema_version: u32,
    language: Language,
    source_file: PathBuf,
    source_hash: String,       // so compare can warn on staleness
    functions: Vec<FunctionAnalysis>,
}

struct FunctionAnalysis {
    name: String,
    original_name: Option<String>,   // Rust: from #[link_name]/#[no_mangle]/mapping file
    mangled: Option<String>,
    location: Location,              // file, byte range, line range, col
    signature: Signature,            // inputs: Vec<Param>, outputs: Vec<TypeRef>
    metrics: Metrics,
    constants: Vec<Constant>,        // with kind + textual form + source span
    calls: Vec<Call>,                // callee name + count + span
    types_used: Vec<TypeRef>,        // locals, fields touched, generics
    attributes: BTreeMap<String, String>, // free-form extension bag per language
}

struct Metrics {
    loc_code: u32,
    loc_comments: u32,
    loc_asm: u32,           // inline asm blocks / asm! macros
    inputs: u32,
    outputs: u32,           // tuple/out-params flattened
    branches: u32,          // if/else-if/match arms/ternary/&&/||
    loops: u32,
    max_loop_nesting: u32,
    max_if_nesting: u32,
    max_combined_nesting: u32,
    calls_unique: u32,
    calls_total: u32,
    // extras worth adding:
    cyclomatic: u32,
    cognitive: u32,         // Sonar-style; penalizes nesting
    halstead: Halstead,     // n1,n2,N1,N2 → volume, difficulty
    early_returns: u32,
    goto_count: u32,        // C only; signals translation risk
    unsafe_blocks: u32,     // Rust only
}
```

The `attributes` bag lets each language stash language-specific flags (e.g. `virtual`, `template`, `async`) without bloating the core struct.

### LanguageAnalyzer trait

```rust
trait LanguageAnalyzer {
    fn language(&self) -> Language;
    fn extensions(&self) -> &[&str];
    fn analyze_file(&self, path: &Path) -> Result<Report>;
    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report>;
}
```

Each implementation is essentially a tree-sitter visitor that emits `FunctionAnalysis` per function node. Shared helpers in `complexity-core` handle nesting stacks, constant literal parsing, comment/code line counting from byte ranges.

### Function matching (Rust ↔ other)

Separate from analysis, in `complexity-compare`. Strategies tried in order:

1. **Explicit mapping file** (YAML/TOML): `{ rust: "parse_header", c: "ph_parse" }`.
2. **FFI attribute**: `#[no_mangle]` / `#[link_name]` → use `original_name` directly.
3. **Extern block declarations**: for Rust callers, the extern block lists the C symbols; names match 1:1.
4. **Name normalization**: snake/camel-fold, strip `_impl`, `_inner`, module prefixes.
5. **Signature + metric fingerprint**: arity, return kind, LOC bucket — break ties.

Output always records which strategy matched, so the user can audit.

### CLI surface

```
complexity analyze <path> [-l <lang>] [-o report.json] [--recurse]
complexity compare <rust.json> <other.json> [--mapping map.yaml]
                   [--sort deviation|name] [--top N] [--format table|json]
complexity missing <rust.json> <other.json>            # in other, not in rust
complexity sort    <report.json> [--by cognitive|cyclomatic|combined-nesting|loc|composite]
complexity constants-diff <rust.json> <other.json>     # grouped by function, by kind
complexity predict train --pairs dir/  --model model.json
complexity predict apply --model model.json --source other.json [--against rust.json]
```

`--format json` everywhere so results chain into CI / dashboards.

### Deviation score (for `compare --sort deviation`)

Per matched pair, compute a weighted normalized difference:

```
dev = Σ_i w_i * |m_rust_i - m_other_i| / max(1, scale_i)
```

where `scale_i` is the 95th-percentile of that metric across the file (so a 10-line function with 2 loops isn't dwarfed by a 500-line one), and weights default to `{cyclomatic: 2, cognitive: 2, combined_nesting: 2, calls_total: 1, loc: 1, constants: 1.5}`. Weights configurable via TOML. Sort desc and show top N with a side-by-side metric table — these are the functions most likely to be mistranslated.

### `missing` command

Set difference over matched-name keys; also flag **partial matches**: function exists but metric dev is above a threshold, "looks like a stub" (e.g. LOC < 20% of original). Partial matches often signal incomplete translation more than absent functions.

### `sort` suggestion (single file)

Useful sort keys:
- `cognitive` — best single "is this hard to read" signal
- `cyclomatic` — classic
- `combined-nesting * loc` — surfaces deeply-nested mid-sized functions that humans struggle with
- `halstead.difficulty` — catches expression-heavy code without much control flow
- `composite` — default: z-score sum of cognitive, combined-nesting, calls_total, constants_count

### Constants diff

Group per matched function. For each constant kind (int/float/string/char/bool), compare multisets:

- Exact equality → OK.
- Same kind, different value → potential translation bug (magic number drift).
- Missing on one side → highlight (often indicates a branch was dropped or an error message lost).
- Integer radix differences collapsed (`0xFF` vs `255` are equal).

Output: per function, a three-column diff (rust-only, both, other-only) sorted by function deviation.

### Prediction model

`complexity-predict` trains **one linear model per target metric** (cyclomatic_rust ~ f(C features), cognitive_rust ~ …, etc.), using matched pairs from an existing translated codebase as training data.

Features for each function pair:
- All source-language metrics
- LOC bucket (log-binned) and goto count
- Counts of: `switch` cases, macro expansions flagged, pointer-heavy signatures, inline asm
- Indicator: function is `static` / has `extern "C"`

```rust
struct Model {
    per_metric: BTreeMap<MetricName, LinearFit>,  // coeffs + intercept + residual std
    heuristics: Vec<HeuristicRule>,
}

struct LinearFit { coefs: Vec<f64>, intercept: f64, feature_order: Vec<String>, rmse: f64 }
```

Heuristic adjustments applied after the linear step (ordered, composable):

- C `switch` over small int → Rust `match`: branches stay ~equal, cyclomatic drops by ~1 (default arm).
- C `goto cleanup` pattern → Rust `?`: early-returns += N, branches -= N.
- C macro with embedded control flow → Rust inline code: expect LOC inflation on Rust side.
- C `malloc/free` pairs → Rust drop: calls_total -= 2k, unsafe_blocks likely 0.

The model outputs **predicted Rust metrics + residual std** per function. When a real Rust file is supplied via `--against`, flag functions where `(actual - predicted) / residual_std > 2.5` — these are statistical outliers: the translation did something unexpectedly divergent. This complements raw deviation, which doesn't know which differences are *normal* for this codebase.

Train with OLS (use `linfa` or `nalgebra` + closed-form) — keeps the model interpretable and the coefficients inspectable as JSON.

### Extensibility for Java / R / Python

The only per-language work is:
1. A tree-sitter grammar dependency.
2. An implementation of `LanguageAnalyzer` (~one file, mostly a visitor).
3. Optional language-specific attributes in the `attributes` bag.
4. Optional heuristic rules in the prediction model.

Compare/sort/diff/predict commands work unchanged because they consume only the shared JSON schema.

### A few things worth calling out

- **Version the JSON schema** (`schema_version` field) — you'll change it.
- **Store source ranges, not just line numbers** — lets you re-open in editor / build rich HTML reports later.
- **Record each constant's textual form alongside its parsed value** — `0xFF` vs `255` is sometimes the thing you want to see in the diff.
- **Don't try to resolve `#include`/macros for the C analyzer v1.** Analyze at the token/AST level only; add libclang later as an opt-in backend when the macro blindness becomes the limiting factor.
- Matching by name is brittle across large refactors — the mapping file escape hatch is essential; design for it from day one, not retrofitted.
