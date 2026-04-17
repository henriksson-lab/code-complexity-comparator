use crate::core::FunctionAnalysis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeuristicRule {
    pub name: String,
    pub description: String,
    pub condition: Condition,
    pub adjustments: Vec<Adjustment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Condition {
    /// Apply when source function has > threshold gotos.
    GotoAtLeast(u32),
    /// Apply when source function has at least N switch/case arms.
    SwitchCasesAtLeast(u32),
    /// Apply when source has pointer-heavy signature (N+ pointer/ref types).
    PointerInputsAtLeast(u32),
    /// Apply when source has inline asm.
    HasAsm,
    /// Always.
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adjustment {
    pub metric: String,
    pub delta: f64,
    /// Multiplicative factor applied before delta (default 1.0).
    #[serde(default = "one_f64")]
    pub factor: f64,
}

fn one_f64() -> f64 {
    1.0
}

pub fn apply_heuristics(
    rules: &[HeuristicRule],
    other: &FunctionAnalysis,
    predicted: &mut BTreeMap<String, f64>,
) {
    for r in rules {
        if condition_matches(&r.condition, other) {
            for a in &r.adjustments {
                let entry = predicted.entry(a.metric.clone()).or_insert(0.0);
                *entry = (*entry * a.factor) + a.delta;
            }
        }
    }
}

fn condition_matches(c: &Condition, f: &FunctionAnalysis) -> bool {
    match c {
        Condition::GotoAtLeast(n) => f.metrics.goto_count >= *n,
        Condition::SwitchCasesAtLeast(n) => {
            // Approximate switch cases via SwitchCase contribution to branches.
            // We don't distinguish here; use branches as a weak proxy.
            f.metrics.branches >= *n
        }
        Condition::PointerInputsAtLeast(n) => {
            let p = f
                .signature
                .inputs
                .iter()
                .filter(|p| p.ty.text.contains('*') || p.ty.text.contains('&'))
                .count();
            p as u32 >= *n
        }
        Condition::HasAsm => f.metrics.loc_asm > 0,
        Condition::Always => true,
    }
}

pub fn default_rules() -> Vec<HeuristicRule> {
    vec![
        HeuristicRule {
            name: "c_goto_to_rust_early_return".into(),
            description: "C 'goto cleanup' pattern typically becomes '?' in Rust, adding early returns and removing some branches.".into(),
            condition: Condition::GotoAtLeast(1),
            adjustments: vec![
                Adjustment { metric: "early_returns".into(), delta: 1.0, factor: 1.0 },
                Adjustment { metric: "branches".into(), delta: -1.0, factor: 1.0 },
                Adjustment { metric: "cyclomatic".into(), delta: -1.0, factor: 1.0 },
            ],
        },
        HeuristicRule {
            name: "c_switch_to_rust_match".into(),
            description: "Switch with many arms: Rust `match` typically drops a default or cyclomatic by 1.".into(),
            condition: Condition::SwitchCasesAtLeast(5),
            adjustments: vec![
                Adjustment { metric: "cyclomatic".into(), delta: -1.0, factor: 1.0 },
            ],
        },
        HeuristicRule {
            name: "pointer_heavy_signature".into(),
            description: "Heavy pointer I/O in C typically becomes borrows/slices in Rust; expect fewer unsafe blocks but similar branches.".into(),
            condition: Condition::PointerInputsAtLeast(3),
            adjustments: vec![
                Adjustment { metric: "loc_code".into(), delta: 1.0, factor: 1.0 },
            ],
        },
        HeuristicRule {
            name: "inline_asm_preserves".into(),
            description: "Inline asm blocks generally survive translation wrapped in unsafe{}; LOC stable.".into(),
            condition: Condition::HasAsm,
            adjustments: vec![],
        },
    ]
}
