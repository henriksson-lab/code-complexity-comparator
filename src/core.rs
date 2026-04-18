use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    C,
    Cpp,
    Rust,
    Java,
    Python,
    R,
    Perl,
    Fortran,
    Unknown,
}

impl Language {
    pub fn from_ext(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "rs" => Language::Rust,
            "java" => Language::Java,
            "py" => Language::Python,
            "r" => Language::R,
            "pl" | "pm" | "t" => Language::Perl,
            "f" | "f90" | "f95" | "f03" | "f08" | "for" | "ftn" => Language::Fortran,
            _ => Language::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::Python => "python",
            Language::R => "r",
            Language::Perl => "perl",
            Language::Fortran => "fortran",
            Language::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub language: Language,
    pub source_file: PathBuf,
    pub source_hash: String,
    pub functions: Vec<FunctionAnalysis>,
}

impl Report {
    pub fn new(language: Language, source_file: PathBuf, source_hash: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language,
            source_file,
            source_hash,
            functions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionAnalysis {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangled: Option<String>,
    pub location: Location,
    pub signature: Signature,
    pub metrics: Metrics,
    pub constants: Vec<Constant>,
    pub calls: Vec<Call>,
    pub types_used: Vec<TypeRef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub file: PathBuf,
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Signature {
    pub inputs: Vec<Param>,
    pub outputs: Vec<TypeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub text: String,
}

impl TypeRef {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub loc_code: u32,
    pub loc_comments: u32,
    pub loc_asm: u32,
    pub inputs: u32,
    pub outputs: u32,
    pub branches: u32,
    pub loops: u32,
    pub max_loop_nesting: u32,
    pub max_if_nesting: u32,
    pub max_combined_nesting: u32,
    pub calls_unique: u32,
    pub calls_total: u32,
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub halstead: Halstead,
    pub early_returns: u32,
    pub goto_count: u32,
    pub unsafe_blocks: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Halstead {
    pub n1: u32,
    pub n2: u32,
    pub big_n1: u32,
    pub big_n2: u32,
    pub volume: f64,
    pub difficulty: f64,
}

impl Halstead {
    pub fn compute(&mut self) {
        let n = (self.n1 + self.n2) as f64;
        let big_n = (self.big_n1 + self.big_n2) as f64;
        if n > 0.0 {
            self.volume = big_n * n.log2();
        }
        if self.n2 > 0 {
            self.difficulty = (self.n1 as f64 / 2.0) * (self.big_n2 as f64 / self.n2 as f64);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Constant {
    Int {
        value: i64,
        text: String,
        span: (u32, u32),
    },
    Float {
        value: f64,
        text: String,
        span: (u32, u32),
    },
    String {
        value: String,
        span: (u32, u32),
    },
    Char {
        value: String,
        span: (u32, u32),
    },
    Bool {
        value: bool,
        span: (u32, u32),
    },
}

impl Constant {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Constant::Int { .. } => "int",
            Constant::Float { .. } => "float",
            Constant::String { .. } => "string",
            Constant::Char { .. } => "char",
            Constant::Bool { .. } => "bool",
        }
    }

    pub fn equivalent_to(&self, other: &Constant) -> bool {
        match (self, other) {
            (Constant::Int { value: a, .. }, Constant::Int { value: b, .. }) => a == b,
            (Constant::Float { value: a, .. }, Constant::Float { value: b, .. }) => {
                (a - b).abs() < 1e-12
            }
            (Constant::String { value: a, .. }, Constant::String { value: b, .. }) => a == b,
            (Constant::Char { value: a, .. }, Constant::Char { value: b, .. }) => a == b,
            (Constant::Bool { value: a, .. }, Constant::Bool { value: b, .. }) => a == b,
            _ => false,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Constant::Int { value, text, .. } => format!("{} ({})", value, text),
            Constant::Float { value, text, .. } => format!("{} ({})", value, text),
            Constant::String { value, .. } => format!("\"{}\"", value.escape_debug()),
            Constant::Char { value, .. } => format!("'{}'", value),
            Constant::Bool { value, .. } => format!("{}", value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub callee: String,
    pub count: u32,
    pub span: (u32, u32),
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Msg(String),
}

pub fn hash_source(src: &str) -> String {
    // Lightweight non-crypto hash so reports can detect staleness without
    // pulling in a crypto dep. Deterministic across runs.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in src.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}
