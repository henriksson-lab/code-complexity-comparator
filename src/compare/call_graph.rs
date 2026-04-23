use crate::compare::matching::{MatchResult, MatchStrategy};
use crate::compare::upstream::FunctionRef;
use crate::core::{FunctionAnalysis, Report};
use crate::order::{self, SccKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdgeRef {
    pub src: FunctionRef,
    pub dst: FunctionRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphDiffSummary {
    pub matched_pairs: usize,
    pub translated_edges_in_rust: usize,
    pub translated_edges_in_other: usize,
    pub edges_only_in_rust: usize,
    pub edges_only_in_other: usize,
    pub recursive_kind_mismatches: usize,
    pub scc_size_mismatches: usize,
    pub rust_ambiguous_call_sites: usize,
    pub other_ambiguous_call_sites: usize,
    pub rust_unresolved_call_sites: usize,
    pub other_unresolved_call_sites: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphPairDiffRow {
    pub rust: FunctionRef,
    pub other: FunctionRef,
    pub match_strategy: MatchStrategy,
    pub total: f64,
    pub rust_callers: usize,
    pub other_callers: usize,
    pub rust_callees: usize,
    pub other_callees: usize,
    pub only_in_rust_callers: Vec<FunctionRef>,
    pub only_in_other_callers: Vec<FunctionRef>,
    pub only_in_rust_callees: Vec<FunctionRef>,
    pub only_in_other_callees: Vec<FunctionRef>,
    pub rust_recursive_kind: String,
    pub other_recursive_kind: String,
    pub rust_scc_size: usize,
    pub other_scc_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphDiffAnalysis {
    pub summary: CallGraphDiffSummary,
    pub edges_only_in_rust: Vec<GraphEdgeRef>,
    pub edges_only_in_other: Vec<GraphEdgeRef>,
    pub pairs: Vec<CallGraphPairDiffRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FunctionKey<'a> {
    name: &'a str,
    file: &'a std::path::Path,
    line_start: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EdgeKey(usize, usize);

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

fn reverse_edges(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut reverse = vec![Vec::new(); edges.len()];
    for (src, dsts) in edges.iter().enumerate() {
        for &dst in dsts {
            reverse[dst].push(src);
        }
    }
    for preds in &mut reverse {
        preds.sort_unstable();
        preds.dedup();
    }
    reverse
}

fn scc_metadata(graph: &order::CallGraph) -> (Vec<SccKind>, Vec<usize>) {
    let comps = order::tarjan_scc(graph);
    let mut kind_by_idx = vec![SccKind::None; graph.nodes.len()];
    let mut size_by_idx = vec![1usize; graph.nodes.len()];
    for comp in comps {
        let kind = if comp.len() > 1 {
            SccKind::Mutual
        } else if graph.has_self_loop[comp[0]] {
            SccKind::SelfLoop
        } else {
            SccKind::None
        };
        let size = comp.len();
        for idx in comp {
            kind_by_idx[idx] = kind;
            size_by_idx[idx] = size;
        }
    }
    (kind_by_idx, size_by_idx)
}

fn kind_name(kind: SccKind) -> String {
    match kind {
        SccKind::None => "none".into(),
        SccKind::SelfLoop => "self".into(),
        SccKind::Mutual => "mutual".into(),
    }
}

pub fn analyze_call_graph_diff(
    rust: &Report,
    other: &Report,
    matches: &MatchResult<'_>,
    strict: bool,
) -> CallGraphDiffAnalysis {
    let rust_graph = order::build_call_graph(rust, strict);
    let other_graph = order::build_call_graph(other, strict);
    let rust_reverse = reverse_edges(&rust_graph.edges);
    let other_reverse = reverse_edges(&other_graph.edges);
    let (rust_kind, rust_scc_size) = scc_metadata(&rust_graph);
    let (other_kind, other_scc_size) = scc_metadata(&other_graph);

    let rust_index_by_key = index_by_key(rust);
    let other_index_by_key = index_by_key(other);
    let mut pair_by_rust: HashMap<usize, usize> = HashMap::new();
    let mut pair_by_other: HashMap<usize, usize> = HashMap::new();
    for (pair_idx, pair) in matches.pairs.iter().enumerate() {
        if let Some(&ri) = rust_index_by_key.get(&function_key(pair.rust)) {
            pair_by_rust.insert(ri, pair_idx);
        }
        if let Some(&oi) = other_index_by_key.get(&function_key(pair.other)) {
            pair_by_other.insert(oi, pair_idx);
        }
    }

    let rust_pair_edges = translated_edges(&rust_graph.edges, &pair_by_rust);
    let other_pair_edges = translated_edges(&other_graph.edges, &pair_by_other);

    let only_in_rust_edge_keys: Vec<_> = rust_pair_edges
        .difference(&other_pair_edges)
        .copied()
        .collect();
    let only_in_other_edge_keys: Vec<_> = other_pair_edges
        .difference(&rust_pair_edges)
        .copied()
        .collect();

    let mut pairs = Vec::new();
    let mut recursive_kind_mismatches = 0usize;
    let mut scc_size_mismatches = 0usize;

    for (pair_idx, pair) in matches.pairs.iter().enumerate() {
        let ri = *rust_index_by_key.get(&function_key(pair.rust)).unwrap();
        let oi = *other_index_by_key.get(&function_key(pair.other)).unwrap();

        let rust_callee_pairs = translated_neighbors(&rust_graph.edges[ri], &pair_by_rust);
        let other_callee_pairs = translated_neighbors(&other_graph.edges[oi], &pair_by_other);
        let rust_caller_pairs = translated_neighbors(&rust_reverse[ri], &pair_by_rust);
        let other_caller_pairs = translated_neighbors(&other_reverse[oi], &pair_by_other);

        let only_in_rust_callees = pair_refs(
            &difference(&rust_callee_pairs, &other_callee_pairs),
            matches,
            Side::Rust,
        );
        let only_in_other_callees = pair_refs(
            &difference(&other_callee_pairs, &rust_callee_pairs),
            matches,
            Side::Other,
        );
        let only_in_rust_callers = pair_refs(
            &difference(&rust_caller_pairs, &other_caller_pairs),
            matches,
            Side::Rust,
        );
        let only_in_other_callers = pair_refs(
            &difference(&other_caller_pairs, &rust_caller_pairs),
            matches,
            Side::Other,
        );

        let recursive_kind_mismatch = rust_kind[ri] != other_kind[oi];
        let scc_size_mismatch = rust_scc_size[ri] != other_scc_size[oi];
        if recursive_kind_mismatch {
            recursive_kind_mismatches += 1;
        }
        if scc_size_mismatch {
            scc_size_mismatches += 1;
        }

        let total = (only_in_rust_callers.len()
            + only_in_other_callers.len()
            + only_in_rust_callees.len()
            + only_in_other_callees.len()) as f64
            + if recursive_kind_mismatch { 2.0 } else { 0.0 }
            + ((rust_scc_size[ri] as isize - other_scc_size[oi] as isize).abs() as f64 * 0.5);

        pairs.push(CallGraphPairDiffRow {
            rust: function_ref(pair.rust),
            other: function_ref(pair.other),
            match_strategy: pair.strategy,
            total,
            rust_callers: rust_caller_pairs.len(),
            other_callers: other_caller_pairs.len(),
            rust_callees: rust_callee_pairs.len(),
            other_callees: other_callee_pairs.len(),
            only_in_rust_callers,
            only_in_other_callers,
            only_in_rust_callees,
            only_in_other_callees,
            rust_recursive_kind: kind_name(rust_kind[ri]),
            other_recursive_kind: kind_name(other_kind[oi]),
            rust_scc_size: rust_scc_size[ri],
            other_scc_size: other_scc_size[oi],
        });
        let _ = pair_idx;
    }

    pairs.sort_by(|a, b| {
        b.total
            .partial_cmp(&a.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rust.file.cmp(&b.rust.file))
            .then(a.rust.line_start.cmp(&b.rust.line_start))
            .then(a.rust.name.cmp(&b.rust.name))
    });

    CallGraphDiffAnalysis {
        summary: CallGraphDiffSummary {
            matched_pairs: matches.pairs.len(),
            translated_edges_in_rust: rust_pair_edges.len(),
            translated_edges_in_other: other_pair_edges.len(),
            edges_only_in_rust: only_in_rust_edge_keys.len(),
            edges_only_in_other: only_in_other_edge_keys.len(),
            recursive_kind_mismatches,
            scc_size_mismatches,
            rust_ambiguous_call_sites: rust_graph.ambiguous_call_sites,
            other_ambiguous_call_sites: other_graph.ambiguous_call_sites,
            rust_unresolved_call_sites: rust_graph.unresolved_call_sites,
            other_unresolved_call_sites: other_graph.unresolved_call_sites,
        },
        edges_only_in_rust: edge_refs(&only_in_rust_edge_keys, matches, Side::Rust),
        edges_only_in_other: edge_refs(&only_in_other_edge_keys, matches, Side::Other),
        pairs,
    }
}

fn translated_edges(edges: &[Vec<usize>], pair_by_side: &HashMap<usize, usize>) -> HashSet<EdgeKey> {
    let mut out = HashSet::new();
    for (&src_idx, &src_pair) in pair_by_side {
        for &dst_idx in &edges[src_idx] {
            if let Some(&dst_pair) = pair_by_side.get(&dst_idx) {
                out.insert(EdgeKey(src_pair, dst_pair));
            }
        }
    }
    out
}

fn translated_neighbors(neighbors: &[usize], pair_by_side: &HashMap<usize, usize>) -> HashSet<usize> {
    neighbors
        .iter()
        .filter_map(|idx| pair_by_side.get(idx).copied())
        .collect()
}

fn difference(a: &HashSet<usize>, b: &HashSet<usize>) -> Vec<usize> {
    let mut out: Vec<_> = a.difference(b).copied().collect();
    out.sort_unstable();
    out
}

#[derive(Debug, Clone, Copy)]
enum Side {
    Rust,
    Other,
}

fn pair_refs(pair_ids: &[usize], matches: &MatchResult<'_>, side: Side) -> Vec<FunctionRef> {
    pair_ids
        .iter()
        .map(|idx| match side {
            Side::Rust => function_ref(matches.pairs[*idx].rust),
            Side::Other => function_ref(matches.pairs[*idx].other),
        })
        .collect()
}

fn edge_refs(edge_keys: &[EdgeKey], matches: &MatchResult<'_>, side: Side) -> Vec<GraphEdgeRef> {
    let mut out: Vec<_> = edge_keys
        .iter()
        .map(|edge| {
            let src = match side {
                Side::Rust => function_ref(matches.pairs[edge.0].rust),
                Side::Other => function_ref(matches.pairs[edge.0].other),
            };
            let dst = match side {
                Side::Rust => function_ref(matches.pairs[edge.1].rust),
                Side::Other => function_ref(matches.pairs[edge.1].other),
            };
            GraphEdgeRef { src, dst }
        })
        .collect();
    out.sort_by(|a, b| {
        a.src.file
            .cmp(&b.src.file)
            .then(a.src.line_start.cmp(&b.src.line_start))
            .then(a.src.name.cmp(&b.src.name))
            .then(a.dst.file.cmp(&b.dst.file))
            .then(a.dst.line_start.cmp(&b.dst.line_start))
            .then(a.dst.name.cmp(&b.dst.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::match_reports;
    use crate::compare::matching::{Mapping, MappingEntry};
    use crate::core::{Call, Halstead, Language, Location, Metrics, Report, Signature};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fa(name: &str, file: &str, line: u32, calls: &[&str]) -> FunctionAnalysis {
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
                halstead: Halstead::default(),
                ..Default::default()
            },
            constants: Vec::new(),
            calls: calls
                .iter()
                .map(|callee| Call {
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
    fn call_graph_diff_flags_edge_rewiring() {
        let rust = rep(
            Language::Rust,
            vec![
                fa("a", "src/a.rs", 1, &["b"]),
                fa("b", "src/b.rs", 10, &["c"]),
                fa("c", "src/c.rs", 20, &[]),
            ],
        );
        let other = rep(
            Language::C,
            vec![
                fa("a_c", "src/a.c", 1, &["c_c"]),
                fa("b_c", "src/b.c", 10, &[]),
                fa("c_c", "src/c.c", 20, &[]),
            ],
        );
        let mapping = Mapping {
            entries: vec![
                MappingEntry { rust: "a".into(), other: "a_c".into(), ..Default::default() },
                MappingEntry { rust: "b".into(), other: "b_c".into(), ..Default::default() },
                MappingEntry { rust: "c".into(), other: "c_c".into(), ..Default::default() },
            ],
        };
        let matches = match_reports(&rust, &other, Some(&mapping));

        let analysis = analyze_call_graph_diff(&rust, &other, &matches, false);
        assert_eq!(analysis.summary.edges_only_in_rust, 2);
        assert_eq!(analysis.summary.edges_only_in_other, 1);
        assert_eq!(analysis.pairs[0].rust.name, "a");
        assert!(analysis.pairs[0].total > 0.0);
    }

    #[test]
    fn call_graph_diff_flags_recursive_shape_mismatches() {
        let rust = rep(
            Language::Rust,
            vec![
                fa("a", "src/a.rs", 1, &["a"]),
                fa("b", "src/b.rs", 10, &[]),
            ],
        );
        let other = rep(
            Language::C,
            vec![
                fa("a_c", "src/a.c", 1, &["b_c"]),
                fa("b_c", "src/b.c", 10, &["a_c"]),
            ],
        );
        let mapping = Mapping {
            entries: vec![
                MappingEntry { rust: "a".into(), other: "a_c".into(), ..Default::default() },
                MappingEntry { rust: "b".into(), other: "b_c".into(), ..Default::default() },
            ],
        };
        let matches = match_reports(&rust, &other, Some(&mapping));

        let analysis = analyze_call_graph_diff(&rust, &other, &matches, false);
        assert_eq!(analysis.summary.recursive_kind_mismatches, 2);
        assert_eq!(analysis.summary.scc_size_mismatches, 2);
        assert!(analysis.pairs.iter().all(|p| p.total >= 2.0));
    }
}
