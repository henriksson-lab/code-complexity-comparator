use complexity_core::{FunctionAnalysis, Metrics, Report};

#[derive(Debug, Clone, Copy)]
pub enum SortKey {
    Cognitive,
    Cyclomatic,
    CombinedNesting,
    Loc,
    HalsteadDifficulty,
    CombinedNestingXLoc,
    Composite,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "cognitive" => Self::Cognitive,
            "cyclomatic" => Self::Cyclomatic,
            "combined-nesting" | "combined_nesting" | "nesting" => Self::CombinedNesting,
            "loc" => Self::Loc,
            "halstead-difficulty" | "halstead_difficulty" | "halstead" => Self::HalsteadDifficulty,
            "combined-nesting-x-loc" | "nesting-x-loc" => Self::CombinedNestingXLoc,
            "composite" => Self::Composite,
            _ => return None,
        })
    }

    pub fn eval(&self, m: &Metrics, norm: Option<&NormStats>) -> f64 {
        match self {
            Self::Cognitive => m.cognitive as f64,
            Self::Cyclomatic => m.cyclomatic as f64,
            Self::CombinedNesting => m.max_combined_nesting as f64,
            Self::Loc => m.loc_code as f64,
            Self::HalsteadDifficulty => m.halstead.difficulty,
            Self::CombinedNestingXLoc => (m.max_combined_nesting as f64) * (m.loc_code as f64),
            Self::Composite => {
                // z-score sum across a few metrics
                let n = norm.expect("norm required for composite");
                zscore(m.cognitive as f64, n.cognitive_mean, n.cognitive_std)
                    + zscore(m.max_combined_nesting as f64, n.nesting_mean, n.nesting_std)
                    + zscore(m.calls_total as f64, n.calls_mean, n.calls_std)
                    + zscore(m.loc_code as f64, n.loc_mean, n.loc_std)
                    + zscore(
                        m.halstead.difficulty,
                        n.halstead_diff_mean,
                        n.halstead_diff_std,
                    )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NormStats {
    pub cognitive_mean: f64,
    pub cognitive_std: f64,
    pub nesting_mean: f64,
    pub nesting_std: f64,
    pub calls_mean: f64,
    pub calls_std: f64,
    pub loc_mean: f64,
    pub loc_std: f64,
    pub halstead_diff_mean: f64,
    pub halstead_diff_std: f64,
}

pub fn norm_stats(r: &Report) -> NormStats {
    fn mean_std(v: &[f64]) -> (f64, f64) {
        if v.is_empty() {
            return (0.0, 1.0);
        }
        let m = v.iter().sum::<f64>() / v.len() as f64;
        let var = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64;
        (m, var.sqrt().max(1e-9))
    }
    let cognitive: Vec<f64> = r.functions.iter().map(|f| f.metrics.cognitive as f64).collect();
    let nesting: Vec<f64> = r.functions.iter().map(|f| f.metrics.max_combined_nesting as f64).collect();
    let calls: Vec<f64> = r.functions.iter().map(|f| f.metrics.calls_total as f64).collect();
    let loc: Vec<f64> = r.functions.iter().map(|f| f.metrics.loc_code as f64).collect();
    let halstead: Vec<f64> = r.functions.iter().map(|f| f.metrics.halstead.difficulty).collect();
    let (cm, cs) = mean_std(&cognitive);
    let (nm, ns) = mean_std(&nesting);
    let (km, ks) = mean_std(&calls);
    let (lm, ls) = mean_std(&loc);
    let (hm, hs) = mean_std(&halstead);
    NormStats {
        cognitive_mean: cm,
        cognitive_std: cs,
        nesting_mean: nm,
        nesting_std: ns,
        calls_mean: km,
        calls_std: ks,
        loc_mean: lm,
        loc_std: ls,
        halstead_diff_mean: hm,
        halstead_diff_std: hs,
    }
}

fn zscore(x: f64, mean: f64, std: f64) -> f64 {
    (x - mean) / std.max(1e-9)
}

pub fn sort_report<'a>(r: &'a Report, key: SortKey) -> Vec<&'a FunctionAnalysis> {
    let norm = if matches!(key, SortKey::Composite) {
        Some(norm_stats(r))
    } else {
        None
    };
    let mut v: Vec<&FunctionAnalysis> = r.functions.iter().collect();
    v.sort_by(|a, b| {
        let va = key.eval(&a.metrics, norm.as_ref());
        let vb = key.eval(&b.metrics, norm.as_ref());
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}
