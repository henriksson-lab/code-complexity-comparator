use anyhow::{anyhow, Result};
use crate::core::{FunctionAnalysis, Report};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStrategy {
    Mapping,
    FfiAttribute,
    ExactName,
    Normalized,
    Fingerprint,
}

#[derive(Debug, Clone)]
pub struct Pair<'a> {
    pub rust: &'a FunctionAnalysis,
    pub other: &'a FunctionAnalysis,
    pub strategy: MatchStrategy,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Mapping {
    #[serde(default)]
    pub entries: Vec<MappingEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MappingEntry {
    pub rust: String,
    pub other: String,
}

impl Mapping {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("read mapping {}: {}", path.display(), e))?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "toml" {
            toml::from_str(&s).map_err(|e| anyhow!("parse mapping: {}", e))
        } else {
            serde_json::from_str(&s).map_err(|e| anyhow!("parse mapping: {}", e))
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatchResult<'a> {
    pub pairs: Vec<Pair<'a>>,
}

pub fn match_reports<'a>(
    rust: &'a Report,
    other: &'a Report,
    mapping: Option<&Mapping>,
) -> MatchResult<'a> {
    let mut pairs = Vec::new();
    let mut used_rust: HashSet<usize> = HashSet::new();
    let mut used_other: HashSet<usize> = HashSet::new();

    // 1. Explicit mapping.
    if let Some(m) = mapping {
        for e in &m.entries {
            let ri = rust.functions.iter().position(|f| f.name == e.rust);
            let oi = other.functions.iter().position(|f| f.name == e.other);
            if let (Some(ri), Some(oi)) = (ri, oi) {
                if !used_rust.contains(&ri) && !used_other.contains(&oi) {
                    used_rust.insert(ri);
                    used_other.insert(oi);
                    pairs.push(Pair {
                        rust: &rust.functions[ri],
                        other: &other.functions[oi],
                        strategy: MatchStrategy::Mapping,
                    });
                }
            }
        }
    }

    // 2. FFI attribute: rust.original_name matches other.name.
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        if let Some(orig) = &rf.original_name {
            if let Some((oi, _)) = other
                .functions
                .iter()
                .enumerate()
                .find(|(oi, f)| !used_other.contains(oi) && &f.name == orig)
            {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::FfiAttribute,
                });
            }
        }
    }

    // 3. Exact name match.
    let mut by_name_other: HashMap<&str, usize> = HashMap::new();
    for (oi, f) in other.functions.iter().enumerate() {
        by_name_other.insert(f.name.as_str(), oi);
    }
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        if let Some(&oi) = by_name_other.get(rf.name.as_str()) {
            if !used_other.contains(&oi) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::ExactName,
                });
            }
        }
    }

    // 4. Normalized name match.
    let mut by_norm_other: HashMap<String, Vec<usize>> = HashMap::new();
    for (oi, f) in other.functions.iter().enumerate() {
        if used_other.contains(&oi) {
            continue;
        }
        by_norm_other
            .entry(normalize_name(&f.name))
            .or_default()
            .push(oi);
    }
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        let norm = normalize_name(&rf.name);
        if let Some(cands) = by_norm_other.get(&norm) {
            if let Some(&oi) = cands.iter().find(|oi| !used_other.contains(oi)) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::Normalized,
                });
            }
        }
    }

    // 5. Fingerprint fallback: only when names share a substantial token.
    //   Short names ("w", "k", "map") are common as builder methods or
    //   accessors and produce too many spurious matches, so require the
    //   shared token to be >= 4 chars and the fingerprints to match exactly.
    let r_unmatched: Vec<usize> = (0..rust.functions.len())
        .filter(|i| !used_rust.contains(i))
        .collect();
    let o_unmatched: Vec<usize> = (0..other.functions.len())
        .filter(|i| !used_other.contains(i))
        .collect();
    for &ri in &r_unmatched {
        let rf = &rust.functions[ri];
        let fp_r = fingerprint(rf);
        let rtoks = tokenize_name(&rf.name);
        for &oi in &o_unmatched {
            if used_other.contains(&oi) {
                continue;
            }
            let of = &other.functions[oi];
            if fp_r != fingerprint(of) {
                continue;
            }
            let otoks = tokenize_name(&of.name);
            if shared_substantial_token(&rtoks, &otoks) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: of,
                    strategy: MatchStrategy::Fingerprint,
                });
                break;
            }
        }
    }

    MatchResult { pairs }
}

pub fn normalize_name(name: &str) -> String {
    // Lowercase, split on camelCase and snake_case, drop underscores and common
    // suffixes/prefixes like _impl, _inner, p_, _c, _rs.
    let mut chars = String::with_capacity(name.len());
    let mut prev_is_lower = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && prev_is_lower {
            chars.push('_');
        }
        chars.push(ch.to_ascii_lowercase());
        prev_is_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    let s = chars.replace("::", "_");
    let parts: Vec<&str> = s
        .split('_')
        .filter(|p| !p.is_empty())
        .filter(|p| !matches!(*p, "impl" | "inner" | "helper" | "rs" | "c" | "cpp"))
        .collect();
    parts.join("_")
}

fn tokenize_name(name: &str) -> Vec<String> {
    normalize_name(name).split('_').map(|s| s.to_string()).collect()
}

fn shared_substantial_token(a: &[String], b: &[String]) -> bool {
    for ta in a {
        if ta.len() < 4 {
            continue;
        }
        for tb in b {
            if ta == tb {
                return true;
            }
        }
    }
    false
}

fn fingerprint(f: &FunctionAnalysis) -> (u32, u32, u32) {
    let loc_bucket = bucket_log(f.metrics.loc_code);
    let arity = f.signature.inputs.len() as u32;
    let outputs = f.signature.outputs.len() as u32;
    (arity, outputs, loc_bucket)
}

fn bucket_log(n: u32) -> u32 {
    if n == 0 {
        0
    } else {
        (n as f64).log2() as u32
    }
}
