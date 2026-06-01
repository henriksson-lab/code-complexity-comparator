//! Per-field structural comparison for matched struct pairs. Builds on top
//! of `match_structs` (name-level pairing) and walks each pair's fields
//! looking for:
//!
//! - **Disabled in rust** — a rust field whose name starts with `_` (e.g.
//!   `_unused: T`), or whose type is a placeholder (`()`, `PhantomData<…>`)
//!   while the other side has a real field. This is the headline check
//!   the user asked for: it catches "I ported the struct but forgot to
//!   actually wire this member up."
//! - **Type mismatch** — both sides have a field that aligns by name, but
//!   the inferred `TypeCategory` differs (rust int ↔ C string, say). Uses
//!   `classify_type` so spelling differences (`u32` vs `uint32_t`) don't
//!   trip the check.
//! - **Missing in rust** — other has a field with no counterpart on rust.
//! - **Extra in rust** — rust has a field with no counterpart on other.
//!   Lower severity by default; new abstractions on the rust side are
//!   common during a port.
//!
//! Field alignment is name-based only. Exact match wins, then `_`-stripped
//! match (so `_count` ↔ `count` aligns and gets flagged DisabledInRust),
//! then `normalize_name` (snake/camel-fold). Positional fallback is
//! deliberately *not* attempted: with structs, position is meaningless
//! across translations and forced positional pairing produces noise.

use crate::compare::matching::{class_matches, path_suffix_matches, Mapping};
use crate::compare::structs::{StructMatchResult, StructMatchStrategy};
use crate::core::{StructAnalysis, StructField};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FieldFindingKind {
    /// Rust has the field but its name is `_xxx` (intentionally unused) or
    /// its type is a placeholder (`()`, `PhantomData<…>`), while the
    /// counterpart on the other side is a regular field. Headline finding —
    /// these are exactly the "ported the struct but didn't wire up this
    /// member" cases.
    DisabledInRust,
    /// Both sides have an aligned field but `classify_type` disagrees on
    /// the category (e.g. rust `String` ↔ C `int`).
    TypeMismatch,
    /// Field exists on the other side; no counterpart on rust.
    MissingInRust,
    /// Field exists on rust; no counterpart on other. Lower severity —
    /// new infra on the rust side is common during a port.
    ExtraInRust,
}

impl FieldFindingKind {
    pub fn name(&self) -> &'static str {
        match self {
            FieldFindingKind::DisabledInRust => "disabled_in_rust",
            FieldFindingKind::TypeMismatch => "type_mismatch",
            FieldFindingKind::MissingInRust => "missing_in_rust",
            FieldFindingKind::ExtraInRust => "extra_in_rust",
        }
    }

    fn default_severity(&self) -> f64 {
        match self {
            FieldFindingKind::DisabledInRust => 1.0,
            FieldFindingKind::TypeMismatch => 0.7,
            FieldFindingKind::MissingInRust => 0.6,
            FieldFindingKind::ExtraInRust => 0.4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldFinding {
    pub kind: FieldFindingKind,
    /// Field name on whichever side has it. For `DisabledInRust` /
    /// `TypeMismatch`, this is the rust-side name (since both sides exist).
    pub field: String,
    pub detail: String,
    pub severity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairFieldDiff {
    pub rust_name: String,
    pub other_name: String,
    pub match_strategy: StructMatchStrategy,
    pub rust_field_count: u32,
    pub other_field_count: u32,
    pub findings: Vec<FieldFinding>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructFieldDiffSummary {
    pub matched_pairs: usize,
    pub pairs_with_findings: usize,
    pub pairs_with_arity_mismatch: usize,
    pub total_findings: usize,
    pub findings_by_kind: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructFieldDiffAnalysis {
    pub summary: StructFieldDiffSummary,
    pub pairs: Vec<PairFieldDiff>,
}

/// Top-level entry. Produces a per-pair finding list ranked by severity.
///
/// `mapping` is the same file used for function/struct matching. When an
/// entry has `fields = [[rust_name, other_name], ...]`, those alignments
/// take priority over the heuristic passes — the caller writes them when
/// renames hide a field from canonicalisation (e.g. `count` ↔ `n_items`).
pub fn analyze_struct_field_diff(
    matched: &StructMatchResult,
    mapping: Option<&Mapping>,
) -> StructFieldDiffAnalysis {
    let mut pairs_out = Vec::new();
    let mut findings_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_findings = 0usize;
    let mut pairs_with_findings = 0usize;
    let mut pairs_with_arity_mismatch = 0usize;

    for p in &matched.pairs {
        let override_fields = mapping.and_then(|m| field_overrides_for_pair(m, p.rust, p.other));
        let findings = check_struct_pair(p.rust, p.other, override_fields.as_ref());
        let score: f64 = findings.iter().map(|f| f.severity).sum();
        for f in &findings {
            *findings_by_kind
                .entry(f.kind.name().to_string())
                .or_insert(0) += 1;
        }
        total_findings += findings.len();
        if !findings.is_empty() {
            pairs_with_findings += 1;
        }
        let r_count = p.rust.fields.len() as u32;
        let o_count = p.other.fields.len() as u32;
        if r_count != o_count {
            pairs_with_arity_mismatch += 1;
        }
        pairs_out.push(PairFieldDiff {
            rust_name: p.rust.name.clone(),
            other_name: p.other.name.clone(),
            match_strategy: p.strategy,
            rust_field_count: r_count,
            other_field_count: o_count,
            findings,
            score,
        });
    }

    pairs_out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rust_name.cmp(&b.rust_name))
    });

    StructFieldDiffAnalysis {
        summary: StructFieldDiffSummary {
            matched_pairs: matched.pairs.len(),
            pairs_with_findings,
            pairs_with_arity_mismatch,
            total_findings,
            findings_by_kind,
        },
        pairs: pairs_out,
    }
}

fn check_struct_pair(
    rust: &StructAnalysis,
    other: &StructAnalysis,
    overrides: Option<&HashMap<String, String>>,
) -> Vec<FieldFinding> {
    let mut findings = Vec::new();

    // Build alignment: for each rust field, find one matching other field.
    // Four passes in priority order:
    //   0. Explicit mapping override (`fields = [["a","x"], ...]` in TOML).
    //   1. Exact name (rust ↔ other).
    //   2. Strip leading `_` from rust side, then exact match. This is how
    //      `_count` aligns to `count` — and why DisabledInRust fires
    //      rather than ExtraInRust on disabled-in-rust fields.
    //   3. Canonicalise both sides via `canonicalize_field_name` (snake-fold
    //      + leading underscore strip + `m_` member-prefix strip).
    // Each other-side field can be claimed once.
    let mut other_used: HashSet<usize> = HashSet::new();
    let mut alignment: Vec<Option<usize>> = vec![None; rust.fields.len()];

    // Pass 0: explicit overrides.
    if let Some(map) = overrides {
        let other_idx_by_name: HashMap<&str, usize> = other
            .fields
            .iter()
            .enumerate()
            .map(|(j, f)| (f.name.as_str(), j))
            .collect();
        for (i, rf) in rust.fields.iter().enumerate() {
            if let Some(other_name) = map.get(&rf.name) {
                if let Some(&j) = other_idx_by_name.get(other_name.as_str()) {
                    if !other_used.contains(&j) {
                        alignment[i] = Some(j);
                        other_used.insert(j);
                    }
                }
            }
        }
    }
    // Pass 1: exact name.
    for (i, rf) in rust.fields.iter().enumerate() {
        if alignment[i].is_some() {
            continue;
        }
        if let Some((j, _)) = other
            .fields
            .iter()
            .enumerate()
            .find(|(j, of)| !other_used.contains(j) && of.name == rf.name)
        {
            alignment[i] = Some(j);
            other_used.insert(j);
        }
    }
    // Pass 2: strip leading `_` from rust side, then exact match. This is
    // how `_count` aligns to `count` — and why we can flag DisabledInRust.
    for (i, rf) in rust.fields.iter().enumerate() {
        if alignment[i].is_some() {
            continue;
        }
        let stripped = strip_disabled_prefix(&rf.name);
        if stripped == rf.name {
            continue;
        }
        if let Some((j, _)) = other
            .fields
            .iter()
            .enumerate()
            .find(|(j, of)| !other_used.contains(j) && of.name == stripped)
        {
            alignment[i] = Some(j);
            other_used.insert(j);
        }
    }
    // Pass 3: canonical-rust-name fold on both sides.
    for (i, rf) in rust.fields.iter().enumerate() {
        if alignment[i].is_some() {
            continue;
        }
        let rn = canonicalize_field_name(&rf.name);
        if rn.is_empty() {
            continue;
        }
        if let Some((j, _)) = other
            .fields
            .iter()
            .enumerate()
            .find(|(j, of)| !other_used.contains(j) && canonicalize_field_name(&of.name) == rn)
        {
            alignment[i] = Some(j);
            other_used.insert(j);
        }
    }

    // Per-aligned-pair checks.
    for (i, rf) in rust.fields.iter().enumerate() {
        if let Some(j) = alignment[i] {
            let of = &other.fields[j];
            let disabled = field_disabled_reason(rf);
            if let Some(reason) = disabled {
                findings.push(FieldFinding {
                    kind: FieldFindingKind::DisabledInRust,
                    field: rf.name.clone(),
                    detail: format!(
                        "rust `{}: {}` paired with other `{}: {}` — {}",
                        rf.name, rf.ty.text, of.name, of.ty.text, reason
                    ),
                    severity: FieldFindingKind::DisabledInRust.default_severity(),
                });
                // Skip TypeMismatch on disabled fields — the type is
                // typically `()` / `PhantomData` and would always trip.
                continue;
            }
            if rf.category != of.category {
                findings.push(FieldFinding {
                    kind: FieldFindingKind::TypeMismatch,
                    field: rf.name.clone(),
                    detail: format!(
                        "rust `{}: {}` ({}) vs other `{}: {}` ({})",
                        rf.name,
                        rf.ty.text,
                        rf.category.as_str(),
                        of.name,
                        of.ty.text,
                        of.category.as_str()
                    ),
                    severity: FieldFindingKind::TypeMismatch.default_severity(),
                });
            }
        } else {
            // Rust field with no counterpart. If it looks disabled, that's
            // still worth flagging — the user added a placeholder where
            // the other side has nothing — but as ExtraInRust, since
            // there's no counterpart-arg to compare against.
            findings.push(FieldFinding {
                kind: FieldFindingKind::ExtraInRust,
                field: rust.fields[i].name.clone(),
                detail: format!(
                    "rust has `{}: {}` with no counterpart on other",
                    rust.fields[i].name, rust.fields[i].ty.text
                ),
                severity: FieldFindingKind::ExtraInRust.default_severity(),
            });
        }
    }
    for (j, of) in other.fields.iter().enumerate() {
        if !other_used.contains(&j) {
            findings.push(FieldFinding {
                kind: FieldFindingKind::MissingInRust,
                field: of.name.clone(),
                detail: format!(
                    "other has `{}: {}` with no counterpart on rust",
                    of.name, of.ty.text
                ),
                severity: FieldFindingKind::MissingInRust.default_severity(),
            });
        }
    }

    findings.sort_by(|a, b| {
        b.severity
            .partial_cmp(&a.severity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.field.cmp(&b.field))
    });
    findings
}

/// Detect intentionally-disabled rust field. Returns `Some(reason)` when
/// the field looks like a placeholder. Two patterns:
///
/// - **Name prefix** `_xxx` (where `xxx` starts with an alpha character).
///   The alpha gate excludes our walker's synthetic tuple-struct field
///   names (`_0`, `_1`, …) — those are not user-disabled, just unnamed
///   positional fields.
/// - **Placeholder type** `()` or `PhantomData<…>`. Both have a runtime
///   size (or zero size in PhantomData's case) but no real data; ports
///   sometimes use them as "I'll fill this in later" stubs.
fn field_disabled_reason(f: &StructField) -> Option<String> {
    if let Some(reason) = name_disabled_reason(&f.name) {
        return Some(reason);
    }
    let t = f.ty.text.trim();
    if t == "()" {
        return Some("type is `()` (zero-sized placeholder)".to_string());
    }
    if t.starts_with("PhantomData") || t.starts_with("std::marker::PhantomData") {
        return Some("type is `PhantomData<…>` (compile-time-only marker)".to_string());
    }
    None
}

fn name_disabled_reason(name: &str) -> Option<String> {
    if !name.starts_with('_') {
        return None;
    }
    // Skip synthetic tuple-struct names `_0`, `_1`, ... — those are produced
    // by the walker for unnamed positional fields, not user-disabled.
    if name.len() < 2 {
        return None;
    }
    let next = name.chars().nth(1)?;
    if next.is_ascii_digit() {
        return None;
    }
    Some(format!(
        "name starts with `_` (rust convention for intentionally-unused field)",
    ))
}

fn strip_disabled_prefix(name: &str) -> &str {
    if name_disabled_reason(name).is_some() {
        // Strip the leading underscore so `_count` aligns to `count`.
        &name[1..]
    } else {
        name
    }
}

/// Fold a field name into a canonical Rust form for cross-language matching.
/// Designed for struct fields specifically — different from the function-
/// oriented `compare::matching::normalize_name`, which strips suffixes like
/// `_impl` / `_inner` / `_rs` that are uncommon in field names but common
/// in function names.
///
/// Steps applied in order:
/// 1. Strip leading underscores (so `_count` and `count` collide).
/// 2. camelCase / PascalCase → snake_case (`fooBar` → `foo_bar`).
/// 3. Lowercase everything.
/// 4. Strip a leading `m_` member-convention prefix (C++ class fields
///    often use this — `m_count` → `count`). The strip only fires when
///    the remainder is a non-trivial identifier (≥ 2 chars), so tiny
///    names like `m_x` keep their `m_` rather than collapsing to `x`.
///
/// Not stripped (deliberately): `n_`, `p_`, `b_`, `i_`, `s_` — these have
/// too many legitimate uses in normal code (`n_items`, `p_value`,
/// `b_value`) to strip without ambiguity. Renames involving those go in
/// the explicit mapping table.
pub fn canonicalize_field_name(name: &str) -> String {
    let s = name.trim_start_matches('_');
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
        prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    if let Some(rest) = out.strip_prefix("m_") {
        if rest.len() >= 2 {
            return rest.to_string();
        }
    }
    out
}

/// Find the `MappingEntry` whose `rust`/`other` names + path/class
/// constraints match this struct pair, then return its `fields` overrides
/// as a `rust_field → other_field` lookup. Returns `None` when there's no
/// matching entry or it carries no `fields` table.
fn field_overrides_for_pair(
    mapping: &Mapping,
    rust: &StructAnalysis,
    other: &StructAnalysis,
) -> Option<HashMap<String, String>> {
    let entry = mapping.entries.iter().find(|e| {
        let r_match = e.rust == rust.name
            && path_suffix_matches(&rust.location.file, e.rust_path.as_deref())
            && e.rust_line.is_none_or(|ln| rust.location.line_start == ln)
            && class_matches(None, e.rust_class.as_deref());
        let o_match = e.other == other.name
            && path_suffix_matches(&other.location.file, e.other_path.as_deref())
            && e.other_line
                .is_none_or(|ln| other.location.line_start == ln)
            && class_matches(None, e.other_class.as_deref());
        r_match && o_match
    })?;
    let pairs = entry.fields.as_ref()?;
    let map: HashMap<String, String> = pairs.iter().map(|p| (p[0].clone(), p[1].clone())).collect();
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::structs::StructPair;
    use crate::core::{
        classify_type, Location, StructAnalysis, StructField, StructMetrics, TypeRef,
    };
    use std::path::PathBuf;

    fn field(name: &str, ty: &str) -> StructField {
        StructField {
            name: name.into(),
            ty: TypeRef::new(ty),
            category: classify_type(ty),
        }
    }

    fn sa(name: &str, fields: Vec<StructField>) -> StructAnalysis {
        StructAnalysis {
            name: name.into(),
            kind: "struct".into(),
            location: Location {
                file: PathBuf::from("/x"),
                line_start: 1,
                line_end: 1,
                col_start: 0,
                col_end: 0,
                byte_start: 0,
                byte_end: 0,
            },
            metrics: StructMetrics::from_fields(&fields),
            fields,
            attributes: Default::default(),
        }
    }

    fn run(rust: StructAnalysis, other: StructAnalysis) -> Vec<FieldFinding> {
        check_struct_pair(&rust, &other, None)
    }

    #[test]
    fn underscore_prefix_flagged_when_aligned_to_real_field() {
        // `_count: i32` aligns to `count` on the other side via the
        // strip-underscore pass; flagged as DisabledInRust.
        // Use a plain numeric on the other field so `classify_type` doesn't
        // surface its known quirk where `*const u8` (Pointer) and `char*`
        // (String) classify differently.
        let rust = sa("Foo", vec![field("_count", "i32"), field("size", "u32")]);
        let other = sa("foo", vec![field("count", "int"), field("size", "size_t")]);
        let findings = run(rust, other);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FieldFindingKind::DisabledInRust);
        assert_eq!(findings[0].field, "_count");
    }

    #[test]
    fn phantomdata_flagged() {
        let rust = sa("Foo", vec![field("count", "PhantomData<u32>")]);
        let other = sa("foo", vec![field("count", "int")]);
        let findings = run(rust, other);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FieldFindingKind::DisabledInRust);
        assert!(findings[0].detail.contains("PhantomData"));
    }

    #[test]
    fn unit_type_flagged() {
        let rust = sa("Foo", vec![field("count", "()")]);
        let other = sa("foo", vec![field("count", "int")]);
        let findings = run(rust, other);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FieldFindingKind::DisabledInRust);
    }

    #[test]
    fn synthetic_tuple_field_not_flagged() {
        // Tuple-struct positional fields named _0, _1 by the walker. These
        // are NOT user-disabled — they're just unnamed positional fields.
        // We must not flag them. Rust `struct Foo(i32)` produces field
        // `_0: i32`; it should align positionally with C's `i` if names
        // match — but here we only do name matching, so it lands as
        // ExtraInRust against an unrelated field. The key invariant is
        // just: no DisabledInRust finding fires on `_0`.
        let rust = sa("Foo", vec![field("_0", "i32")]);
        let other = sa("foo", vec![field("v", "int")]);
        let findings = run(rust, other);
        assert!(
            findings
                .iter()
                .all(|f| f.kind != FieldFindingKind::DisabledInRust),
            "synthetic _0 field should not be flagged: {:?}",
            findings
        );
    }

    #[test]
    fn type_mismatch_by_category() {
        // Same field name, different category (int vs string).
        let rust = sa("Foo", vec![field("name", "u32")]);
        let other = sa("foo", vec![field("name", "char *")]);
        let findings = run(rust, other);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FieldFindingKind::TypeMismatch);
    }

    #[test]
    fn type_mismatch_skipped_for_disabled_fields() {
        // `_count: ()` aligned to `count: int`. Type categories differ
        // (other vs int) but we don't double-emit — DisabledInRust supersedes.
        let rust = sa("Foo", vec![field("_count", "()")]);
        let other = sa("foo", vec![field("count", "int")]);
        let findings = run(rust, other);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FieldFindingKind::DisabledInRust);
    }

    #[test]
    fn missing_and_extra_reported() {
        let rust = sa("Foo", vec![field("a", "i32"), field("z", "u32")]);
        let other = sa("foo", vec![field("a", "int"), field("b", "int")]);
        let findings = run(rust, other);
        // `a` aligns and matches type → no finding.
        // `z` (rust) has no counterpart → ExtraInRust.
        // `b` (other) has no counterpart → MissingInRust.
        assert_eq!(findings.len(), 2);
        let kinds: HashSet<_> = findings.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&FieldFindingKind::ExtraInRust));
        assert!(kinds.contains(&FieldFindingKind::MissingInRust));
    }

    #[test]
    fn faithful_translation_no_findings() {
        let rust = sa("Foo", vec![field("count", "u32"), field("name", "String")]);
        let other = sa(
            "foo",
            vec![field("count", "uint32_t"), field("name", "char *")],
        );
        let findings = run(rust, other);
        assert!(findings.is_empty(), "got {:?}", findings);
    }

    #[test]
    fn analysis_aggregates_summary() {
        let rust = sa("Foo", vec![field("_count", "i32"), field("size", "u32")]);
        let other = sa("foo", vec![field("count", "int"), field("size", "size_t")]);
        let pair = StructPair {
            rust: &rust,
            other: &other,
            strategy: StructMatchStrategy::ExactName,
        };
        let matched = StructMatchResult { pairs: vec![pair] };
        let analysis = analyze_struct_field_diff(&matched, None);
        assert_eq!(analysis.summary.matched_pairs, 1);
        assert_eq!(analysis.summary.pairs_with_findings, 1);
        // Field counts are equal (2 each), so no arity mismatch.
        assert_eq!(analysis.summary.pairs_with_arity_mismatch, 0);
        assert_eq!(analysis.summary.total_findings, 1);
    }
}
