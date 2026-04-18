use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ASD_SCHEMA_VERSION: &str = "0.1.0";
pub const ASD_PATH_PREFIX: &str = "/asd/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub symbol_id: String,
    pub symbol_fp: String,
    pub qname: String,
    pub language: String,
    pub kind: SymbolKind,
    pub file: String,
    pub start: Position,
    pub end: Position,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Function,
    Method,
    Class,
    Variable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub entry_id: String,
    pub symbol_id: String,
    pub kind: LedgerKind,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub author: Author,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_policy: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Decision,
    Assumption,
    Constraint,
    Rationale,
    Hazard,
    Tradeoff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    Agent,
    Human,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Evidence {
    Trace { id: String },
    Ledger { id: String },
    Test { qname: String },
    Ctxone { id: String },
    External { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectDecl {
    pub symbol_id: String,
    #[serde(default)]
    pub declared: Vec<Effect>,
    #[serde(default)]
    pub transitive: Vec<TransitiveEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effect {
    pub effect: EffectCategory,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub qualifiers: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EffectCategory {
    #[serde(rename = "io.fs.read")]
    IoFsRead,
    #[serde(rename = "io.fs.write")]
    IoFsWrite,
    #[serde(rename = "io.net.in")]
    IoNetIn,
    #[serde(rename = "io.net.out")]
    IoNetOut,
    #[serde(rename = "io.db.read")]
    IoDbRead,
    #[serde(rename = "io.db.write")]
    IoDbWrite,
    #[serde(rename = "state.global.read")]
    StateGlobalRead,
    #[serde(rename = "state.global.write")]
    StateGlobalWrite,
    #[serde(rename = "state.process")]
    StateProcess,
    #[serde(rename = "env.read")]
    EnvRead,
    #[serde(rename = "time.read")]
    TimeRead,
    #[serde(rename = "time.sleep")]
    TimeSleep,
    #[serde(rename = "random")]
    Random,
    #[serde(rename = "proc.spawn")]
    ProcSpawn,
    #[serde(rename = "throw")]
    Throw,
    #[serde(rename = "log")]
    Log,
    #[serde(rename = "pure")]
    Pure,
}

impl EffectCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            EffectCategory::IoFsRead => "io.fs.read",
            EffectCategory::IoFsWrite => "io.fs.write",
            EffectCategory::IoNetIn => "io.net.in",
            EffectCategory::IoNetOut => "io.net.out",
            EffectCategory::IoDbRead => "io.db.read",
            EffectCategory::IoDbWrite => "io.db.write",
            EffectCategory::StateGlobalRead => "state.global.read",
            EffectCategory::StateGlobalWrite => "state.global.write",
            EffectCategory::StateProcess => "state.process",
            EffectCategory::EnvRead => "env.read",
            EffectCategory::TimeRead => "time.read",
            EffectCategory::TimeSleep => "time.sleep",
            EffectCategory::Random => "random",
            EffectCategory::ProcSpawn => "proc.spawn",
            EffectCategory::Throw => "throw",
            EffectCategory::Log => "log",
            EffectCategory::Pure => "pure",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitiveEffect {
    pub effect: EffectCategory,
    pub via: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub qualifiers: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub by: VerificationSource,
    pub at: DateTime<Utc>,
    pub status: VerificationStatus,
    #[serde(default)]
    pub mismatches: Vec<Mismatch>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationSource {
    StaticChecker,
    RuntimeTracer,
    TestObserved,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Ok,
    Mismatch,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub kind: String,
    pub effect: EffectCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_in: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl LedgerEntry {
    pub fn new(
        symbol_id: impl Into<String>,
        kind: LedgerKind,
        summary: impl Into<String>,
        author: Author,
    ) -> Self {
        Self {
            entry_id: format!("led_{}", Uuid::new_v4().simple()),
            symbol_id: symbol_id.into(),
            kind,
            summary: summary.into(),
            body: None,
            author,
            confidence: None,
            evidence: Vec::new(),
            supersedes: Vec::new(),
            created_at: Utc::now(),
            tags: Vec::new(),
            matched_policy: None,
        }
    }
}

impl LedgerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LedgerKind::Decision => "decision",
            LedgerKind::Assumption => "assumption",
            LedgerKind::Constraint => "constraint",
            LedgerKind::Rationale => "rationale",
            LedgerKind::Hazard => "hazard",
            LedgerKind::Tradeoff => "tradeoff",
        }
    }
}
