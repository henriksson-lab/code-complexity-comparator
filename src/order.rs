//! Bottom-up ordering of functions for porting.
//!
//! Builds a call graph from a `Report`, runs Tarjan's SCC to condense mutually
//! recursive groups, and emits functions in reverse topological order of the
//! condensation — callees before callers. This is the order you want to
//! re-implement the code in when porting (e.g. C -> Rust): every function
//! can be translated after its dependencies are already in place.
//!
//! Output is a simple CSV so the user can mark progress by flipping a
//! `translated` column. A companion `order-annotate` step joins the CSV
//! against a Rust report to surface the existing Rust counterparts.
//!
//! Callee resolution is by name only — the tool's `Call.callee` strings are
//! reduced to the bare identifier (see the README under "Known limitations"),
//! so same-named functions in different files cause ambiguity. By default
//! ambiguous calls add edges to every candidate (a safe over-approximation
//! for ordering: it can only pull a dependency earlier, never later). With
//! `strict = true`, ambiguous calls are dropped instead.
use anyhow::{anyhow, Result};
use crate::compare::{match_reports, Mapping, MatchResult, Pair};
use crate::core::Report;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub name: String,
    pub file: PathBuf,
    pub line_start: u32,
}

pub struct CallGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<Vec<usize>>,
    pub has_self_loop: Vec<bool>,
    pub ambiguous_call_sites: usize,
    pub unresolved_call_sites: usize,
}

pub fn build_call_graph(report: &Report, strict: bool) -> CallGraph {
    let nodes: Vec<GraphNode> = report
        .functions
        .iter()
        .map(|f| GraphNode {
            name: f.name.clone(),
            file: f.location.file.clone(),
            line_start: f.location.line_start,
        })
        .collect();

    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in report.functions.iter().enumerate() {
        by_name.entry(f.name.as_str()).or_default().push(i);
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut has_self_loop = vec![false; nodes.len()];
    let mut ambiguous = 0usize;
    let mut unresolved = 0usize;

    for (i, f) in report.functions.iter().enumerate() {
        let mut dsts: BTreeSet<usize> = BTreeSet::new();
        for call in &f.calls {
            match by_name.get(call.callee.as_str()) {
                None => unresolved += 1,
                Some(v) if v.len() == 1 => {
                    dsts.insert(v[0]);
                }
                Some(v) => {
                    ambiguous += 1;
                    if !strict {
                        for &j in v {
                            dsts.insert(j);
                        }
                    }
                }
            }
        }
        if dsts.contains(&i) {
            has_self_loop[i] = true;
        }
        edges[i] = dsts.into_iter().collect();
    }

    CallGraph {
        nodes,
        edges,
        has_self_loop,
        ambiguous_call_sites: ambiguous,
        unresolved_call_sites: unresolved,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SccKind {
    None,
    SelfLoop,
    Mutual,
}

pub fn scc_kind_str(k: SccKind) -> &'static str {
    match k {
        SccKind::None => "",
        SccKind::SelfLoop => "self",
        SccKind::Mutual => "mutual",
    }
}

#[derive(Debug, Clone)]
pub struct OrderedFunction {
    pub index: usize,
    pub scc_id: Option<u32>,
    pub scc_kind: SccKind,
}

/// Iterative Tarjan's SCC. Returns the list of strongly-connected components
/// in the order they are completed, which is reverse topological order on the
/// condensation DAG — i.e. for a call graph, sinks (functions that call
/// nothing outside their SCC) come first. That is the bottom-up porting order.
pub fn tarjan_scc(g: &CallGraph) -> Vec<Vec<usize>> {
    let n = g.nodes.len();
    let mut index_of: Vec<i64> = vec![-1; n];
    let mut lowlink: Vec<i64> = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut comps: Vec<Vec<usize>> = Vec::new();
    let mut next_index: i64 = 0;

    struct Frame {
        node: usize,
        ci: usize,
    }

    for root in 0..n {
        if index_of[root] != -1 {
            continue;
        }
        let mut frames: Vec<Frame> = Vec::new();
        index_of[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        frames.push(Frame { node: root, ci: 0 });

        while let Some(top) = frames.last_mut() {
            let v = top.node;
            let ci = top.ci;
            if ci < g.edges[v].len() {
                top.ci += 1;
                let w = g.edges[v][ci];
                if index_of[w] == -1 {
                    index_of[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push(Frame { node: w, ci: 0 });
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index_of[w]);
                }
            } else {
                let v_low = lowlink[v];
                if v_low == index_of[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    comps.push(comp);
                }
                frames.pop();
                if let Some(parent) = frames.last_mut() {
                    lowlink[parent.node] = lowlink[parent.node].min(v_low);
                }
            }
        }
    }
    comps
}

pub fn order_bottom_up(g: &CallGraph) -> Vec<OrderedFunction> {
    let comps = tarjan_scc(g);
    let mut out: Vec<OrderedFunction> = Vec::with_capacity(g.nodes.len());
    let mut scc_counter: u32 = 0;
    for comp in comps {
        let mut members = comp;
        members.sort_by(|a, b| {
            let na = &g.nodes[*a];
            let nb = &g.nodes[*b];
            na.file
                .cmp(&nb.file)
                .then(na.line_start.cmp(&nb.line_start))
                .then(na.name.cmp(&nb.name))
        });
        let kind = if members.len() > 1 {
            SccKind::Mutual
        } else if g.has_self_loop[members[0]] {
            SccKind::SelfLoop
        } else {
            SccKind::None
        };
        let scc_id = if kind == SccKind::None {
            None
        } else {
            scc_counter += 1;
            Some(scc_counter)
        };
        for m in members {
            out.push(OrderedFunction {
                index: m,
                scc_id,
                scc_kind: kind,
            });
        }
    }
    out
}

// ---------------- CSV ----------------

pub fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let esc = s.replace('"', "\"\"");
        format!("\"{}\"", esc)
    } else {
        s.to_string()
    }
}

/// Minimal RFC-4180 CSV parser. CRLF is normalized to LF up front.
pub fn parse_csv(s: &str) -> Vec<Vec<String>> {
    let normalized: String = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_field = String::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut in_quote = false;
    let mut field_started = false;
    let mut chars = normalized.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur_field.push('"');
                } else {
                    in_quote = false;
                }
            } else {
                cur_field.push(c);
            }
        } else {
            match c {
                ',' => {
                    cur_row.push(std::mem::take(&mut cur_field));
                    field_started = false;
                }
                '\n' => {
                    cur_row.push(std::mem::take(&mut cur_field));
                    rows.push(std::mem::take(&mut cur_row));
                    field_started = false;
                }
                '"' if !field_started => {
                    in_quote = true;
                    field_started = true;
                }
                _ => {
                    cur_field.push(c);
                    field_started = true;
                }
            }
        }
    }
    if field_started || !cur_row.is_empty() {
        cur_row.push(cur_field);
        rows.push(cur_row);
    }
    rows
}

#[derive(Debug, Clone)]
pub struct CsvFile {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn read_csv(path: &Path) -> Result<CsvFile> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
    let mut all = parse_csv(&s);
    let headers = if all.is_empty() { Vec::new() } else { all.remove(0) };
    Ok(CsvFile { headers, rows: all })
}

/// Read a previous order CSV and build a (name, file) -> translated-value map
/// so a re-run of `order` can preserve progress flags across call-graph shifts.
pub fn read_translated_map(path: &Path) -> Result<HashMap<(String, String), String>> {
    let csv = read_csv(path)?;
    let name_idx = csv
        .headers
        .iter()
        .position(|h| h == "name")
        .ok_or_else(|| anyhow!("no `name` column in {}", path.display()))?;
    let file_idx = csv
        .headers
        .iter()
        .position(|h| h == "file")
        .ok_or_else(|| anyhow!("no `file` column in {}", path.display()))?;
    let trans_idx = csv
        .headers
        .iter()
        .position(|h| h == "translated")
        .ok_or_else(|| anyhow!("no `translated` column in {}", path.display()))?;
    let max_idx = name_idx.max(file_idx).max(trans_idx);
    let mut m = HashMap::new();
    for row in csv.rows {
        if row.len() <= max_idx {
            continue;
        }
        let key = (row[name_idx].clone(), row[file_idx].clone());
        m.insert(key, row[trans_idx].clone());
    }
    Ok(m)
}

pub fn render_order_csv(
    g: &CallGraph,
    order: &[OrderedFunction],
    prev_translated: &HashMap<(String, String), String>,
) -> String {
    let mut out = String::new();
    out.push_str("name,file,line_start,scc_id,scc_kind,translated\n");
    for of in order {
        let n = &g.nodes[of.index];
        let file_str = n.file.to_string_lossy().to_string();
        let scc_id_s = of.scc_id.map(|x| x.to_string()).unwrap_or_default();
        let translated = prev_translated
            .get(&(n.name.clone(), file_str.clone()))
            .cloned()
            .unwrap_or_else(|| "FALSE".to_string());
        out.push_str(&csv_escape(&n.name));
        out.push(',');
        out.push_str(&csv_escape(&file_str));
        out.push(',');
        out.push_str(&n.line_start.to_string());
        out.push(',');
        out.push_str(&scc_id_s);
        out.push(',');
        out.push_str(scc_kind_str(of.scc_kind));
        out.push(',');
        out.push_str(&csv_escape(&translated));
        out.push('\n');
    }
    out
}

// ---------------- Annotate ----------------

pub fn render_annotated_csv(
    csv: &CsvFile,
    source: &Report,
    rust: &Report,
    mapping: Option<&Mapping>,
) -> Result<String> {
    let name_idx = csv
        .headers
        .iter()
        .position(|h| h == "name")
        .ok_or_else(|| anyhow!("no `name` column"))?;
    let file_idx = csv
        .headers
        .iter()
        .position(|h| h == "file")
        .ok_or_else(|| anyhow!("no `file` column"))?;
    let line_idx = csv
        .headers
        .iter()
        .position(|h| h == "line_start")
        .ok_or_else(|| anyhow!("no `line_start` column"))?;

    let m: MatchResult = match_reports(rust, source, mapping);

    let mut by_other: HashMap<(String, String, u32), &Pair> = HashMap::new();
    for p in &m.pairs {
        by_other.insert(
            (
                p.other.name.clone(),
                p.other.location.file.to_string_lossy().to_string(),
                p.other.location.line_start,
            ),
            p,
        );
    }

    let annot_cols = ["rust_name", "rust_file", "rust_line_start", "match_strategy"];
    let mut new_headers: Vec<String> = csv.headers.clone();
    for h in annot_cols {
        if !new_headers.iter().any(|x| x == h) {
            new_headers.push(h.to_string());
        }
    }

    let mut out = String::new();
    out.push_str(
        &new_headers
            .iter()
            .map(|h| csv_escape(h))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');

    for row in &csv.rows {
        if row.is_empty() {
            continue;
        }
        let get = |i: usize| -> &str { row.get(i).map(|s| s.as_str()).unwrap_or("") };
        let name = get(name_idx).to_string();
        let file = get(file_idx).to_string();
        let line: u32 = get(line_idx).parse().unwrap_or(0);
        let pair = by_other.get(&(name, file, line));

        let mut cols: Vec<String> = Vec::with_capacity(new_headers.len());
        for h in &new_headers {
            if let Some(i) = csv.headers.iter().position(|x| x == h) {
                cols.push(get(i).to_string());
            } else {
                cols.push(String::new());
            }
        }
        let (rn, rf, rl, strat) = match pair {
            Some(p) => (
                p.rust.name.clone(),
                p.rust.location.file.to_string_lossy().to_string(),
                p.rust.location.line_start.to_string(),
                format!("{:?}", p.strategy),
            ),
            None => (String::new(), String::new(), String::new(), String::new()),
        };
        for (h, v) in [
            ("rust_name", rn),
            ("rust_file", rf),
            ("rust_line_start", rl),
            ("match_strategy", strat),
        ] {
            if let Some(i) = new_headers.iter().position(|x| x == h) {
                cols[i] = v;
            }
        }
        out.push_str(
            &cols
                .iter()
                .map(|c| csv_escape(c))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        Call, FunctionAnalysis, Halstead, Language, Location, Metrics, Report, Signature,
        SCHEMA_VERSION,
    };

    fn mk_fn(name: &str, file: &str, line: u32, calls: &[&str]) -> FunctionAnalysis {
        FunctionAnalysis {
            name: name.into(),
            original_name: None,
            mangled: None,
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
            constants: vec![],
            calls: calls
                .iter()
                .map(|c| Call {
                    callee: (*c).into(),
                    count: 1,
                    span: (0, 0),
                })
                .collect(),
            types_used: vec![],
            attributes: Default::default(),
        }
    }

    fn rep(lang: Language, fns: Vec<FunctionAnalysis>) -> Report {
        Report {
            schema_version: SCHEMA_VERSION,
            language: lang,
            source_file: PathBuf::from("/tmp/x"),
            source_hash: "0".into(),
            functions: fns,
        }
    }

    fn ordered_names(g: &CallGraph, ord: &[OrderedFunction]) -> Vec<String> {
        ord.iter().map(|o| g.nodes[o.index].name.clone()).collect()
    }

    #[test]
    fn simple_chain_is_bottom_up() {
        // a -> b -> c. Bottom-up order: c, b, a.
        let r = rep(
            Language::C,
            vec![
                mk_fn("a", "/x.c", 1, &["b"]),
                mk_fn("b", "/x.c", 10, &["c"]),
                mk_fn("c", "/x.c", 20, &[]),
            ],
        );
        let g = build_call_graph(&r, false);
        let ord = order_bottom_up(&g);
        assert_eq!(ordered_names(&g, &ord), vec!["c", "b", "a"]);
        for of in &ord {
            assert_eq!(of.scc_kind, SccKind::None);
        }
    }

    #[test]
    fn dag_fanout_emits_callees_before_callers() {
        // a -> b, a -> c. b and c come before a.
        let r = rep(
            Language::C,
            vec![
                mk_fn("a", "/x.c", 1, &["b", "c"]),
                mk_fn("b", "/x.c", 10, &[]),
                mk_fn("c", "/x.c", 20, &[]),
            ],
        );
        let g = build_call_graph(&r, false);
        let ord = order_bottom_up(&g);
        let names = ordered_names(&g, &ord);
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(pos("b") < pos("a"));
        assert!(pos("c") < pos("a"));
    }

    #[test]
    fn self_recursion_flagged() {
        let r = rep(Language::C, vec![mk_fn("fib", "/x.c", 1, &["fib"])]);
        let g = build_call_graph(&r, false);
        assert!(g.has_self_loop[0]);
        let ord = order_bottom_up(&g);
        assert_eq!(ord.len(), 1);
        assert_eq!(ord[0].scc_kind, SccKind::SelfLoop);
        assert!(ord[0].scc_id.is_some());
    }

    #[test]
    fn mutual_recursion_grouped_into_scc() {
        // a <-> b, each also calls c. c has no callees.
        let r = rep(
            Language::C,
            vec![
                mk_fn("a", "/x.c", 1, &["b", "c"]),
                mk_fn("b", "/x.c", 10, &["a", "c"]),
                mk_fn("c", "/x.c", 20, &[]),
            ],
        );
        let g = build_call_graph(&r, false);
        let ord = order_bottom_up(&g);
        let names = ordered_names(&g, &ord);
        // c is bottom
        assert_eq!(names[0], "c");
        // a and b form one SCC marked mutual
        let scc_of = |n: &str| {
            ord.iter()
                .find(|o| g.nodes[o.index].name == n)
                .and_then(|o| o.scc_id)
        };
        assert_eq!(scc_of("a"), scc_of("b"));
        assert!(scc_of("a").is_some());
        for of in &ord {
            let kind = of.scc_kind;
            match g.nodes[of.index].name.as_str() {
                "a" | "b" => assert_eq!(kind, SccKind::Mutual),
                "c" => assert_eq!(kind, SccKind::None),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn ambiguous_callee_non_strict_adds_all_edges() {
        // Two functions named `helper` in different files; `caller` calls `helper`.
        let r = rep(
            Language::C,
            vec![
                mk_fn("helper", "/a.c", 1, &[]),
                mk_fn("helper", "/b.c", 1, &[]),
                mk_fn("caller", "/c.c", 1, &["helper"]),
            ],
        );
        let g = build_call_graph(&r, false);
        assert_eq!(g.ambiguous_call_sites, 1);
        let caller_idx = g.nodes.iter().position(|n| n.name == "caller").unwrap();
        assert_eq!(g.edges[caller_idx].len(), 2, "edges go to both helpers");

        let strict = build_call_graph(&r, true);
        assert_eq!(strict.ambiguous_call_sites, 1);
        let caller_idx = strict.nodes.iter().position(|n| n.name == "caller").unwrap();
        assert_eq!(strict.edges[caller_idx].len(), 0, "strict drops ambiguous");
    }

    #[test]
    fn unresolved_callee_is_counted_not_edged() {
        // puts is external — no matching function in the report.
        let r = rep(
            Language::C,
            vec![mk_fn("main", "/x.c", 1, &["puts", "exit"])],
        );
        let g = build_call_graph(&r, false);
        assert_eq!(g.unresolved_call_sites, 2);
        assert!(g.edges[0].is_empty());
    }

    #[test]
    fn csv_escape_roundtrip() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("has,comma"), "\"has,comma\"");
        assert_eq!(csv_escape("has\"quote"), "\"has\"\"quote\"");
        assert_eq!(csv_escape("new\nline"), "\"new\nline\"");
    }

    #[test]
    fn csv_parse_handles_quotes_and_commas_in_fields() {
        let s = "name,file,line_start,scc_id,scc_kind,translated\n\
                 \"fn,with,commas\",\"/x.c\",1,,,FALSE\n\
                 a,\"/y with \"\"quotes\"\".c\",2,,,TRUE\n";
        let rows = parse_csv(s);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][0], "fn,with,commas");
        assert_eq!(rows[2][1], "/y with \"quotes\".c");
        assert_eq!(rows[2][5], "TRUE");
    }

    #[test]
    fn merge_preserves_translated_flag() {
        use std::collections::HashMap;
        let r = rep(
            Language::C,
            vec![
                mk_fn("a", "/x.c", 1, &["b"]),
                mk_fn("b", "/x.c", 10, &[]),
            ],
        );
        let g = build_call_graph(&r, false);
        let ord = order_bottom_up(&g);

        // First render as if fresh.
        let fresh = render_order_csv(&g, &ord, &HashMap::new());
        assert!(fresh.contains(",FALSE\n"));

        // Simulate the user flipping `b` to TRUE.
        let mut prev: HashMap<(String, String), String> = HashMap::new();
        prev.insert(("b".into(), "/x.c".into()), "TRUE".into());
        let merged = render_order_csv(&g, &ord, &prev);
        // b's line should end in TRUE, a's line in FALSE.
        let mut saw_b_true = false;
        let mut saw_a_false = false;
        for line in merged.lines().skip(1) {
            if line.starts_with("b,") && line.ends_with(",TRUE") {
                saw_b_true = true;
            }
            if line.starts_with("a,") && line.ends_with(",FALSE") {
                saw_a_false = true;
            }
        }
        assert!(saw_b_true, "b should carry forward TRUE; got:\n{}", merged);
        assert!(saw_a_false, "a should default to FALSE; got:\n{}", merged);
    }

    #[test]
    fn annotate_joins_rust_columns() {
        // Source (C) report: one function `foo`. Rust report: one function
        // `foo`. Exact-name match should fill the rust_* columns.
        let source = rep(
            Language::C,
            vec![mk_fn("foo", "/src/x.c", 1, &[])],
        );
        let rust = rep(
            Language::Rust,
            vec![mk_fn("foo", "/rs/x.rs", 5, &[])],
        );

        // Simulate the CSV produced by `order` on the C source.
        let g = build_call_graph(&source, false);
        let ord = order_bottom_up(&g);
        let csv_str = render_order_csv(&g, &ord, &HashMap::new());
        let mut all = parse_csv(&csv_str);
        let headers = all.remove(0);
        let csv = CsvFile {
            headers,
            rows: all,
        };

        let annotated = render_annotated_csv(&csv, &source, &rust, None).unwrap();
        let mut rows = parse_csv(&annotated);
        let ahdr = rows.remove(0);
        assert!(ahdr.contains(&"rust_name".to_string()));
        assert!(ahdr.contains(&"rust_file".to_string()));
        assert!(ahdr.contains(&"match_strategy".to_string()));
        let rn_idx = ahdr.iter().position(|h| h == "rust_name").unwrap();
        let rf_idx = ahdr.iter().position(|h| h == "rust_file").unwrap();
        let strat_idx = ahdr.iter().position(|h| h == "match_strategy").unwrap();
        assert_eq!(rows[0][rn_idx], "foo");
        assert_eq!(rows[0][rf_idx], "/rs/x.rs");
        assert_eq!(rows[0][strat_idx], "ExactName");
    }
}
