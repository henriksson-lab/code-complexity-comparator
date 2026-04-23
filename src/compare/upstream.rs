use crate::compare::deviation::Weights;
use crate::compare::matching::{
    class_matches, path_suffix_matches, MatchResult, MatchStrategy,
};
use crate::compare::metric_vector;
use crate::core::{FunctionAnalysis, Report};
use crate::order;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionSelector {
    pub name: Option<String>,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub class: Option<String>,
}

impl FunctionSelector {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.path.is_none() && self.line.is_none() && self.class.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionRef {
    pub name: String,
    pub file: String,
    pub line_start: u32,
    pub enclosing_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamWarning {
    pub side: String,
    pub function: FunctionRef,
    pub counterpart: Option<FunctionRef>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamPairRow {
    pub rust: FunctionRef,
    pub other: FunctionRef,
    pub match_strategy: MatchStrategy,
    pub rust_in_upstream: bool,
    pub other_in_upstream: bool,
    pub overlap: bool,
    pub total: f64,
    pub per_metric: Vec<(String, f64, f64, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamAnalysis {
    pub rust_seed: Option<FunctionRef>,
    pub other_seed: Option<FunctionRef>,
    pub rust_upstream: Vec<FunctionRef>,
    pub other_upstream: Vec<FunctionRef>,
    pub warnings: Vec<UpstreamWarning>,
    pub pairs: Vec<UpstreamPairRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FunctionKey<'a> {
    name: &'a str,
    file: &'a std::path::Path,
    line_start: u32,
}

fn function_key<'a>(f: &'a FunctionAnalysis) -> FunctionKey<'a> {
    FunctionKey {
        name: &f.name,
        file: &f.location.file,
        line_start: f.location.line_start,
    }
}

fn function_ref(f: &FunctionAnalysis) -> FunctionRef {
    FunctionRef {
        name: f.name.clone(),
        file: f.location.file.to_string_lossy().into_owned(),
        line_start: f.location.line_start,
        enclosing_type: f.enclosing_type.clone(),
    }
}

fn index_by_key(report: &Report) -> HashMap<FunctionKey<'_>, usize> {
    report
        .functions
        .iter()
        .enumerate()
        .map(|(idx, f)| (function_key(f), idx))
        .collect()
}

pub fn analyze_upstream(
    rust: &Report,
    other: &Report,
    matches: &MatchResult<'_>,
    rust_selector: Option<&FunctionSelector>,
    other_selector: Option<&FunctionSelector>,
    strict: bool,
) -> Result<UpstreamAnalysis> {
    let rust_seed_idx = resolve_seed("rust", rust, rust_selector)?;
    let other_seed_idx = resolve_seed("other", other, other_selector)?;

    if rust_seed_idx.is_none() && other_seed_idx.is_none() {
        return Err(anyhow!("provide at least one seed function selector"));
    }

    let rust_index_by_key = index_by_key(rust);
    let other_index_by_key = index_by_key(other);
    let (pair_by_rust, pair_by_other) = pair_maps(matches, &rust_index_by_key, &other_index_by_key);

    let rust_seed_idx = match rust_seed_idx {
        Some(i) => Some(i),
        None => other_seed_idx
            .and_then(|i| pair_by_other.get(&function_key(&other.functions[i])).copied()),
    };
    let other_seed_idx = match other_seed_idx {
        Some(i) => Some(i),
        None => rust_seed_idx
            .and_then(|i| pair_by_rust.get(&function_key(&rust.functions[i])).copied()),
    };

    let rust_upstream_idx = rust_seed_idx
        .map(|i| upstream_indices(rust, i, strict))
        .unwrap_or_default();
    let other_upstream_idx = other_seed_idx
        .map(|i| upstream_indices(other, i, strict))
        .unwrap_or_default();

    let rust_upstream_keys: HashSet<_> = rust_upstream_idx
        .iter()
        .map(|&i| function_key(&rust.functions[i]))
        .collect();
    let other_upstream_keys: HashSet<_> = other_upstream_idx
        .iter()
        .map(|&i| function_key(&other.functions[i]))
        .collect();

    let weights = Weights::default();
    let deviation_by_pair = deviation_map(rust, other, matches, &weights);

    let mut warnings = Vec::new();
    let mut pair_rows = Vec::new();
    let mut seen_pairs: HashSet<(FunctionKey<'_>, FunctionKey<'_>)> = HashSet::new();

    for &ri in &rust_upstream_idx {
        let rf = &rust.functions[ri];
        let rkey = function_key(rf);
        match pair_by_rust.get(&rkey).copied() {
            Some(oi) => {
                let of = &other.functions[oi];
                let okey = function_key(of);
                let overlap = other_upstream_keys.contains(&okey);
                if !overlap {
                    warnings.push(UpstreamWarning {
                        side: "rust".into(),
                        function: function_ref(rf),
                        counterpart: Some(function_ref(of)),
                        message: "mapped counterpart is not upstream on the original-code side".into(),
                    });
                }
                if seen_pairs.insert((rkey, okey)) {
                    pair_rows.push(pair_row(
                        rf,
                        of,
                        pair_strategy(matches, rkey, okey).unwrap_or(MatchStrategy::Mapping),
                        true,
                        overlap,
                        deviation_by_pair.get(&(owned_key(rf), owned_key(of))).cloned(),
                    ));
                }
            }
            None => warnings.push(UpstreamWarning {
                side: "rust".into(),
                function: function_ref(rf),
                counterpart: None,
                message: "no counterpart exists in the 1:1 pairing table".into(),
            }),
        }
    }

    for &oi in &other_upstream_idx {
        let of = &other.functions[oi];
        let okey = function_key(of);
        match pair_by_other.get(&okey).copied() {
            Some(ri) => {
                let rf = &rust.functions[ri];
                let rkey = function_key(rf);
                let overlap = rust_upstream_keys.contains(&rkey);
                if !overlap {
                    warnings.push(UpstreamWarning {
                        side: "other".into(),
                        function: function_ref(of),
                        counterpart: Some(function_ref(rf)),
                        message: "mapped counterpart is not upstream on the Rust side".into(),
                    });
                }
                if seen_pairs.insert((rkey, okey)) {
                    pair_rows.push(pair_row(
                        rf,
                        of,
                        pair_strategy(matches, rkey, okey).unwrap_or(MatchStrategy::Mapping),
                        overlap,
                        true,
                        deviation_by_pair.get(&(owned_key(rf), owned_key(of))).cloned(),
                    ));
                }
            }
            None => warnings.push(UpstreamWarning {
                side: "other".into(),
                function: function_ref(of),
                counterpart: None,
                message: "no counterpart exists in the 1:1 pairing table".into(),
            }),
        }
    }

    warnings.sort_by(|a, b| {
        a.side
            .cmp(&b.side)
            .then(a.function.file.cmp(&b.function.file))
            .then(a.function.line_start.cmp(&b.function.line_start))
            .then(a.function.name.cmp(&b.function.name))
    });
    pair_rows.sort_by(|a, b| {
        a.overlap
            .cmp(&b.overlap)
            .then_with(|| b.total.partial_cmp(&a.total).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.rust.file.cmp(&b.rust.file))
            .then(a.rust.line_start.cmp(&b.rust.line_start))
            .then(a.rust.name.cmp(&b.rust.name))
    });

    Ok(UpstreamAnalysis {
        rust_seed: rust_seed_idx.map(|i| function_ref(&rust.functions[i])),
        other_seed: other_seed_idx.map(|i| function_ref(&other.functions[i])),
        rust_upstream: sorted_refs(rust, &rust_upstream_idx),
        other_upstream: sorted_refs(other, &other_upstream_idx),
        warnings,
        pairs: pair_rows,
    })
}

fn sorted_refs(report: &Report, idxs: &[usize]) -> Vec<FunctionRef> {
    let mut refs: Vec<_> = idxs.iter().map(|&i| function_ref(&report.functions[i])).collect();
    refs.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line_start.cmp(&b.line_start))
            .then(a.name.cmp(&b.name))
    });
    refs
}

fn pair_row(
    rust: &FunctionAnalysis,
    other: &FunctionAnalysis,
    strategy: MatchStrategy,
    rust_in_upstream: bool,
    other_in_upstream: bool,
    deviation: Option<PairDeviation>,
) -> UpstreamPairRow {
    let deviation = deviation.unwrap_or_else(|| fallback_deviation(rust, other));
    UpstreamPairRow {
        rust: function_ref(rust),
        other: function_ref(other),
        match_strategy: strategy,
        rust_in_upstream,
        other_in_upstream,
        overlap: rust_in_upstream && other_in_upstream,
        total: deviation.total,
        per_metric: deviation.per_metric,
    }
}

#[derive(Debug, Clone)]
struct PairDeviation {
    total: f64,
    per_metric: Vec<(String, f64, f64, f64)>,
}

fn fallback_deviation(rust: &FunctionAnalysis, other: &FunctionAnalysis) -> PairDeviation {
    let mut per = Vec::new();
    let mut total = 0.0;
    for ((k, rv), (_, ov)) in metric_vector(&rust.metrics)
        .iter()
        .zip(metric_vector(&other.metrics).iter())
    {
        let contrib = (rv - ov).abs();
        total += contrib;
        per.push((k.to_string(), *rv, *ov, contrib));
    }
    per.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    PairDeviation { total, per_metric: per }
}

fn pair_maps<'a>(
    matches: &'a MatchResult<'a>,
    rust_index_by_key: &HashMap<FunctionKey<'a>, usize>,
    other_index_by_key: &HashMap<FunctionKey<'a>, usize>,
) -> (
    HashMap<FunctionKey<'a>, usize>,
    HashMap<FunctionKey<'a>, usize>,
) {
    let mut by_rust = HashMap::new();
    let mut by_other = HashMap::new();
    for p in &matches.pairs {
        let rkey = function_key(p.rust);
        let okey = function_key(p.other);
        if let (Some(&ri), Some(&oi)) = (rust_index_by_key.get(&rkey), other_index_by_key.get(&okey)) {
            by_rust.insert(rkey, oi);
            by_other.insert(okey, ri);
        }
    }
    (by_rust, by_other)
}

fn pair_strategy(
    matches: &MatchResult<'_>,
    rust_key: FunctionKey<'_>,
    other_key: FunctionKey<'_>,
) -> Option<MatchStrategy> {
    matches
        .pairs
        .iter()
        .find(|p| function_key(p.rust) == rust_key && function_key(p.other) == other_key)
        .map(|p| p.strategy)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OwnedKey {
    name: String,
    file: String,
    line_start: u32,
}

fn owned_key(f: &FunctionAnalysis) -> OwnedKey {
    OwnedKey {
        name: f.name.clone(),
        file: f.location.file.to_string_lossy().into_owned(),
        line_start: f.location.line_start,
    }
}

fn deviation_map(
    rust: &Report,
    other: &Report,
    matches: &MatchResult<'_>,
    weights: &Weights,
) -> HashMap<(OwnedKey, OwnedKey), PairDeviation> {
    let mut samples: HashMap<&'static str, Vec<f64>> = HashMap::new();
    for f in rust.functions.iter().chain(other.functions.iter()) {
        for (k, v) in metric_vector(&f.metrics) {
            samples.entry(k).or_default().push(v);
        }
    }
    let mut scale: HashMap<&'static str, f64> = HashMap::new();
    for (k, mut values) in samples {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((values.len() as f64 - 1.0) * 0.95).round() as usize;
        scale.insert(k, values[idx.min(values.len() - 1)].max(1.0));
    }

    let mut out = HashMap::new();
    for pair in &matches.pairs {
        let mut per_metric = Vec::new();
        let mut total = 0.0;
        for ((k, rv), (_, ov)) in metric_vector(&pair.rust.metrics)
            .iter()
            .zip(metric_vector(&pair.other.metrics).iter())
        {
            let weight = *weights.per_metric.get(*k).unwrap_or(&1.0);
            let denom = *scale.get(k).unwrap_or(&1.0);
            let contrib = weight * (rv - ov).abs() / denom;
            total += contrib;
            per_metric.push((k.to_string(), *rv, *ov, contrib));
        }
        per_metric.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        out.insert(
            (owned_key(pair.rust), owned_key(pair.other)),
            PairDeviation { total, per_metric },
        );
    }
    out
}

fn resolve_seed(
    side: &str,
    report: &Report,
    selector: Option<&FunctionSelector>,
) -> Result<Option<usize>> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    if selector.is_empty() {
        return Ok(None);
    }
    let matches: Vec<usize> = report
        .functions
        .iter()
        .enumerate()
        .filter(|(_, f)| selector.name.as_ref().is_none_or(|name| &f.name == name))
        .filter(|(_, f)| path_suffix_matches(&f.location.file, selector.path.as_deref()))
        .filter(|(_, f)| selector.line.is_none_or(|line| f.location.line_start == line))
        .filter(|(_, f)| class_matches(f.enclosing_type.as_deref(), selector.class.as_deref()))
        .map(|(i, _)| i)
        .collect();

    match matches.as_slice() {
        [] => Err(anyhow!("no {} seed function matched the supplied selector", side)),
        [idx] => Ok(Some(*idx)),
        many => {
            let preview = many
                .iter()
                .take(5)
                .map(|idx| {
                    let f = &report.functions[*idx];
                    format!(
                        "{} @ {}:{}",
                        f.name,
                        f.location.file.display(),
                        f.location.line_start
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!(
                "selector for {} is ambiguous ({} matches): {}",
                side,
                many.len(),
                preview
            ))
        }
    }
}

fn upstream_indices(report: &Report, seed_idx: usize, strict: bool) -> Vec<usize> {
    let graph = order::build_call_graph(report, strict);
    let mut reverse = vec![Vec::new(); graph.nodes.len()];
    for (src, dsts) in graph.edges.iter().enumerate() {
        for &dst in dsts {
            reverse[dst].push(src);
        }
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(seed_idx);
    while let Some(cur) = queue.pop_front() {
        for &pred in &reverse[cur] {
            if visited.insert(pred) {
                queue.push_back(pred);
            }
        }
    }

    let mut out: Vec<_> = visited.into_iter().collect();
    out.sort_by(|a, b| {
        let fa = &report.functions[*a];
        let fb = &report.functions[*b];
        fa.location
            .file
            .cmp(&fb.location.file)
            .then(fa.location.line_start.cmp(&fb.location.line_start))
            .then(fa.name.cmp(&fb.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::match_reports;
    use crate::compare::matching::{Mapping, MappingEntry};
    use crate::core::{Halstead, Language, Location, Metrics, Signature};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fa(name: &str, file: &str, line: u32, calls: &[&str], loc: u32) -> FunctionAnalysis {
        FunctionAnalysis {
            name: name.into(),
            original_name: None,
            mangled: None,
            enclosing_type: None,
            location: Location {
                file: PathBuf::from(file),
                line_start: line,
                line_end: line + 1,
                col_start: 0,
                col_end: 0,
                byte_start: 0,
                byte_end: 0,
            },
            signature: Signature::default(),
            metrics: Metrics {
                loc_code: loc,
                halstead: Halstead::default(),
                ..Default::default()
            },
            constants: Vec::new(),
            calls: calls
                .iter()
                .map(|callee| crate::core::Call {
                    callee: (*callee).into(),
                    count: 1,
                    span: (0, 0),
                })
                .collect(),
            types_used: Vec::new(),
            attributes: BTreeMap::new(),
        }
    }

    fn rep(language: Language, functions: Vec<FunctionAnalysis>) -> Report {
        Report {
            schema_version: crate::core::SCHEMA_VERSION,
            language,
            source_file: PathBuf::from("/tmp/test"),
            source_hash: "0".into(),
            functions,
            structs: Vec::new(),
        }
    }

    #[test]
    fn upstream_auto_resolves_other_seed_from_rust_seed() {
        let rust = rep(
            Language::Rust,
            vec![
                fa("root", "src/root.rs", 1, &["helper"], 30),
                fa("helper", "src/helper.rs", 10, &["leaf"], 20),
                fa("leaf", "src/leaf.rs", 20, &[], 5),
            ],
        );
        let other = rep(
            Language::C,
            vec![
                fa("root_c", "src/root.c", 1, &["helper_c"], 22),
                fa("helper_c", "src/helper.c", 10, &["leaf_c"], 18),
                fa("leaf_c", "src/leaf.c", 20, &[], 6),
            ],
        );
        let mapping = Mapping {
            entries: vec![
                MappingEntry { rust: "root".into(), other: "root_c".into(), ..Default::default() },
                MappingEntry { rust: "helper".into(), other: "helper_c".into(), ..Default::default() },
                MappingEntry { rust: "leaf".into(), other: "leaf_c".into(), ..Default::default() },
            ],
        };
        let matches = match_reports(&rust, &other, Some(&mapping));

        let analysis = analyze_upstream(
            &rust,
            &other,
            &matches,
            Some(&FunctionSelector { name: Some("leaf".into()), ..Default::default() }),
            None,
            false,
        )
        .unwrap();

        assert_eq!(analysis.other_seed.as_ref().unwrap().name, "leaf_c");
        assert_eq!(
            analysis
                .rust_upstream
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["helper", "root"]
        );
        assert_eq!(
            analysis
                .other_upstream
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["helper_c", "root_c"]
        );
        assert!(analysis.warnings.is_empty());
        assert!(analysis.pairs.iter().all(|p| p.overlap));
    }

    #[test]
    fn upstream_flags_non_overlapping_counterparts() {
        let rust = rep(
            Language::Rust,
            vec![
                fa("rust_only_parent", "src/a.rs", 1, &["leaf"], 100),
                fa("rust_peer", "src/b.rs", 5, &[], 9),
                fa("leaf", "src/leaf.rs", 20, &[], 5),
            ],
        );
        let other = rep(
            Language::C,
            vec![
                fa("helper_c", "src/a.c", 1, &[], 8),
                fa("other_only_parent", "src/b.c", 5, &["leaf_c"], 50),
                fa("leaf_c", "src/leaf.c", 20, &[], 5),
            ],
        );
        let mapping = Mapping {
            entries: vec![
                MappingEntry {
                    rust: "rust_only_parent".into(),
                    other: "helper_c".into(),
                    ..Default::default()
                },
                MappingEntry {
                    rust: "rust_peer".into(),
                    other: "other_only_parent".into(),
                    ..Default::default()
                },
                MappingEntry { rust: "leaf".into(), other: "leaf_c".into(), ..Default::default() },
            ],
        };
        let matches = match_reports(&rust, &other, Some(&mapping));

        let analysis = analyze_upstream(
            &rust,
            &other,
            &matches,
            Some(&FunctionSelector { name: Some("leaf".into()), ..Default::default() }),
            Some(&FunctionSelector { name: Some("leaf_c".into()), ..Default::default() }),
            false,
        )
        .unwrap();

        assert_eq!(analysis.warnings.len(), 2);
        assert_eq!(analysis.pairs.len(), 2);
        assert!(!analysis.pairs[0].overlap);
        assert!(!analysis.pairs[1].overlap);
        assert!(analysis.pairs[0].total >= analysis.pairs[1].total);
    }
}
