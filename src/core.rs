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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<StructAnalysis>,
}

impl Report {
    pub fn new(language: Language, source_file: PathBuf, source_hash: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language,
            source_file,
            source_hash,
            functions: Vec::new(),
            structs: Vec::new(),
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
    /// Enclosing class / impl-target / struct. For Rust, the type the
    /// containing `impl` block is written against (trait name is ignored —
    /// `impl Display for Strand` yields `Strand`). For Python, the
    /// containing `class` name. `None` for free functions, nested closures,
    /// or languages that don't model methods this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing_type: Option<String>,
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

/// A struct / class / record declaration. Fields are modelled as typed
/// variables so that cross-language comparisons can count them by type
/// category the same way functions compare by metric counts.
///
/// The `kind` string records the underlying construct: `"struct"`, `"class"`,
/// `"union"`, `"record"`, `"derived_type"`. Matching is name-based across
/// kinds, which lets a Rust `struct` pair with a C `struct` or a Python
/// `class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructAnalysis {
    pub name: String,
    pub kind: String,
    pub location: Location,
    pub fields: Vec<StructField>,
    pub metrics: StructMetrics,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub ty: TypeRef,
    pub category: TypeCategory,
}

/// Language-neutral bucket for a field type. `Other` catches user-defined
/// types that aren't classifiable from the textual type alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeCategory {
    Int,
    Float,
    Bool,
    Char,
    String,
    Pointer,
    Array,
    Collection,
    Other,
}

impl TypeCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            TypeCategory::Int => "int",
            TypeCategory::Float => "float",
            TypeCategory::Bool => "bool",
            TypeCategory::Char => "char",
            TypeCategory::String => "string",
            TypeCategory::Pointer => "pointer",
            TypeCategory::Array => "array",
            TypeCategory::Collection => "collection",
            TypeCategory::Other => "other",
        }
    }

    pub fn all() -> &'static [TypeCategory] {
        &[
            TypeCategory::Int,
            TypeCategory::Float,
            TypeCategory::Bool,
            TypeCategory::Char,
            TypeCategory::String,
            TypeCategory::Pointer,
            TypeCategory::Array,
            TypeCategory::Collection,
            TypeCategory::Other,
        ]
    }
}

/// Per-struct metrics mirroring the shape of `Metrics` for functions. The
/// per-category counts are the primary comparison features — two structs
/// that hold the same numeric counts across int/float/pointer/… are shaped
/// the same even if the declared type names diverge across languages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructMetrics {
    pub field_count: u32,
    pub int_count: u32,
    pub float_count: u32,
    pub bool_count: u32,
    pub char_count: u32,
    pub string_count: u32,
    pub pointer_count: u32,
    pub array_count: u32,
    pub collection_count: u32,
    pub other_count: u32,
}

impl StructMetrics {
    pub fn from_fields(fields: &[StructField]) -> Self {
        let mut m = StructMetrics::default();
        m.field_count = fields.len() as u32;
        for f in fields {
            match f.category {
                TypeCategory::Int => m.int_count += 1,
                TypeCategory::Float => m.float_count += 1,
                TypeCategory::Bool => m.bool_count += 1,
                TypeCategory::Char => m.char_count += 1,
                TypeCategory::String => m.string_count += 1,
                TypeCategory::Pointer => m.pointer_count += 1,
                TypeCategory::Array => m.array_count += 1,
                TypeCategory::Collection => m.collection_count += 1,
                TypeCategory::Other => m.other_count += 1,
            }
        }
        m
    }
}

/// Heuristic classifier for a textual type. Operates on the trimmed type
/// string — it has no access to a type environment, so this is a best-effort
/// bucketing shared by Rust, C/C++, Java, Python, Fortran. Ordering matters:
/// pointer/reference markers are checked before primitive name matching so
/// that `*const c_char` classifies as a pointer/string, not as `char`.
pub fn classify_type(ty: &str) -> TypeCategory {
    let t = ty.trim();
    if t.is_empty() {
        return TypeCategory::Other;
    }
    let lower = t.to_ascii_lowercase();

    // String-ish — check before pointer, since `*const c_char` / `char *`
    // should land in `String` rather than `Pointer`.
    if is_string_type(&lower) {
        return TypeCategory::String;
    }

    // Array: `[T; N]`, `T[N]`, `T[]`, Fortran `dimension(...)`.
    if is_array_type(t) {
        return TypeCategory::Array;
    }

    // Collection: Vec<T>, HashMap<...>, std::vector<T>, List<T>, dict, set
    if is_collection_type(&lower) {
        return TypeCategory::Collection;
    }

    // Pointer / reference
    if is_pointer_type(t, &lower) {
        return TypeCategory::Pointer;
    }

    // Primitives
    if is_bool_type(&lower) {
        return TypeCategory::Bool;
    }
    if is_char_type(&lower) {
        return TypeCategory::Char;
    }
    if is_float_type(&lower) {
        return TypeCategory::Float;
    }
    if is_int_type(&lower) {
        return TypeCategory::Int;
    }
    TypeCategory::Other
}

fn is_string_type(lower: &str) -> bool {
    // Match common string-ish shapes across languages. We require fairly
    // precise tokens to avoid classifying e.g. `string_view_iterator` as
    // string.
    let trimmed = lower.trim_start_matches('&').trim();
    matches!(
        trimmed,
        "string"
            | "str"
            | "&str"
            | "&'static str"
            | "cstr"
            | "cstring"
            | "osstring"
            | "osstr"
            | "pathbuf"
            | "path"
            | "charsequence"
    ) || trimmed.starts_with("string")
        || trimmed.starts_with("std::string")
        || trimmed.ends_with("::string")
        || trimmed.contains("char *")
        || trimmed.contains("char*")
        || trimmed.contains("c_char")
        || trimmed.contains("character(len")
        || trimmed.starts_with("character*")
}

fn is_array_type(t: &str) -> bool {
    let trimmed = t.trim();
    // Rust: `[T; N]` or `[T]`
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return true;
    }
    // C / Java: `T[N]` / `T[]`
    if trimmed.ends_with(']') {
        return true;
    }
    // Fortran: `dimension(...)`
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("dimension(") || lower.contains(", dimension")
}

fn is_collection_type(lower: &str) -> bool {
    // Matches Rust `Vec<T>`, `HashMap<..>`, `BTreeMap`, `HashSet`;
    // C++ `std::vector<..>`, `std::map`, `std::set`, `std::unordered_*`;
    // Java `List<..>`, `ArrayList`, `Map`, `Set`; Python `list`, `dict`,
    // `set`, `tuple`.
    for prefix in [
        "vec<",
        "vec::",
        "hashmap<",
        "btreemap<",
        "hashset<",
        "btreeset<",
        "vecdeque<",
        "linkedlist<",
        "std::vector",
        "std::map",
        "std::set",
        "std::list",
        "std::deque",
        "std::array",
        "std::unordered_map",
        "std::unordered_set",
        "list<",
        "map<",
        "set<",
        "arraylist<",
        "hashtable<",
        "iterable<",
        "collection<",
    ] {
        if lower.starts_with(prefix) || lower.contains(&format!(" {}", prefix)) {
            return true;
        }
    }
    matches!(
        lower.as_ref(),
        "list"
            | "dict"
            | "set"
            | "tuple"
            | "frozenset"
            | "map"
            | "vector"
            | "arraylist"
            | "hashmap"
            | "btreemap"
    )
}

fn is_pointer_type(t: &str, lower: &str) -> bool {
    // C / C++: trailing *, `T *`, `T**`. Rust: raw pointers `*const T` /
    // `*mut T`, references `&T` / `&mut T`, smart pointers `Box<..>`,
    // `Rc<..>`, `Arc<..>`, `&'a T`, `Option<&T>`.
    let trimmed = t.trim();
    if trimmed.contains('*') {
        return true;
    }
    if trimmed.starts_with('&') {
        return true;
    }
    for prefix in ["box<", "rc<", "arc<", "refcell<", "cell<", "nonnull<", "weak<"] {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn is_bool_type(lower: &str) -> bool {
    matches!(lower, "bool" | "boolean" | "logical" | "_bool")
}

fn is_char_type(lower: &str) -> bool {
    matches!(lower, "char" | "c_char" | "u_char" | "wchar_t" | "char16_t" | "char32_t" | "character")
}

fn is_float_type(lower: &str) -> bool {
    matches!(
        lower,
        "f32"
            | "f64"
            | "float"
            | "double"
            | "long double"
            | "real"
            | "real4"
            | "real8"
            | "real16"
            | "float32"
            | "float64"
    ) || lower.starts_with("real(") || lower.starts_with("complex(")
}

fn is_int_type(lower: &str) -> bool {
    matches!(
        lower,
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
            | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
            | "int" | "uint" | "short" | "long" | "byte"
            | "signed" | "unsigned"
            | "long long" | "unsigned int" | "unsigned short" | "unsigned long"
            | "unsigned long long" | "signed int" | "signed short" | "signed long"
            | "size_t" | "ssize_t" | "ptrdiff_t" | "intptr_t" | "uintptr_t"
            | "int8_t" | "int16_t" | "int32_t" | "int64_t"
            | "uint8_t" | "uint16_t" | "uint32_t" | "uint64_t"
            | "c_int" | "c_uint" | "c_short" | "c_ushort" | "c_long" | "c_ulong"
            | "c_longlong" | "c_ulonglong" | "c_size_t" | "c_ssize_t"
            | "integer" | "integer*1" | "integer*2" | "integer*4" | "integer*8"
    ) || lower.starts_with("integer(")
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
