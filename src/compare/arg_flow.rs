//! Cross-translation argument-flow checks. Builds on top of the function
//! match table to compare *what* each matched pair calls and *how*: argument
//! shapes, constant values, parameter forwarding. Layered tiers degrade
//! gracefully so the analysis still produces output even when reports lack
//! call-site detail or parameter mappings:
//!
//! - **Tier 1** (always runs when both sides have `call_sites`): arity drift,
//!   constant drift on positionally-aligned args, in-loop deltas. No
//!   parameter-mapping inference needed.
//! - **Tier 2** (gated on `infer_parameter_map` returning a map): forwarding
//!   skew — when a Rust parameter is forwarded into a callee on one side but
//!   the *other* parameter is forwarded on the other side. Pairs that fail
//!   inference are listed in `unresolved_pairs` with a reason; the user fixes
//!   them by renaming or adding to `ccc_mapping.toml`.
//!
//! Phase A of mapping inference only — name → type → positional. By design
//! conservative: same-typed-many-arg cases that name/type can't disambiguate
//! are *refused*, not guessed. See memory note "argument-flow analysis scope".

use crate::compare::matching::{
    class_matches, path_suffix_matches, Mapping, MappingEntry, MatchResult, MatchStrategy,
};
use crate::compare::upstream::FunctionRef;
use crate::core::{
    classify_type, ArgExpr, CallSite, FunctionAnalysis, Param, Predicate, Term, TypeCategory,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterMap {
    /// `rust_to_other[i] = Some(j)` means Rust parameter `i` corresponds to
    /// the other side's parameter `j`. `None` means the Rust parameter has
    /// no counterpart (e.g. arity differs and no signal lined them up).
    pub rust_to_other: Vec<Option<u32>>,
    pub source: ParamMapSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamMapSource {
    Explicit,
    Name,
    Type,
    Positional,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoMapReason {
    /// One side has zero parameters while the other has some — nothing to
    /// align. Findings that don't depend on the map (tier 1) still run.
    OneSideEmpty { rust_arity: u32, other_arity: u32 },
    /// Different parameter counts and no name/type signal aligned them.
    ArityMismatch { rust_arity: u32, other_arity: u32 },
    /// Names didn't normalize-match unambiguously.
    AmbiguousNames,
    /// Type vector wasn't distinctive enough — multiple equally-good
    /// alignments exist (e.g. several same-typed `i32` params with no
    /// helpful names).
    AmbiguousTypes,
}

impl NoMapReason {
    pub fn human(&self) -> String {
        match self {
            NoMapReason::OneSideEmpty {
                rust_arity,
                other_arity,
            } => {
                format!(
                    "one side empty (rust={}, other={})",
                    rust_arity, other_arity
                )
            }
            NoMapReason::ArityMismatch {
                rust_arity,
                other_arity,
            } => {
                format!(
                    "arity differs ({} vs {}), no name/type signal",
                    rust_arity, other_arity
                )
            }
            NoMapReason::AmbiguousNames => "ambiguous parameter names".to_string(),
            NoMapReason::AmbiguousTypes => "ambiguous parameter types".to_string(),
        }
    }
}

/// Build an explicit parameter map from a `MappingEntry.params` override.
/// Returns `None` when the entry has no override or when the indices are
/// out of range for either signature (we silently fall through to Phase A
/// in that case rather than producing a corrupt map).
pub fn parameter_map_from_entry(
    entry: &MappingEntry,
    rust: &FunctionAnalysis,
    other: &FunctionAnalysis,
) -> Option<ParameterMap> {
    let pairs = entry.params.as_ref()?;
    let r_arity = rust.signature.inputs.len() as u32;
    let o_arity = other.signature.inputs.len() as u32;
    let mut map: Vec<Option<u32>> = vec![None; rust.signature.inputs.len()];
    for [r, o] in pairs {
        if *r >= r_arity || *o >= o_arity {
            return None;
        }
        map[*r as usize] = Some(*o);
    }
    Some(ParameterMap {
        rust_to_other: map,
        source: ParamMapSource::Explicit,
    })
}

/// Phase A inference. Tries name match first, then type-category match, then
/// positional with type-category corroboration. Returns `Err(reason)` when
/// none of those produce a unique alignment — the caller tags such pairs as
/// unresolved and skips tier-2 checks rather than guessing.
pub fn infer_parameter_map(
    rust: &FunctionAnalysis,
    other: &FunctionAnalysis,
) -> Result<ParameterMap, NoMapReason> {
    let r = &rust.signature.inputs;
    let o = &other.signature.inputs;

    if r.is_empty() && o.is_empty() {
        // Nothing to map — `Positional` is technically vacuous but keeps the
        // downstream gate "do we have a map?" trivially true so tier-2 checks
        // run for arg-less callees.
        return Ok(ParameterMap {
            rust_to_other: Vec::new(),
            source: ParamMapSource::Positional,
        });
    }
    if r.is_empty() || o.is_empty() {
        return Err(NoMapReason::OneSideEmpty {
            rust_arity: r.len() as u32,
            other_arity: o.len() as u32,
        });
    }

    if let Some(map) = try_name_match(r, o) {
        return Ok(ParameterMap {
            rust_to_other: map,
            source: ParamMapSource::Name,
        });
    }

    match try_type_match(r, o) {
        TypeMatchOutcome::Unique(map) => {
            return Ok(ParameterMap {
                rust_to_other: map,
                source: ParamMapSource::Type,
            });
        }
        TypeMatchOutcome::Ambiguous => {
            // Don't fall through to positional: ambiguous types means the
            // signature can't be aligned reliably, period.
            return Err(NoMapReason::AmbiguousTypes);
        }
        TypeMatchOutcome::NoSignal => {}
    }

    if r.len() == o.len() {
        let positional_ok = r
            .iter()
            .zip(o.iter())
            .all(|(rp, op)| classify_type(&rp.ty.text) == classify_type(&op.ty.text));
        if positional_ok {
            return Ok(ParameterMap {
                rust_to_other: (0..r.len() as u32).map(Some).collect(),
                source: ParamMapSource::Positional,
            });
        }
        return Err(NoMapReason::AmbiguousTypes);
    }

    Err(NoMapReason::ArityMismatch {
        rust_arity: r.len() as u32,
        other_arity: o.len() as u32,
    })
}

fn normalize_param_name(s: &str) -> String {
    let mut t = s.trim().to_ascii_lowercase();
    // Strip a couple of common decorations. Keep the list small — overzealous
    // stripping turns `count` and `count_in` into the same key, which is the
    // intended behavior, but `n_buckets` ↔ `buckets` etc would over-match if
    // we stripped every `n_` prefix.
    for prefix in ["p_", "_"] {
        if t.starts_with(prefix) {
            t = t[prefix.len()..].to_string();
            break;
        }
    }
    for suffix in ["_in", "_out", "_p", "_ptr"] {
        if t.ends_with(suffix) {
            t = t[..t.len() - suffix.len()].to_string();
            break;
        }
    }
    t.replace('_', "")
}

fn try_name_match(r: &[Param], o: &[Param]) -> Option<Vec<Option<u32>>> {
    let rn: Vec<String> = r.iter().map(|p| normalize_param_name(&p.name)).collect();
    let on: Vec<String> = o.iter().map(|p| normalize_param_name(&p.name)).collect();

    // Reject if either side has any duplicate names — they'd be inherently
    // ambiguous and we can't tell which-to-which.
    let mut rseen = HashSet::new();
    for n in rn.iter().filter(|n| !n.is_empty()) {
        if !rseen.insert(n) {
            return None;
        }
    }
    let mut oseen = HashSet::new();
    for n in on.iter().filter(|n| !n.is_empty()) {
        if !oseen.insert(n) {
            return None;
        }
    }

    let mut map: Vec<Option<u32>> = vec![None; r.len()];
    let mut matched = 0usize;
    for (i, name) in rn.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        if let Some(j) = on.iter().position(|n| n == name) {
            map[i] = Some(j as u32);
            matched += 1;
        }
    }

    if matched == 0 {
        return None;
    }
    // Require: every rust param with a non-empty name found a counterpart.
    // Anonymous params (rare in modern code) are tolerated as `None`.
    for (i, name) in rn.iter().enumerate() {
        if !name.is_empty() && map[i].is_none() {
            return None;
        }
    }
    Some(map)
}

enum TypeMatchOutcome {
    Unique(Vec<Option<u32>>),
    Ambiguous,
    NoSignal,
}

fn try_type_match(r: &[Param], o: &[Param]) -> TypeMatchOutcome {
    let rt: Vec<TypeCategory> = r.iter().map(|p| classify_type(&p.ty.text)).collect();
    let ot: Vec<TypeCategory> = o.iter().map(|p| classify_type(&p.ty.text)).collect();

    let mut map: Vec<Option<u32>> = vec![None; r.len()];
    let mut used_other = vec![false; o.len()];

    // Iterate-to-fixpoint: pin any rust param that has a single unused
    // counterpart of the same category. New pinnings can unlock further
    // unique matches, hence the loop.
    loop {
        let mut progress = false;
        for (i, ti) in rt.iter().enumerate() {
            if map[i].is_some() {
                continue;
            }
            let candidates: Vec<usize> = ot
                .iter()
                .enumerate()
                .filter(|(j, t)| !used_other[*j] && **t == *ti)
                .map(|(j, _)| j)
                .collect();
            if candidates.len() == 1 {
                map[i] = Some(candidates[0] as u32);
                used_other[candidates[0]] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    let matched = map.iter().filter(|x| x.is_some()).count();

    // After fixpoint: if anything unmapped on rust side still has multiple
    // unused candidates of the same category on other, the alignment is
    // ambiguous. Refuse rather than guess.
    for (i, ti) in rt.iter().enumerate() {
        if map[i].is_some() {
            continue;
        }
        let remaining = ot
            .iter()
            .enumerate()
            .filter(|(j, t)| !used_other[*j] && **t == *ti)
            .count();
        if remaining > 0 {
            return TypeMatchOutcome::Ambiguous;
        }
    }

    if matched == 0 {
        TypeMatchOutcome::NoSignal
    } else {
        TypeMatchOutcome::Unique(map)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingKind {
    /// Aligned call sites pass different numbers of arguments.
    ArityDrift,
    /// Same-position arg is a literal on both sides but the values differ
    /// (e.g. `f(7)` vs `f(8)`).
    ConstDrift,
    /// Both sides forward a parameter, but the indices don't agree under
    /// the enclosing function's parameter map.
    ForwardingSkew,
    /// Two argument positions disagree but the values are swapped — i.e.
    /// rust arg `i` is equivalent to other arg `j` *and* rust arg `j` is
    /// equivalent to other arg `i`. Strong signal that argument order
    /// changed during translation.
    ArgOrderSwap,
    /// One side calls inside a loop, the other doesn't.
    InLoopDelta,
    /// A call site exists on one side with no aligned counterpart on the
    /// other — group sizes for the same callee differ.
    DanglingSiteRust,
    DanglingSiteOther,
    /// Both sides have a path condition, both fully lowered (no Opaque
    /// leaves), but they're not structurally equivalent after substituting
    /// rust parameter indices via the enclosing map. Catches direction
    /// flips like `if (x < 0) f()` vs `if (x >= 0) f()` with the call
    /// moved into the other branch.
    PathCondDrift,
}

impl FindingKind {
    pub fn name(&self) -> &'static str {
        match self {
            FindingKind::ArityDrift => "arity_drift",
            FindingKind::ConstDrift => "const_drift",
            FindingKind::ForwardingSkew => "forwarding_skew",
            FindingKind::ArgOrderSwap => "arg_order_swap",
            FindingKind::InLoopDelta => "in_loop_delta",
            FindingKind::DanglingSiteRust => "dangling_site_rust",
            FindingKind::DanglingSiteOther => "dangling_site_other",
            FindingKind::PathCondDrift => "path_cond_drift",
        }
    }

    fn default_severity(&self) -> f64 {
        match self {
            FindingKind::ConstDrift => 1.0,
            FindingKind::ForwardingSkew => 1.0,
            FindingKind::ArgOrderSwap => 1.0,
            FindingKind::PathCondDrift => 0.9,
            FindingKind::ArityDrift => 0.7,
            FindingKind::DanglingSiteRust | FindingKind::DanglingSiteOther => 0.6,
            FindingKind::InLoopDelta => 0.4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgFlowFinding {
    pub kind: FindingKind,
    pub callee: String,
    /// Byte span of the rust call site, if the finding has one. `(0, 0)` for
    /// findings that only have a counterpart on the other side.
    pub rust_span: (u32, u32),
    pub other_span: (u32, u32),
    pub detail: String,
    pub severity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairArgFlow {
    pub rust: FunctionRef,
    pub other: FunctionRef,
    pub match_strategy: MatchStrategy,
    pub parameter_map: Option<ParameterMap>,
    pub no_map_reason: Option<NoMapReason>,
    pub findings: Vec<ArgFlowFinding>,
    pub score: f64,
    pub call_site_count_rust: u32,
    pub call_site_count_other: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedPair {
    pub rust: FunctionRef,
    pub other: FunctionRef,
    pub reason: NoMapReason,
    pub call_site_count_rust: u32,
    pub call_site_count_other: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgFlowSummary {
    pub matched_pairs: usize,
    pub pairs_with_param_map: usize,
    pub pairs_unresolved: usize,
    pub total_findings: usize,
    pub findings_by_kind: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgFlowAnalysis {
    pub summary: ArgFlowSummary,
    pub pairs: Vec<PairArgFlow>,
    pub unresolved_pairs: Vec<UnresolvedPair>,
}

fn function_ref(f: &FunctionAnalysis) -> FunctionRef {
    FunctionRef {
        name: f.name.clone(),
        file: f.location.file.to_string_lossy().into_owned(),
        line_start: f.location.line_start,
        enclosing_type: f.enclosing_type.clone(),
    }
}

/// Top-level entry: walk every matched pair, infer the enclosing parameter
/// map, then run tier-1 (always) and tier-2 (if mapped) checks on each
/// callee group within the pair.
///
/// `mapping` is the same file used for function matching, optionally
/// supplying explicit `params` overrides per entry. When an override is
/// present *and* matches the pair (by name/path/class with the same
/// suffix-matching rules used by `match_reports`), it wins over Phase A
/// inference and the pair is tagged `ParamMapSource::Explicit`.
pub fn analyze_arg_flow<'a>(
    matched: &MatchResult<'a>,
    mapping: Option<&Mapping>,
) -> ArgFlowAnalysis {
    // A function pair tells us which rust callee names correspond to which
    // other-language callee names. We only have function-level matches,
    // not call-site-level — so this is the bridge from "rust calls foo" to
    // "the other side calls fooʹ".
    let rust_to_other: HashMap<String, String> = matched
        .pairs
        .iter()
        .map(|p| (p.rust.name.clone(), p.other.name.clone()))
        .collect();

    // For each translated callee (keyed by its canonical / other-side name),
    // record the set of (rust_arity, other_arity) tuples seen across all
    // matching pairs. The arity-drift check accepts a call site whose
    // (rust_args.len(), other_args.len()) matches *any* known signature pair
    // — i.e. the call is consistent with some translated overload. This
    // suppresses the systemic per-API delta (e.g. Rust `msv_filter` (3 in)
    // ↔ C `p7_MSVFilter` (5 in) means callsites passing 3 and 5 args
    // respectively are following the documented shape, not drifting). Drift
    // only fires when the callsite arities don't line up with any known
    // translated signature — i.e. *this* call is shaped differently from
    // how the API was ported. Multiple SIMD variants of the same C symbol
    // (sse / neon / vmx all named `p7_MSVFilter`) coexist cleanly here, as
    // do helper variants like `msv_filter_with_scratch`.
    let mut callee_arity: HashMap<String, HashSet<(u32, u32)>> = HashMap::new();
    for p in &matched.pairs {
        let r_arity = p.rust.signature.inputs.len() as u32;
        let o_arity = p.other.signature.inputs.len() as u32;
        callee_arity
            .entry(p.other.name.clone())
            .or_default()
            .insert((r_arity, o_arity));
    }

    let mut pairs_out = Vec::new();
    let mut unresolved = Vec::new();
    let mut findings_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_findings = 0usize;
    let mut pairs_with_param_map = 0usize;

    for p in &matched.pairs {
        // Mapping override takes precedence when both sides of an entry
        // match this pair under the same path/class rules `match_reports`
        // uses. We can't simply trust that the entry that produced this
        // function-pair carries the override — `Pair` doesn't currently
        // carry a back-reference — so we re-look-up here. Same logic, runs
        // once per pair, cheap.
        let override_map = mapping.and_then(|m| {
            m.entries.iter().find_map(|e| {
                let r_match = e.rust == p.rust.name
                    && path_suffix_matches(&p.rust.location.file, e.rust_path.as_deref())
                    && e.rust_line
                        .is_none_or(|ln| p.rust.location.line_start == ln)
                    && class_matches(p.rust.enclosing_type.as_deref(), e.rust_class.as_deref());
                let o_match = e.other == p.other.name
                    && path_suffix_matches(&p.other.location.file, e.other_path.as_deref())
                    && e.other_line
                        .is_none_or(|ln| p.other.location.line_start == ln)
                    && class_matches(p.other.enclosing_type.as_deref(), e.other_class.as_deref());
                if r_match && o_match {
                    parameter_map_from_entry(e, p.rust, p.other)
                } else {
                    None
                }
            })
        });

        let (param_map, no_map_reason) = match override_map {
            Some(m) => {
                pairs_with_param_map += 1;
                (Some(m), None)
            }
            None => match infer_parameter_map(p.rust, p.other) {
                Ok(map) => {
                    pairs_with_param_map += 1;
                    (Some(map), None)
                }
                Err(reason) => (None, Some(reason)),
            },
        };

        // Tier 1 runs unconditionally; tier 2 only if we have a map.
        let mut findings = Vec::new();
        let r_call_count = p.rust.call_sites.len() as u32;
        let o_call_count = p.other.call_sites.len() as u32;

        check_pair_call_sites(
            p.rust,
            p.other,
            param_map.as_ref(),
            &rust_to_other,
            &callee_arity,
            &mut findings,
        );

        let score: f64 = findings.iter().map(|f| f.severity).sum();
        for f in &findings {
            *findings_by_kind
                .entry(f.kind.name().to_string())
                .or_insert(0) += 1;
        }
        total_findings += findings.len();

        if let Some(reason) = &no_map_reason {
            // Mirror the unresolved info as a top-level entry too, so users
            // can read "what to fix" without scanning every pair.
            unresolved.push(UnresolvedPair {
                rust: function_ref(p.rust),
                other: function_ref(p.other),
                reason: reason.clone(),
                call_site_count_rust: r_call_count,
                call_site_count_other: o_call_count,
            });
        }

        pairs_out.push(PairArgFlow {
            rust: function_ref(p.rust),
            other: function_ref(p.other),
            match_strategy: p.strategy,
            parameter_map: param_map,
            no_map_reason,
            findings,
            score,
            call_site_count_rust: r_call_count,
            call_site_count_other: o_call_count,
        });
    }

    pairs_out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rust.name.cmp(&b.rust.name))
    });
    // Push unresolved pairs (with no findings) below scored ones in the
    // top-level list; the caller can also read `unresolved_pairs` directly.
    // The dedicated stderr summary in the CLI uses `unresolved_pairs`.

    // Order unresolved by call-site activity — biggest first, since those
    // are the pairs where adding a mapping entry buys the most coverage.
    unresolved.sort_by(|a, b| {
        let aw = a.call_site_count_rust + a.call_site_count_other;
        let bw = b.call_site_count_rust + b.call_site_count_other;
        bw.cmp(&aw).then(a.rust.name.cmp(&b.rust.name))
    });

    let summary = ArgFlowSummary {
        matched_pairs: matched.pairs.len(),
        pairs_with_param_map,
        pairs_unresolved: unresolved.len(),
        total_findings,
        findings_by_kind,
    };

    ArgFlowAnalysis {
        summary,
        pairs: pairs_out,
        unresolved_pairs: unresolved,
    }
}

fn check_pair_call_sites(
    rust_fn: &FunctionAnalysis,
    other_fn: &FunctionAnalysis,
    enclosing_map: Option<&ParameterMap>,
    rust_to_other_name: &HashMap<String, String>,
    callee_arity: &HashMap<String, HashSet<(u32, u32)>>,
    findings: &mut Vec<ArgFlowFinding>,
) {
    if rust_fn.call_sites.is_empty() && other_fn.call_sites.is_empty() {
        return;
    }

    // Group rust call sites by their *canonical* (other-side) callee name,
    // so a rust callee `foo` paired to other callee `mm_foo` lands in the
    // same bucket as the other side's `mm_foo` calls. Callees with no
    // function-level pair fall through to their own name (works for FFI /
    // libc / std calls that share names across translations).
    let mut rust_groups: HashMap<String, Vec<&CallSite>> = HashMap::new();
    for cs in &rust_fn.call_sites {
        let canonical = rust_to_other_name
            .get(&cs.callee)
            .cloned()
            .unwrap_or_else(|| cs.callee.clone());
        rust_groups.entry(canonical).or_default().push(cs);
    }
    let mut other_groups: HashMap<String, Vec<&CallSite>> = HashMap::new();
    for cs in &other_fn.call_sites {
        other_groups.entry(cs.callee.clone()).or_default().push(cs);
    }

    // Visit every callee that appears on either side; sort for stable output.
    let mut callees: Vec<String> = rust_groups
        .keys()
        .cloned()
        .chain(other_groups.keys().cloned())
        .collect();
    callees.sort_unstable();
    callees.dedup();

    for callee in callees {
        let rs = rust_groups.get(&callee).cloned().unwrap_or_default();
        let os = other_groups.get(&callee).cloned().unwrap_or_default();
        let callee_signature_arities = callee_arity.get(&callee);

        // Lexical-order alignment. Sufficient for MVP — same-callee calls
        // generally appear in similar order across translation. A real
        // alignment cost function (path conditions / arg similarity) is the
        // step-3 job.
        let n = rs.len().min(os.len());
        for i in 0..n {
            check_aligned(
                rs[i],
                os[i],
                enclosing_map,
                callee_signature_arities,
                &callee,
                findings,
            );
        }
        for &dangling in &rs[n..] {
            findings.push(ArgFlowFinding {
                kind: FindingKind::DanglingSiteRust,
                callee: callee.clone(),
                rust_span: dangling.span,
                other_span: (0, 0),
                detail: format!(
                    "{} call sites on rust, {} on other — {} extra rust site(s) unmatched",
                    rs.len(),
                    os.len(),
                    rs.len() - os.len()
                ),
                severity: FindingKind::DanglingSiteRust.default_severity(),
            });
        }
        for &dangling in &os[n..] {
            findings.push(ArgFlowFinding {
                kind: FindingKind::DanglingSiteOther,
                callee: callee.clone(),
                rust_span: (0, 0),
                other_span: dangling.span,
                detail: format!(
                    "{} call sites on rust, {} on other — {} extra other site(s) unmatched",
                    rs.len(),
                    os.len(),
                    os.len() - rs.len()
                ),
                severity: FindingKind::DanglingSiteOther.default_severity(),
            });
        }
    }
}

/// Substitute Rust-side `Param` indices into the other-side parameter space
/// using the enclosing function's parameter map. Returns `None` if any
/// `Param` term references an index that has no counterpart (`map[i] = None`)
/// — the caller treats that as uncheckable rather than emitting a finding
/// against a partially-substituted predicate.
fn remap_term(t: &Term, map: &ParameterMap) -> Option<Term> {
    match t {
        Term::Param { index } => {
            let j = map.rust_to_other.get(*index as usize).copied().flatten()?;
            Some(Term::Param { index: j })
        }
        Term::Const { value } => Some(Term::Const {
            value: value.clone(),
        }),
        Term::Field { base, name } => Some(Term::Field {
            base: Box::new(remap_term(base, map)?),
            name: name.clone(),
        }),
        Term::Opaque { text } => Some(Term::Opaque { text: text.clone() }),
    }
}

fn remap_predicate(p: &Predicate, map: &ParameterMap) -> Option<Predicate> {
    match p {
        Predicate::Cmp { op, left, right } => Some(Predicate::Cmp {
            op: *op,
            left: remap_term(left, map)?,
            right: remap_term(right, map)?,
        }),
        Predicate::And { items } => Some(Predicate::And {
            items: items
                .iter()
                .map(|p| remap_predicate(p, map))
                .collect::<Option<Vec<_>>>()?,
        }),
        Predicate::Or { items } => Some(Predicate::Or {
            items: items
                .iter()
                .map(|p| remap_predicate(p, map))
                .collect::<Option<Vec<_>>>()?,
        }),
        Predicate::Not { item } => Some(Predicate::Not {
            item: Box::new(remap_predicate(item, map)?),
        }),
        Predicate::Truthy { term } => Some(Predicate::Truthy {
            term: remap_term(term, map)?,
        }),
        Predicate::True => Some(Predicate::True),
        Predicate::False => Some(Predicate::False),
        Predicate::Opaque { text } => Some(Predicate::Opaque { text: text.clone() }),
    }
}

/// Render a predicate in human-readable form for finding details. Not a
/// pretty-printer beyond what's needed to read the diff in a terminal.
fn format_predicate(p: &Predicate) -> String {
    match p {
        Predicate::Cmp { op, left, right } => {
            format!(
                "{} {} {}",
                format_term(left),
                op.as_str(),
                format_term(right)
            )
        }
        Predicate::And { items } => items
            .iter()
            .map(format_predicate)
            .collect::<Vec<_>>()
            .join(" && "),
        Predicate::Or { items } => items
            .iter()
            .map(format_predicate)
            .collect::<Vec<_>>()
            .join(" || "),
        Predicate::Not { item } => format!("!({})", format_predicate(item)),
        Predicate::Truthy { term } => format_term(term),
        Predicate::True => "true".to_string(),
        Predicate::False => "false".to_string(),
        Predicate::Opaque { text } => format!("<opaque: {}>", text),
    }
}

fn format_term(t: &Term) -> String {
    match t {
        Term::Param { index } => format!("p{}", index),
        Term::Const { value } => value.display(),
        Term::Field { base, name } => format!("{}.{}", format_term(base), name),
        Term::Opaque { text } => format!("<opaque: {}>", text),
    }
}

/// Best-effort equivalence between two arg expressions across translation,
/// optionally remapping rust parameter indices through the enclosing map.
/// Conservative: returns `false` for shapes we can't reason about (e.g.
/// `Param` on one side, `Opaque` on the other), because falsely declaring
/// equivalence would mask real drift.
fn args_equivalent(ra: &ArgExpr, oa: &ArgExpr, enclosing_map: Option<&ParameterMap>) -> bool {
    match (ra, oa) {
        (ArgExpr::Const { value: rc }, ArgExpr::Const { value: oc }) => rc.equivalent_to(oc),
        (ArgExpr::Param { index: ri }, ArgExpr::Param { index: oi }) => {
            match enclosing_map {
                Some(m) => m.rust_to_other.get(*ri as usize).copied().flatten() == Some(*oi),
                // Without a map, fall back to positional identity. Better
                // than always-false: it lets the swap detector fire on
                // simple `f(a, b)` ↔ `f(b, a)` where `a`, `b` are the
                // enclosing function's params at the same indices on
                // both sides.
                None => ri == oi,
            }
        }
        (ArgExpr::NestedCall { callee: rc }, ArgExpr::NestedCall { callee: oc }) => rc == oc,
        (ArgExpr::Opaque { text: rt }, ArgExpr::Opaque { text: ot }) => rt.trim() == ot.trim(),
        _ => false,
    }
}

fn format_arg(a: &ArgExpr) -> String {
    match a {
        ArgExpr::Const { value } => value.display(),
        ArgExpr::Param { index } => format!("p{}", index),
        ArgExpr::NestedCall { callee } => format!("{}(…)", callee),
        ArgExpr::Opaque { text } => {
            let t = text.trim();
            if t.len() > 20 {
                format!("{}…", &t[..20])
            } else {
                t.to_string()
            }
        }
    }
}

fn check_aligned(
    rs: &CallSite,
    os: &CallSite,
    enclosing_map: Option<&ParameterMap>,
    callee_signature_arities: Option<&HashSet<(u32, u32)>>,
    callee: &str,
    findings: &mut Vec<ArgFlowFinding>,
) {
    if rs.in_loop != os.in_loop {
        findings.push(ArgFlowFinding {
            kind: FindingKind::InLoopDelta,
            callee: callee.to_string(),
            rust_span: rs.span,
            other_span: os.span,
            detail: format!("rust in_loop={}, other in_loop={}", rs.in_loop, os.in_loop),
            severity: FindingKind::InLoopDelta.default_severity(),
        });
    }

    if rs.args.len() != os.args.len() {
        // If the callsite arities match any known translated signature for
        // this callee, this is just the documented API shape — not a
        // translation bug. The most common case is Rust dropping out-params
        // or trailing dsq buffers that the C signature carried; SIMD-variant
        // and `_with_scratch` overloads also live here.
        let pair = (rs.args.len() as u32, os.args.len() as u32);
        let consistent_with_signature = callee_signature_arities
            .map(|s| s.contains(&pair))
            .unwrap_or(false);
        if !consistent_with_signature {
            findings.push(ArgFlowFinding {
                kind: FindingKind::ArityDrift,
                callee: callee.to_string(),
                rust_span: rs.span,
                other_span: os.span,
                detail: format!(
                    "rust passes {} arg(s), other passes {}",
                    rs.args.len(),
                    os.args.len()
                ),
                severity: FindingKind::ArityDrift.default_severity(),
            });
        }
        // Different arities → bail out of per-position checks regardless,
        // since positional alignment is meaningless when arities differ.
        return;
    }

    // Path-condition drift: both sides have a fully-lowered predicate, and
    // after substituting rust's parameter indices into the other-side
    // numbering they're not structurally equivalent. Examples this catches:
    //  - direction flip with branches swapped: `if (x < 0) f()` vs
    //    `if (x >= 0) f()` (the calls land in opposite branches)
    //  - moving a call out from under a guard entirely
    //  - operator typo in translation (`==` vs `!=`, etc.)
    //
    // Skipped if either side has no path_cond (one is unconditional —
    // dangling-site or in-loop checks already cover that), or if either
    // predicate has any `Opaque` leaf, or if substitution can't complete
    // (a Param has no counterpart). Better to underreport than emit a
    // misleading "predicates differ" finding.
    if let (Some(rs_cond), Some(os_cond), Some(map)) =
        (rs.path_cond.as_ref(), os.path_cond.as_ref(), enclosing_map)
    {
        if !rs_cond.has_opaque() && !os_cond.has_opaque() {
            if let Some(remapped) = remap_predicate(rs_cond, map) {
                let rs_canon = remapped.canonicalize();
                let os_canon = os_cond.clone().canonicalize();
                if !rs_canon.equivalent_to(&os_canon) {
                    findings.push(ArgFlowFinding {
                        kind: FindingKind::PathCondDrift,
                        callee: callee.to_string(),
                        rust_span: rs.span,
                        other_span: os.span,
                        detail: format!(
                            "path conditions differ: rust=`{}` (mapped: `{}`), other=`{}`",
                            format_predicate(rs_cond),
                            format_predicate(&rs_canon),
                            format_predicate(&os_canon)
                        ),
                        severity: FindingKind::PathCondDrift.default_severity(),
                    });
                }
            }
        }
    }

    // Argument-order swap: catch transpositions where rust arg `i` and arg
    // `j` mismatch their counterparts at positions `i` and `j` but match
    // each other when swapped. Strong evidence the translator transposed
    // two args of the same callee. Reported per-pair `(i, j)`. Positions
    // involved in a detected swap are suppressed in the per-position
    // const/forwarding checks below since the swap explains the mismatch.
    let mut swapped: HashSet<usize> = HashSet::new();
    for i in 0..rs.args.len() {
        for j in (i + 1)..rs.args.len() {
            if swapped.contains(&i) || swapped.contains(&j) {
                continue;
            }
            let i_pos_match = args_equivalent(&rs.args[i], &os.args[i], enclosing_map);
            let j_pos_match = args_equivalent(&rs.args[j], &os.args[j], enclosing_map);
            if i_pos_match || j_pos_match {
                continue;
            }
            let cross_i_to_j = args_equivalent(&rs.args[i], &os.args[j], enclosing_map);
            let cross_j_to_i = args_equivalent(&rs.args[j], &os.args[i], enclosing_map);
            if cross_i_to_j && cross_j_to_i {
                findings.push(ArgFlowFinding {
                    kind: FindingKind::ArgOrderSwap,
                    callee: callee.to_string(),
                    rust_span: rs.span,
                    other_span: os.span,
                    detail: format!(
                        "args {}↔{} swapped: rust=({}, {}) vs other=({}, {})",
                        i,
                        j,
                        format_arg(&rs.args[i]),
                        format_arg(&rs.args[j]),
                        format_arg(&os.args[i]),
                        format_arg(&os.args[j]),
                    ),
                    severity: FindingKind::ArgOrderSwap.default_severity(),
                });
                swapped.insert(i);
                swapped.insert(j);
            }
        }
    }

    for (i, (ra, oa)) in rs.args.iter().zip(os.args.iter()).enumerate() {
        if swapped.contains(&i) {
            continue;
        }
        // Const drift — values differ on aligned literal positions.
        if let (ArgExpr::Const { value: rc }, ArgExpr::Const { value: oc }) = (ra, oa) {
            if !rc.equivalent_to(oc) {
                findings.push(ArgFlowFinding {
                    kind: FindingKind::ConstDrift,
                    callee: callee.to_string(),
                    rust_span: rs.span,
                    other_span: os.span,
                    detail: format!("arg {}: rust={}, other={}", i, rc.display(), oc.display()),
                    severity: FindingKind::ConstDrift.default_severity(),
                });
            }
        }

        // Forwarding skew — needs the enclosing function's parameter map.
        if let Some(map) = enclosing_map {
            if let (ArgExpr::Param { index: ri }, ArgExpr::Param { index: oi }) = (ra, oa) {
                let expected = map.rust_to_other.get(*ri as usize).copied().flatten();
                match expected {
                    Some(eoi) if eoi == *oi => { /* consistent */ }
                    Some(eoi) => findings.push(ArgFlowFinding {
                        kind: FindingKind::ForwardingSkew,
                        callee: callee.to_string(),
                        rust_span: rs.span,
                        other_span: os.span,
                        detail: format!(
                            "arg {}: rust forwards param idx {} (expected other idx {}), other forwards idx {}",
                            i, ri, eoi, oi
                        ),
                        severity: FindingKind::ForwardingSkew.default_severity(),
                    }),
                    None => findings.push(ArgFlowFinding {
                        kind: FindingKind::ForwardingSkew,
                        callee: callee.to_string(),
                        rust_span: rs.span,
                        other_span: os.span,
                        detail: format!(
                            "arg {}: rust forwards param idx {} but that param has no counterpart on other",
                            i, ri
                        ),
                        severity: FindingKind::ForwardingSkew.default_severity() * 0.8,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        Constant, FunctionAnalysis, Halstead, Location, Metrics, Signature, TypeRef,
    };
    use std::path::PathBuf;

    fn p(name: &str, ty: &str) -> Param {
        Param {
            name: name.into(),
            ty: TypeRef::new(ty),
        }
    }

    fn fa_sig(name: &str, params: Vec<Param>) -> FunctionAnalysis {
        FunctionAnalysis {
            name: name.into(),
            original_name: None,
            mangled: None,
            enclosing_type: None,
            location: Location {
                file: PathBuf::from("/x"),
                line_start: 1,
                line_end: 1,
                col_start: 0,
                col_end: 0,
                byte_start: 0,
                byte_end: 0,
            },
            signature: Signature {
                inputs: params,
                outputs: vec![],
            },
            metrics: Metrics {
                halstead: Halstead::default(),
                ..Default::default()
            },
            constants: vec![],
            calls: vec![],
            call_sites: vec![],
            types_used: vec![],
            attributes: Default::default(),
        }
    }

    #[test]
    fn name_match_pairs_unambiguously() {
        let r = fa_sig("f", vec![p("count", "i32"), p("buf", "&mut [u8]")]);
        let o = fa_sig("f", vec![p("buf", "char *"), p("count", "size_t")]);
        let m = infer_parameter_map(&r, &o).expect("name match");
        assert_eq!(m.source, ParamMapSource::Name);
        // rust idx 0 = count -> other idx 1 = count
        // rust idx 1 = buf   -> other idx 0 = buf
        assert_eq!(m.rust_to_other, vec![Some(1), Some(0)]);
    }

    #[test]
    fn type_match_when_distinctive() {
        // No name overlap, but type vector aligns uniquely: there's exactly
        // one int and one string on each side, so the assignment is forced.
        let r = fa_sig("f", vec![p("a", "i32"), p("s", "&str")]);
        let o = fa_sig("f", vec![p("text", "char *"), p("count", "int")]);
        let m = infer_parameter_map(&r, &o).expect("type match");
        assert_eq!(m.source, ParamMapSource::Type);
        // rust int (idx 0) -> other idx 1 (int); rust string (idx 1) -> other idx 0
        assert_eq!(m.rust_to_other, vec![Some(1), Some(0)]);
    }

    #[test]
    fn refuses_ambiguous_int_vector() {
        // Three same-typed ints on each side, no name overlap. Phase A
        // refuses rather than guessing positionally — the caller will list
        // this pair as unresolved and ask the user for an explicit map.
        let r = fa_sig("f", vec![p("a", "i32"), p("b", "i32"), p("c", "i32")]);
        let o = fa_sig("f", vec![p("x", "int"), p("y", "int"), p("z", "int")]);
        let err = infer_parameter_map(&r, &o).unwrap_err();
        match err {
            NoMapReason::AmbiguousTypes => {}
            other => panic!("expected AmbiguousTypes, got {:?}", other),
        }
    }

    #[test]
    fn positional_when_arity_and_types_align() {
        // No name match, type categories distinctive in same order on both
        // sides: positional with type corroboration accepted.
        let r = fa_sig("f", vec![p("x", "i32"), p("y", "&str")]);
        let o = fa_sig("f", vec![p("a", "int"), p("b", "char *")]);
        let m = infer_parameter_map(&r, &o).expect("positional");
        // (Note: this case actually resolves via Type-match because the
        // categories are distinctive enough — there's only one int and one
        // string on each side, so the unique-by-type code finds it. The
        // positional branch is the strict fallback when types are equal but
        // the positional alignment happens to match.)
        assert!(matches!(
            m.source,
            ParamMapSource::Type | ParamMapSource::Positional
        ));
        assert_eq!(m.rust_to_other, vec![Some(0), Some(1)]);
    }

    #[test]
    fn arity_mismatch_reported_when_no_other_signal() {
        // No name overlap and the type categories don't intersect at all
        // (rust has Int, other has only Strings), so name + type matching
        // both produce NoSignal and we fall through to the positional check
        // — which can't run because arities differ.
        let r = fa_sig("f", vec![p("a", "i32")]);
        let o = fa_sig("f", vec![p("x", "char *"), p("y", "char *")]);
        let err = infer_parameter_map(&r, &o).unwrap_err();
        assert!(
            matches!(err, NoMapReason::ArityMismatch { .. }),
            "got {:?}",
            err
        );
    }

    #[test]
    fn ambiguous_types_when_extra_param_of_same_category() {
        // Rust has 1 int, other has 2 ints — we can't tell which other-int
        // the rust-int corresponds to. Refuse rather than guess; user adds
        // an explicit mapping or renames the params.
        let r = fa_sig("f", vec![p("a", "i32")]);
        let o = fa_sig("f", vec![p("x", "i32"), p("y", "i32")]);
        let err = infer_parameter_map(&r, &o).unwrap_err();
        assert!(matches!(err, NoMapReason::AmbiguousTypes), "got {:?}", err);
    }

    fn site(callee: &str, args: Vec<ArgExpr>, in_loop: bool) -> CallSite {
        CallSite {
            callee: callee.into(),
            span: (0, 0),
            args,
            in_loop,
            path_cond: None,
        }
    }

    fn const_int(v: i64) -> ArgExpr {
        ArgExpr::Const {
            value: Constant::Int {
                value: v,
                text: v.to_string(),
                span: (0, 0),
            },
        }
    }

    #[test]
    fn arg_order_swap_flagged_when_const_args_transposed() {
        // Same callee, same arity, same constant values — but in opposite
        // positions. Translation transposed two args.
        let rs = site("f", vec![const_int(7), const_int(8)], false);
        let os = site("f", vec![const_int(8), const_int(7)], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, None, None, "f", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ArgOrderSwap);
    }

    #[test]
    fn arg_order_swap_with_param_map_uses_remapped_indices() {
        // Enclosing pair: identity map. Rust calls inner(p0, p1); other
        // calls inner(p1, p0). Under the identity map, args at position
        // 0 don't match (p0 vs p1) but cross-match (rust p0 ↔ other p0,
        // rust p1 ↔ other p1). Flag swap.
        let map = ParameterMap {
            rust_to_other: vec![Some(0), Some(1)],
            source: ParamMapSource::Positional,
        };
        let rs = site(
            "inner",
            vec![ArgExpr::Param { index: 0 }, ArgExpr::Param { index: 1 }],
            false,
        );
        let os = site(
            "inner",
            vec![ArgExpr::Param { index: 1 }, ArgExpr::Param { index: 0 }],
            false,
        );
        let mut findings = vec![];
        check_aligned(&rs, &os, Some(&map), None, "inner", &mut findings);
        let kinds: Vec<_> = findings.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FindingKind::ArgOrderSwap),
            "expected ArgOrderSwap; got {:?}",
            kinds
        );
    }

    #[test]
    fn arg_order_swap_not_flagged_when_only_one_position_mismatches() {
        // arg 0 matches, arg 1 doesn't match its counterpart and doesn't
        // cross-match either — that's plain ConstDrift, not a swap.
        let rs = site("f", vec![const_int(7), const_int(8)], false);
        let os = site("f", vec![const_int(7), const_int(9)], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, None, None, "f", &mut findings);
        assert!(
            findings.iter().all(|f| f.kind != FindingKind::ArgOrderSwap),
            "no swap; saw {:?}",
            findings.iter().map(|f| f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn forwarding_skew_uses_enclosing_param_map() {
        // Enclosing pair: rust(a, b) ↔ other(b, a) — params swapped (i.e.
        // map says rust idx 0 ↔ other idx 1). At the call site, rust
        // forwards its idx-0 param and other forwards its idx-1 param.
        // That's *consistent* with the map, so no finding.
        let map = ParameterMap {
            rust_to_other: vec![Some(1), Some(0)],
            source: ParamMapSource::Name,
        };
        let rs = site("inner", vec![ArgExpr::Param { index: 0 }], false);
        let os = site("inner", vec![ArgExpr::Param { index: 1 }], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, Some(&map), None, "inner", &mut findings);
        assert!(findings.is_empty(), "consistent forwarding should not flag");

        // Now break it: rust forwards idx 0 but other forwards idx 0 (not
        // 1, which is what the map predicts). That's a real skew.
        let os_bad = site("inner", vec![ArgExpr::Param { index: 0 }], false);
        let mut findings = vec![];
        check_aligned(&rs, &os_bad, Some(&map), None, "inner", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ForwardingSkew);
    }

    #[test]
    fn const_drift_flagged_at_aligned_position() {
        let rs = site("f", vec![const_int(7)], false);
        let os = site("f", vec![const_int(8)], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, None, None, "f", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ConstDrift);
    }

    #[test]
    fn arity_drift_short_circuits_per_position_checks() {
        let rs = site("f", vec![const_int(7), const_int(0)], false);
        let os = site("f", vec![const_int(9)], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, None, None, "f", &mut findings);
        // Only ArityDrift, not ConstDrift on the first position — we don't
        // pretend to compare positions when arities differ.
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ArityDrift);
    }

    #[test]
    fn mapping_override_resolves_ambiguous_pair() {
        // Three same-typed ints with no name overlap — Phase A would refuse
        // (AmbiguousTypes). With an explicit mapping entry pinning the
        // permutation, analyze_arg_flow uses it as ParamMapSource::Explicit.
        use crate::compare::matching::{match_reports, Mapping, MappingEntry};
        use crate::core::{Language, Report};

        let r_caller = fa_sig("caller", vec![p("a", "i32"), p("b", "i32"), p("c", "i32")]);
        let o_caller = fa_sig("caller", vec![p("x", "int"), p("y", "int"), p("z", "int")]);

        let rust = Report {
            schema_version: crate::core::SCHEMA_VERSION,
            language: Language::Rust,
            source_file: "/r".into(),
            source_hash: "0".into(),
            functions: vec![r_caller],
            structs: vec![],
        };
        let other = Report {
            schema_version: crate::core::SCHEMA_VERSION,
            language: Language::C,
            source_file: "/o".into(),
            source_hash: "0".into(),
            functions: vec![o_caller],
            structs: vec![],
        };
        let mapping = Mapping {
            entries: vec![MappingEntry {
                rust: "caller".into(),
                other: "caller".into(),
                params: Some(vec![[0, 1], [1, 0], [2, 2]]),
                ..Default::default()
            }],
            ..Default::default()
        };

        let matched = match_reports(&rust, &other, Some(&mapping));
        assert_eq!(matched.pairs.len(), 1);
        let analysis = analyze_arg_flow(&matched, Some(&mapping));
        let p = &analysis.pairs[0];
        let pm = p
            .parameter_map
            .as_ref()
            .expect("override should have set the map");
        assert_eq!(pm.source, ParamMapSource::Explicit);
        assert_eq!(pm.rust_to_other, vec![Some(1), Some(0), Some(2)]);
        assert!(p.no_map_reason.is_none());
        assert!(analysis.unresolved_pairs.is_empty());
    }

    #[test]
    fn in_loop_delta_flagged() {
        let rs = site("f", vec![const_int(0)], true);
        let os = site("f", vec![const_int(0)], false);
        let mut findings = vec![];
        check_aligned(&rs, &os, None, None, "f", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::InLoopDelta);
    }

    #[test]
    fn path_cond_drift_flagged_on_direction_flip() {
        // Enclosing pair: identity param map, single Param(0) on both sides.
        // rust call site is guarded by `x < 0`; other call site is guarded
        // by `x >= 0`. After canonicalisation those differ — this is the
        // direction-flip pattern, which would otherwise pass undetected.
        use crate::core::{CmpOp, Constant, Predicate, Term};
        let map = ParameterMap {
            rust_to_other: vec![Some(0)],
            source: ParamMapSource::Name,
        };
        let zero = Term::Const {
            value: Constant::Int {
                value: 0,
                text: "0".into(),
                span: (0, 0),
            },
        };
        let rs = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Cmp {
                op: CmpOp::Lt,
                left: Term::Param { index: 0 },
                right: zero.clone(),
            }),
        };
        let os = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Cmp {
                op: CmpOp::Ge,
                left: Term::Param { index: 0 },
                right: zero,
            }),
        };
        let mut findings = vec![];
        check_aligned(&rs, &os, Some(&map), None, "g", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::PathCondDrift);
    }

    #[test]
    fn path_cond_equivalent_after_canonicalisation_no_finding() {
        // `x < 0` (rust) vs `0 > x` (other). Both canonicalise to the same
        // form; should NOT fire PathCondDrift.
        use crate::core::{CmpOp, Constant, Predicate, Term};
        let map = ParameterMap {
            rust_to_other: vec![Some(0)],
            source: ParamMapSource::Name,
        };
        let zero = Term::Const {
            value: Constant::Int {
                value: 0,
                text: "0".into(),
                span: (0, 0),
            },
        };
        let rs = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Cmp {
                op: CmpOp::Lt,
                left: Term::Param { index: 0 },
                right: zero.clone(),
            }),
        };
        let os = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Cmp {
                op: CmpOp::Gt,
                left: zero,
                right: Term::Param { index: 0 },
            }),
        };
        let mut findings = vec![];
        check_aligned(&rs, &os, Some(&map), None, "g", &mut findings);
        assert!(
            findings.is_empty(),
            "equivalent forms should not flag, got {:?}",
            findings
        );
    }

    #[test]
    fn path_cond_with_opaque_is_skipped() {
        // If either side has an Opaque term in its predicate, we skip the
        // finding rather than risk a false positive on coincidental
        // textual equality / inequality.
        use crate::core::{Predicate, Term};
        let map = ParameterMap {
            rust_to_other: vec![Some(0)],
            source: ParamMapSource::Name,
        };
        let rs = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Truthy {
                term: Term::Opaque {
                    text: "some_helper(x)".into(),
                },
            }),
        };
        let os = CallSite {
            callee: "g".into(),
            span: (0, 0),
            args: vec![],
            in_loop: false,
            path_cond: Some(Predicate::Truthy {
                term: Term::Opaque {
                    text: "different_helper(x)".into(),
                },
            }),
        };
        let mut findings = vec![];
        check_aligned(&rs, &os, Some(&map), None, "g", &mut findings);
        assert!(findings.is_empty(), "Opaque predicates should be skipped");
    }
}
