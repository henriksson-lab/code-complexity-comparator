use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use code_complexity_comparator_rs::analyzer::{LanguageAnalyzer, Registry};
use code_complexity_comparator_rs::compare::{
    analyze_call_graph_diff, analyze_upstream, constants_diff, deviation_rows, load_report,
    match_reports, match_structs, missing, sort_report, struct_deviation_rows, struct_missing,
    FunctionSelector, Mapping, SortKey, Weights,
};
use code_complexity_comparator_rs::core::{Language, Report};
use code_complexity_comparator_rs::lang_c::CAnalyzer;
use code_complexity_comparator_rs::lang_java::JavaAnalyzer;
use code_complexity_comparator_rs::lang_python::PythonAnalyzer;
use code_complexity_comparator_rs::lang_fortran::FortranAnalyzer;
use code_complexity_comparator_rs::lang_perl::PerlAnalyzer;
use code_complexity_comparator_rs::lang_r::RAnalyzer;
use code_complexity_comparator_rs::lang_rust::RustAnalyzer;
use code_complexity_comparator_rs::order;
use code_complexity_comparator_rs::predict::{predict_report, train, Model};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "ccc-rs", about = "Static complexity analysis and cross-language comparison")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Analyze a source file or directory and emit a JSON report.
    Analyze {
        path: PathBuf,
        #[arg(short = 'l', long)]
        lang: Option<LangArg>,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        #[arg(long)]
        recurse: bool,
    },
    /// Compare a rust report against an other-language report, sorted by deviation.
    Compare {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long, default_value = "deviation")]
        sort: SortArg,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Report functions in other but missing from rust, plus partial/stubs.
    Missing {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long, default_value_t = 0.2)]
        stub_loc_ratio: f64,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Sort a single report by complexity.
    Sort {
        report: PathBuf,
        #[arg(long, default_value = "composite")]
        by: String,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Diff constants (magic numbers, strings) per matched function.
    ConstantsDiff {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Compare structs (struct / class / record / derived_type) across two
    /// reports. Matches by name; ranks by a deviation score built from
    /// per-type-category field counts (int/float/pointer/string/…).
    CompareStructs {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Report structs present in `other` but not in `rust` (and vice versa).
    MissingStructs {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Emit functions in bottom-up porting order (callees before callers) as CSV.
    /// Mutually recursive groups are labelled so they can be translated together.
    /// `path` can be a source file, a source directory, or a previously-generated
    /// `report.json`.
    Order {
        path: PathBuf,
        #[arg(short = 'l', long)]
        lang: Option<LangArg>,
        #[arg(long)]
        recurse: bool,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Drop edges for ambiguous callee names instead of conservatively
        /// adding edges to all candidates.
        #[arg(long)]
        strict: bool,
        /// Previous order.csv: carry forward `translated` values for rows
        /// whose (name, file) still appear.
        #[arg(long)]
        merge: Option<PathBuf>,
    },
    /// Annotate an order.csv with the Rust counterpart of each function.
    /// Uses the same matching strategies as `compare` / `missing`.
    OrderAnnotate {
        csv: PathBuf,
        /// The other-language report the CSV was generated from.
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        rust: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Train or apply the prediction model.
    Predict {
        #[command(subcommand)]
        sub: PredictCmd,
    },
    /// Open a split-screen TUI to compare Rust functions against their
    /// original-language counterparts. The mapping file drives the pair list;
    /// file paths are resolved by scanning the given roots (file name must
    /// match, full path is flexible).
    CompareTui {
        #[arg(long, default_value = "ccc_mapping.toml")]
        mapping: PathBuf,
        #[arg(long, default_value = ".")]
        rust_root: PathBuf,
        #[arg(long)]
        other_root: PathBuf,
        /// Auto-detected from the most common non-Rust source extension under
        /// `--other-root` when omitted.
        #[arg(long)]
        other_lang: Option<LangArg>,
    },
    /// Compare the recursive upstream caller sets for a Rust and/or
    /// original-language function, then flag pairing mismatches across the
    /// two sets and rank the translated pairs by complexity mismatch.
    Upstream {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(long = "rust-fn")]
        rust_fn: Option<String>,
        #[arg(long = "rust-path")]
        rust_path: Option<String>,
        #[arg(long = "rust-line")]
        rust_line: Option<u32>,
        #[arg(long = "rust-class")]
        rust_class: Option<String>,
        #[arg(long = "other-fn")]
        other_fn: Option<String>,
        #[arg(long = "other-path")]
        other_path: Option<String>,
        #[arg(long = "other-line")]
        other_line: Option<u32>,
        #[arg(long = "other-class")]
        other_class: Option<String>,
        /// Drop ambiguous call edges instead of conservatively treating every
        /// same-named candidate as a caller/callee.
        #[arg(long)]
        strict: bool,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
    /// Compare translated caller/callee graph structure across matched pairs.
    CallGraphDiff {
        rust: PathBuf,
        other: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
        /// Drop ambiguous call edges instead of conservatively treating every
        /// same-named candidate as a caller/callee.
        #[arg(long)]
        strict: bool,
        #[arg(long, default_value = "table")]
        format: FormatArg,
    },
}

#[derive(Subcommand, Debug)]
enum PredictCmd {
    /// Train a linear + heuristic model from a directory of matched (rust, other) report pairs.
    /// Directory layout: for each base name "x", provide "x.rust.json" and "x.c.json" (or ".cpp.json").
    Train {
        pairs_dir: PathBuf,
        #[arg(long)]
        model: PathBuf,
    },
    /// Apply the model to predict rust metrics from an other-language report.
    Apply {
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        against: Option<PathBuf>,
        #[arg(long)]
        mapping: Option<PathBuf>,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum LangArg {
    C,
    Cpp,
    Rust,
    Java,
    Python,
    R,
    Perl,
    Fortran,
}

impl LangArg {
    fn to_language(self) -> Language {
        match self {
            LangArg::C => Language::C,
            LangArg::Cpp => Language::Cpp,
            LangArg::Rust => Language::Rust,
            LangArg::Java => Language::Java,
            LangArg::Python => Language::Python,
            LangArg::R => Language::R,
            LangArg::Perl => Language::Perl,
            LangArg::Fortran => Language::Fortran,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum FormatArg {
    Table,
    Json,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum SortArg {
    Deviation,
    Name,
}

const DEFAULT_MAPPING: &str = "ccc_mapping.toml";

/// Use the user-supplied path when given; otherwise fall back to
/// `ccc_mapping.toml` in the working directory if it exists. Returning
/// `None` keeps the "no mapping" behavior intact when neither is present.
fn resolve_mapping(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p);
    }
    let default = PathBuf::from(DEFAULT_MAPPING);
    default.exists().then_some(default)
}

fn build_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Box::new(CAnalyzer::c()));
    r.register(Box::new(CAnalyzer::cpp()));
    r.register(Box::new(RustAnalyzer::new()));
    r.register(Box::new(JavaAnalyzer::new()));
    r.register(Box::new(PythonAnalyzer::new()));
    r.register(Box::new(RAnalyzer::new()));
    r.register(Box::new(PerlAnalyzer::new()));
    r.register(Box::new(FortranAnalyzer::new()));
    r
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Analyze { path, lang, out, recurse } => cmd_analyze(&path, lang, out.as_deref(), recurse),
        Cmd::Compare { rust, other, mapping, sort, top, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_compare(&rust, &other, mapping.as_deref(), sort, top, format)
        }
        Cmd::Missing { rust, other, mapping, stub_loc_ratio, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_missing(&rust, &other, mapping.as_deref(), stub_loc_ratio, format)
        }
        Cmd::Sort { report, by, top, format } => cmd_sort(&report, &by, top, format),
        Cmd::ConstantsDiff { rust, other, mapping, top, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_constants_diff(&rust, &other, mapping.as_deref(), top, format)
        }
        Cmd::CompareStructs { rust, other, mapping, top, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_compare_structs(&rust, &other, mapping.as_deref(), top, format)
        }
        Cmd::MissingStructs { rust, other, mapping, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_missing_structs(&rust, &other, mapping.as_deref(), format)
        }
        Cmd::Order { path, lang, recurse, out, strict, merge } => {
            cmd_order(&path, lang, recurse, out.as_deref(), strict, merge.as_deref())
        }
        Cmd::OrderAnnotate { csv, source, rust, mapping, out } => {
            let mapping = resolve_mapping(mapping);
            cmd_order_annotate(&csv, &source, &rust, mapping.as_deref(), out.as_deref())
        }
        Cmd::Predict { sub } => match sub {
            PredictCmd::Train { pairs_dir, model } => cmd_predict_train(&pairs_dir, &model),
            PredictCmd::Apply { model, source, against, mapping, out } => {
                let mapping = resolve_mapping(mapping);
                cmd_predict_apply(
                    &model,
                    &source,
                    against.as_deref(),
                    mapping.as_deref(),
                    out.as_deref(),
                )
            }
        },
        Cmd::CompareTui { mapping, rust_root, other_root, other_lang } => {
            let lang = match other_lang {
                Some(l) => l.to_language(),
                None => {
                    let detected = code_complexity_comparator_rs::tui::detect_other_language(&other_root)
                        .ok_or_else(|| anyhow!(
                            "could not detect source language under {} — pass --other-lang",
                            other_root.display()
                        ))?;
                    eprintln!("auto-detected other-lang: {}", detected.as_str());
                    detected
                }
            };
            code_complexity_comparator_rs::tui::run(code_complexity_comparator_rs::tui::Args {
                mapping,
                rust_root,
                other_root,
                other_lang: lang,
            })
        }
        Cmd::Upstream {
            rust,
            other,
            mapping,
            rust_fn,
            rust_path,
            rust_line,
            rust_class,
            other_fn,
            other_path,
            other_line,
            other_class,
            strict,
            format,
        } => {
            let mapping = resolve_mapping(mapping);
            let rust_selector = FunctionSelector {
                name: rust_fn,
                path: rust_path,
                line: rust_line,
                class: rust_class,
            };
            let other_selector = FunctionSelector {
                name: other_fn,
                path: other_path,
                line: other_line,
                class: other_class,
            };
            cmd_upstream(
                &rust,
                &other,
                mapping.as_deref(),
                if rust_selector.is_empty() { None } else { Some(&rust_selector) },
                if other_selector.is_empty() { None } else { Some(&other_selector) },
                strict,
                format,
            )
        }
        Cmd::CallGraphDiff { rust, other, mapping, strict, format } => {
            let mapping = resolve_mapping(mapping);
            cmd_call_graph_diff(&rust, &other, mapping.as_deref(), strict, format)
        }
    }
}

fn cmd_analyze(path: &Path, lang: Option<LangArg>, out: Option<&Path>, recurse: bool) -> Result<()> {
    let reg = build_registry();
    let mut reports: Vec<Report> = Vec::new();

    if path.is_file() {
        reports.push(analyze_file(&reg, path, lang.map(|l| l.to_language()))?);
    } else if path.is_dir() {
        collect_and_analyze_dir(&reg, path, lang.map(|l| l.to_language()), recurse, &mut reports)?;
    } else {
        return Err(anyhow!("not a file or directory: {}", path.display()));
    }

    // Merge into one report if multiple files.
    let merged = if reports.len() == 1 {
        reports.pop().unwrap()
    } else {
        let mut r = Report {
            schema_version: code_complexity_comparator_rs::core::SCHEMA_VERSION,
            language: reports.first().map(|r| r.language).unwrap_or(Language::Unknown),
            source_file: path.to_path_buf(),
            source_hash: String::new(),
            functions: Vec::new(),
            structs: Vec::new(),
        };
        for sub in reports {
            r.functions.extend(sub.functions);
            r.structs.extend(sub.structs);
        }
        r
    };

    let json = serde_json::to_string_pretty(&merged)?;
    if let Some(p) = out {
        std::fs::write(p, json)?;
        eprintln!("wrote {} ({} functions)", p.display(), merged.functions.len());
    } else {
        println!("{}", json);
    }
    Ok(())
}

fn analyze_file(reg: &Registry, path: &Path, lang: Option<Language>) -> Result<Report> {
    let analyzer: &dyn LanguageAnalyzer = match lang {
        Some(l) => reg.for_language(l).ok_or_else(|| anyhow!("no analyzer for {:?}", l))?,
        None => reg
            .for_path(path)
            .ok_or_else(|| anyhow!("no analyzer for extension of {}", path.display()))?,
    };
    analyzer
        .analyze_file(path)
        .with_context(|| format!("analyze {}", path.display()))
}

fn collect_and_analyze_dir(
    reg: &Registry,
    dir: &Path,
    lang: Option<Language>,
    recurse: bool,
    out: &mut Vec<Report>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            if recurse {
                collect_and_analyze_dir(reg, &p, lang, recurse, out)?;
            }
            continue;
        }
        let ext = match p.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_string(),
            None => continue,
        };
        let analyzer_opt: Option<&dyn LanguageAnalyzer> = match lang {
            Some(l) => {
                // Only analyze files whose language matches.
                let file_lang = Language::from_ext(&ext);
                if file_lang != l {
                    continue;
                }
                reg.for_language(l)
            }
            None => reg.for_extension(&ext),
        };
        if let Some(a) = analyzer_opt {
            match a.analyze_file(&p) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warn: {}: {}", p.display(), e),
            }
        }
    }
    Ok(())
}

fn cmd_compare(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    _sort: SortArg,
    top: usize,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let m = match_reports(&rust_r, &other_r, map.as_ref());
    let weights = Weights::default();
    let rows = deviation_rows(&rust_r, &other_r, &m, &weights);

    let shown: Vec<_> = rows.iter().take(top).collect();
    match format {
        FormatArg::Json => {
            println!("{}", serde_json::to_string_pretty(&shown)?);
        }
        FormatArg::Table => {
            println!("{:<30} {:<30} {:>8} top-contributors", "rust", "other", "deviation");
            for r in shown {
                let contribs: Vec<String> = r
                    .per_metric
                    .iter()
                    .take(3)
                    .map(|(k, rv, ov, c)| format!("{}({:.0}->{:.0} Δ={:.2})", k, ov, rv, c))
                    .collect();
                println!(
                    "{:<30} {:<30} {:>8.2}  {}",
                    truncate(&r.rust_name, 30),
                    truncate(&r.other_name, 30),
                    r.total,
                    contribs.join(", ")
                );
            }
            println!("({} matched pairs total)", rows.len());
        }
    }
    Ok(())
}

fn cmd_missing(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    stub_loc_ratio: f64,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let m = match_reports(&rust_r, &other_r, map.as_ref());
    let rep = missing(&rust_r, &other_r, &m, stub_loc_ratio);
    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&rep)?),
        FormatArg::Table => {
            println!("Missing in Rust ({}):", rep.missing_in_rust.len());
            for n in &rep.missing_in_rust {
                println!("  - {}", n);
            }
            println!("Extra in Rust ({}):", rep.extra_in_rust.len());
            for n in &rep.extra_in_rust {
                println!("  + {}", n);
            }
            println!("Partial/stubs ({}):", rep.partial.len());
            for p in &rep.partial {
                println!("  ~ {} (rust) vs {} (other): {}", p.rust_name, p.other_name, p.reason);
            }
        }
    }
    Ok(())
}

fn cmd_sort(report: &Path, by: &str, top: usize, format: FormatArg) -> Result<()> {
    let r = load_report(report)?;
    let key = SortKey::parse(by).ok_or_else(|| anyhow!("unknown sort key: {}", by))?;
    let sorted = sort_report(&r, key);
    let shown = sorted.iter().take(top);
    match format {
        FormatArg::Json => {
            #[derive(Serialize)]
            struct Row<'a> {
                name: &'a str,
                cyclomatic: u32,
                cognitive: u32,
                nesting: u32,
                loc: u32,
                calls: u32,
                halstead_difficulty: f64,
            }
            let rows: Vec<Row> = shown
                .map(|f| Row {
                    name: &f.name,
                    cyclomatic: f.metrics.cyclomatic,
                    cognitive: f.metrics.cognitive,
                    nesting: f.metrics.max_combined_nesting,
                    loc: f.metrics.loc_code,
                    calls: f.metrics.calls_total,
                    halstead_difficulty: f.metrics.halstead.difficulty,
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        FormatArg::Table => {
            println!(
                "{:<40} {:>5} {:>5} {:>5} {:>5} {:>5} {:>8}",
                "name", "cycl", "cogn", "nest", "loc", "calls", "halstD"
            );
            for f in shown {
                println!(
                    "{:<40} {:>5} {:>5} {:>5} {:>5} {:>5} {:>8.2}",
                    truncate(&f.name, 40),
                    f.metrics.cyclomatic,
                    f.metrics.cognitive,
                    f.metrics.max_combined_nesting,
                    f.metrics.loc_code,
                    f.metrics.calls_total,
                    f.metrics.halstead.difficulty,
                );
            }
            println!("({} functions total)", r.functions.len());
        }
    }
    Ok(())
}

fn cmd_constants_diff(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    top: usize,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let m = match_reports(&rust_r, &other_r, map.as_ref());
    let diff = constants_diff(&m);
    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&diff)?),
        FormatArg::Table => {
            for d in diff.per_function.iter().take(top) {
                if d.only_in_rust.is_empty() && d.only_in_other.is_empty() {
                    continue;
                }
                println!("== {} (rust) <-> {} (other) score={:.2} ==", d.rust_name, d.other_name, d.score);
                for c in &d.only_in_other {
                    println!("  - (other only) [{}] {}", c.kind, c.display);
                }
                for c in &d.only_in_rust {
                    println!("  + (rust  only) [{}] {}", c.kind, c.display);
                }
            }
        }
    }
    Ok(())
}

fn cmd_compare_structs(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    top: usize,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let m = match_structs(&rust_r, &other_r, map.as_ref());
    let rows = struct_deviation_rows(&rust_r, &other_r, &m);

    let shown: Vec<_> = rows.iter().take(top).collect();
    match format {
        FormatArg::Json => {
            println!("{}", serde_json::to_string_pretty(&shown)?);
        }
        FormatArg::Table => {
            println!(
                "{:<30} {:<30} {:>8} top-contributors",
                "rust", "other", "deviation"
            );
            for r in shown {
                let contribs: Vec<String> = r
                    .per_category
                    .iter()
                    .take(3)
                    .filter(|(_, _, _, c)| *c > 0.0)
                    .map(|(k, rv, ov, c)| format!("{}({:.0}->{:.0} Δ={:.2})", k, ov, rv, c))
                    .collect();
                println!(
                    "{:<30} {:<30} {:>8.2}  {}",
                    truncate(&r.rust_name, 30),
                    truncate(&r.other_name, 30),
                    r.total,
                    contribs.join(", ")
                );
            }
            println!(
                "({} matched struct pairs, {} rust / {} other structs total)",
                rows.len(),
                rust_r.structs.len(),
                other_r.structs.len(),
            );
        }
    }
    Ok(())
}

fn cmd_missing_structs(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let m = match_structs(&rust_r, &other_r, map.as_ref());
    let rep = struct_missing(&rust_r, &other_r, &m);
    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&rep)?),
        FormatArg::Table => {
            println!("Missing in Rust ({}):", rep.missing_in_rust.len());
            for n in &rep.missing_in_rust {
                println!("  - {}", n);
            }
            println!("Extra in Rust ({}):", rep.extra_in_rust.len());
            for n in &rep.extra_in_rust {
                println!("  + {}", n);
            }
        }
    }
    Ok(())
}

fn cmd_predict_train(pairs_dir: &Path, model_path: &Path) -> Result<()> {
    let mut pairs: Vec<(Report, Report)> = Vec::new();
    let mut by_base: std::collections::BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> = Default::default();
    for entry in std::fs::read_dir(pairs_dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        // Expect "base.rust.json", "base.c.json", "base.cpp.json" or similar;
        // strip the language suffix to find the base.
        let (base, lang) = match stem.rsplit_once('.') {
            Some((b, l)) => (b.to_string(), l.to_string()),
            None => continue,
        };
        let slot = by_base.entry(base).or_insert((None, None));
        if lang == "rust" {
            slot.0 = Some(p.clone());
        } else {
            slot.1 = Some(p.clone());
        }
    }
    for (base, (rr, or)) in by_base {
        let (rr, or) = match (rr, or) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                eprintln!("skipping base {}: need both rust and other", base);
                continue;
            }
        };
        let rrep = load_report(&rr)?;
        let orep = load_report(&or)?;
        pairs.push((rrep, orep));
    }
    let model = train(&pairs)?;
    model.save(model_path)?;
    eprintln!("saved model to {} ({} pairs, {} metrics)", model_path.display(), pairs.len(), model.per_metric.len());
    Ok(())
}

fn cmd_predict_apply(
    model: &Path,
    source: &Path,
    against: Option<&Path>,
    mapping: Option<&Path>,
    out: Option<&Path>,
) -> Result<()> {
    let model = Model::load(model)?;
    let other = load_report(source)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let against_r = against.map(load_report).transpose()?;
    let ctx = against_r.as_ref().map(|r| (r, map.as_ref()));
    let rep = predict_report(&model, &other, ctx);
    let s = serde_json::to_string_pretty(&rep)?;
    if let Some(p) = out {
        std::fs::write(p, s)?;
        eprintln!("wrote {}", p.display());
    } else {
        println!("{}", s);
    }
    Ok(())
}

fn cmd_order(
    path: &Path,
    lang: Option<LangArg>,
    recurse: bool,
    out: Option<&Path>,
    strict: bool,
    merge: Option<&Path>,
) -> Result<()> {
    let report = if path.extension().and_then(|e| e.to_str()) == Some("json") {
        load_report(path)?
    } else {
        let reg = build_registry();
        let mut reports: Vec<Report> = Vec::new();
        if path.is_file() {
            reports.push(analyze_file(&reg, path, lang.map(|l| l.to_language()))?);
        } else if path.is_dir() {
            collect_and_analyze_dir(&reg, path, lang.map(|l| l.to_language()), recurse, &mut reports)?;
        } else {
            return Err(anyhow!(
                "not a file, directory, or .json report: {}",
                path.display()
            ));
        }
        if reports.is_empty() {
            return Err(anyhow!("no files analyzed under {}", path.display()));
        }
        let language = reports.first().map(|r| r.language).unwrap_or(Language::Unknown);
        let mut merged = Report {
            schema_version: code_complexity_comparator_rs::core::SCHEMA_VERSION,
            language,
            source_file: path.to_path_buf(),
            source_hash: String::new(),
            functions: Vec::new(),
            structs: Vec::new(),
        };
        for sub in reports {
            merged.functions.extend(sub.functions);
            merged.structs.extend(sub.structs);
        }
        merged
    };

    let g = order::build_call_graph(&report, strict);
    let ord = order::order_bottom_up(&g);
    let edges: usize = g.edges.iter().map(|v| v.len()).sum();
    eprintln!(
        "graph: {} functions, {} edges, {} ambiguous call sites, {} unresolved",
        report.functions.len(),
        edges,
        g.ambiguous_call_sites,
        g.unresolved_call_sites,
    );

    let prev = match merge {
        Some(p) => order::read_translated_map(p)?,
        None => HashMap::new(),
    };
    let csv = order::render_order_csv(&g, &ord, &prev);
    if let Some(p) = out {
        std::fs::write(p, &csv)?;
        eprintln!("wrote {} ({} rows)", p.display(), ord.len());
    } else {
        print!("{}", csv);
    }
    Ok(())
}

fn cmd_order_annotate(
    csv_path: &Path,
    source: &Path,
    rust: &Path,
    mapping: Option<&Path>,
    out: Option<&Path>,
) -> Result<()> {
    let csv = order::read_csv(csv_path)?;
    let source_r = load_report(source)?;
    let rust_r = load_report(rust)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let s = order::render_annotated_csv(&csv, &source_r, &rust_r, map.as_ref())?;
    if let Some(p) = out {
        std::fs::write(p, &s)?;
        eprintln!("wrote {}", p.display());
    } else {
        print!("{}", s);
    }
    Ok(())
}

fn cmd_upstream(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    rust_selector: Option<&FunctionSelector>,
    other_selector: Option<&FunctionSelector>,
    strict: bool,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let matches = match_reports(&rust_r, &other_r, map.as_ref());
    let analysis = analyze_upstream(
        &rust_r,
        &other_r,
        &matches,
        rust_selector,
        other_selector,
        strict,
    )?;

    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&analysis)?),
        FormatArg::Table => {
            if let Some(seed) = &analysis.rust_seed {
                println!(
                    "Rust seed: {} @ {}:{}",
                    seed.name, seed.file, seed.line_start
                );
            } else {
                println!("Rust seed: <unresolved>");
            }
            if let Some(seed) = &analysis.other_seed {
                println!(
                    "Other seed: {} @ {}:{}",
                    seed.name, seed.file, seed.line_start
                );
            } else {
                println!("Other seed: <unresolved>");
            }

            println!("Rust upstream set ({}):", analysis.rust_upstream.len());
            for f in &analysis.rust_upstream {
                println!("  - {} @ {}:{}", f.name, f.file, f.line_start);
            }

            println!("Other upstream set ({}):", analysis.other_upstream.len());
            for f in &analysis.other_upstream {
                println!("  - {} @ {}:{}", f.name, f.file, f.line_start);
            }

            println!("Warnings ({}):", analysis.warnings.len());
            for w in &analysis.warnings {
                match &w.counterpart {
                    Some(cp) => println!(
                        "  ! [{}] {} @ {}:{} -> {} @ {}:{}: {}",
                        w.side,
                        w.function.name,
                        w.function.file,
                        w.function.line_start,
                        cp.name,
                        cp.file,
                        cp.line_start,
                        w.message
                    ),
                    None => println!(
                        "  ! [{}] {} @ {}:{}: {}",
                        w.side, w.function.name, w.function.file, w.function.line_start, w.message
                    ),
                }
            }

            println!(
                "{:<5} {:>8} {:<30} {:<30} top-contributors",
                "ovlp", "dev", "rust", "other"
            );
            for row in &analysis.pairs {
                let contribs: Vec<String> = row
                    .per_metric
                    .iter()
                    .take(3)
                    .map(|(k, rv, ov, c)| format!("{}({:.0}->{:.0} Δ={:.2})", k, ov, rv, c))
                    .collect();
                println!(
                    "{:<5} {:>8.2} {:<30} {:<30} {}",
                    if row.overlap { "yes" } else { "no" },
                    row.total,
                    truncate(&row.rust.name, 30),
                    truncate(&row.other.name, 30),
                    contribs.join(", ")
                );
            }
            println!("({} translated pairs touching the upstream sets)", analysis.pairs.len());
        }
    }
    Ok(())
}

fn cmd_call_graph_diff(
    rust: &Path,
    other: &Path,
    mapping: Option<&Path>,
    strict: bool,
    format: FormatArg,
) -> Result<()> {
    let rust_r = load_report(rust)?;
    let other_r = load_report(other)?;
    let map = mapping.map(Mapping::load).transpose()?;
    let matches = match_reports(&rust_r, &other_r, map.as_ref());
    let analysis = analyze_call_graph_diff(&rust_r, &other_r, &matches, strict);

    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&analysis)?),
        FormatArg::Table => {
            println!("Matched pairs: {}", analysis.summary.matched_pairs);
            println!(
                "Translated edges: rust={} other={}",
                analysis.summary.translated_edges_in_rust,
                analysis.summary.translated_edges_in_other
            );
            println!(
                "Edge differences: only-in-rust={} only-in-other={}",
                analysis.summary.edges_only_in_rust,
                analysis.summary.edges_only_in_other
            );
            println!(
                "Recursion mismatches: kind={} scc-size={}",
                analysis.summary.recursive_kind_mismatches,
                analysis.summary.scc_size_mismatches
            );
            println!(
                "Ambiguous/unresolved call sites: rust={}/{} other={}/{}",
                analysis.summary.rust_ambiguous_call_sites,
                analysis.summary.rust_unresolved_call_sites,
                analysis.summary.other_ambiguous_call_sites,
                analysis.summary.other_unresolved_call_sites
            );

            if !analysis.edges_only_in_rust.is_empty() {
                println!("Edges only in rust ({}):", analysis.edges_only_in_rust.len());
                for edge in &analysis.edges_only_in_rust {
                    println!(
                        "  - {} @ {}:{} -> {} @ {}:{}",
                        edge.src.name,
                        edge.src.file,
                        edge.src.line_start,
                        edge.dst.name,
                        edge.dst.file,
                        edge.dst.line_start
                    );
                }
            }
            if !analysis.edges_only_in_other.is_empty() {
                println!("Edges only in other ({}):", analysis.edges_only_in_other.len());
                for edge in &analysis.edges_only_in_other {
                    println!(
                        "  - {} @ {}:{} -> {} @ {}:{}",
                        edge.src.name,
                        edge.src.file,
                        edge.src.line_start,
                        edge.dst.name,
                        edge.dst.file,
                        edge.dst.line_start
                    );
                }
            }

            println!(
                "{:<5} {:>6} {:>5} {:>5} {:<30} {:<30}",
                "ovlp", "total", "rcal", "ocal", "rust", "other"
            );
            for row in &analysis.pairs {
                println!(
                    "{:<5} {:>6.1} {:>5} {:>5} {:<30} {:<30}",
                    if row.total == 0.0 { "yes" } else { "no" },
                    row.total,
                    row.rust_callees + row.rust_callers,
                    row.other_callees + row.other_callers,
                    truncate(&row.rust.name, 30),
                    truncate(&row.other.name, 30)
                );
            }
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}
