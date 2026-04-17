use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use complexity::analyzer::{LanguageAnalyzer, Registry};
use complexity::compare::{
    constants_diff, deviation_rows, load_report, match_reports, missing, sort_report, Mapping,
    SortKey, Weights,
};
use complexity::core::{Language, Report};
use complexity::lang_c::CAnalyzer;
use complexity::lang_rust::RustAnalyzer;
use complexity::predict::{predict_report, train, Model};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "complexity", about = "Static complexity analysis and cross-language comparison")]
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
    /// Train or apply the prediction model.
    Predict {
        #[command(subcommand)]
        sub: PredictCmd,
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
}

impl LangArg {
    fn to_language(self) -> Language {
        match self {
            LangArg::C => Language::C,
            LangArg::Cpp => Language::Cpp,
            LangArg::Rust => Language::Rust,
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

fn build_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Box::new(CAnalyzer::c()));
    r.register(Box::new(CAnalyzer::cpp()));
    r.register(Box::new(RustAnalyzer::new()));
    r
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Analyze { path, lang, out, recurse } => cmd_analyze(&path, lang, out.as_deref(), recurse),
        Cmd::Compare { rust, other, mapping, sort, top, format } => {
            cmd_compare(&rust, &other, mapping.as_deref(), sort, top, format)
        }
        Cmd::Missing { rust, other, mapping, stub_loc_ratio, format } => {
            cmd_missing(&rust, &other, mapping.as_deref(), stub_loc_ratio, format)
        }
        Cmd::Sort { report, by, top, format } => cmd_sort(&report, &by, top, format),
        Cmd::ConstantsDiff { rust, other, mapping, top, format } => {
            cmd_constants_diff(&rust, &other, mapping.as_deref(), top, format)
        }
        Cmd::Predict { sub } => match sub {
            PredictCmd::Train { pairs_dir, model } => cmd_predict_train(&pairs_dir, &model),
            PredictCmd::Apply { model, source, against, mapping, out } => cmd_predict_apply(
                &model,
                &source,
                against.as_deref(),
                mapping.as_deref(),
                out.as_deref(),
            ),
        },
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
            schema_version: complexity::core::SCHEMA_VERSION,
            language: reports.first().map(|r| r.language).unwrap_or(Language::Unknown),
            source_file: path.to_path_buf(),
            source_hash: String::new(),
            functions: Vec::new(),
        };
        for sub in reports {
            r.functions.extend(sub.functions);
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

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}
