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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
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
    /// A design or implementation decision that was made.
    Decision,
    /// An assumption that was made (may need validation).
    Assumption,
    /// A hard constraint this symbol must always satisfy.
    Constraint,
    /// Rationale for why something was done a particular way.
    Rationale,
    /// A known danger: what can go wrong if this changes.
    Hazard,
    /// A tradeoff accepted to gain some benefit.
    Tradeoff,
    /// An invariant that must always hold at this symbol.
    Invariant,
    /// Ownership declaration: which subsystem/team owns this symbol.
    Ownership,
    /// Evidence that an invariant holds (test, review, trace, etc.).
    Proof,
    /// A concrete scenario that should be validated (behaviour + expected outcome).
    ValidationScenario,
    /// A known bug or defect that has not been fixed yet.
    KnownBug,
    /// A domain concept (e.g. "Drift Pad clip playhead") — first-class queryable entity.
    Concept,
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
    /// Language adapter that inferred this effect (e.g. "swift", "python").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// Source-code pattern that triggered inference (e.g. "FileManager", "URLSession").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pattern: Option<String>,
    /// Whether verify-effects confirmed this effect against the current source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
}

impl Effect {
    pub fn new(effect: EffectCategory) -> Self {
        Self { effect, qualifiers: serde_json::Value::Null, note: None, adapter: None, source_pattern: None, verified: None }
    }
}

impl Default for Effect {
    fn default() -> Self {
        Self::new(EffectCategory::Pure)
    }
}

/// Effect category — either a well-known built-in or a user-defined domain
/// string (e.g. `"midi.send"`, `"audio.graph.connect"`).
///
/// User-defined categories are declared with `asd effect declare --category`
/// and are not inferred automatically by language adapters.  Well-known
/// categories may be inferred.
///
/// # Extensibility
/// The `Other(String)` variant accepts any dot-separated namespace string.
/// Recommended namespaces for common domains:
/// - `audio.*` — audio engine operations (`audio.graph.connect`, `audio.graph.disconnect`)
/// - `midi.*` — MIDI I/O (`midi.send`, `midi.receive`)
/// - `scheduler.*` — sequencer/lane control (`scheduler.restart`, `scheduler.stop`)
/// - `ui.*` — UI state mutations (`ui.state.mutate`)
/// - `file.*` — higher-level import/export (`file.import`, `file.export`)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectCategory {
    // Infrastructure effects (inferred by language adapters)
    IoFsRead,
    IoFsWrite,
    IoNetIn,
    IoNetOut,
    IoDbRead,
    IoDbWrite,
    StateGlobalRead,
    StateGlobalWrite,
    StateProcess,
    EnvRead,
    TimeRead,
    TimeSleep,
    Random,
    ProcSpawn,
    Throw,
    Log,
    Pure,
    /// User-defined domain effect (e.g. `"midi.send"`, `"audio.graph.connect"`).
    Other(String),
}

impl EffectCategory {
    pub fn as_str(&self) -> &str {
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
            EffectCategory::Other(s) => s.as_str(),
        }
    }

    /// Returns true for effects that are nearly universal and low-signal
    /// (appear on most symbols, rarely indicate a meaningful side-effect).
    /// Used to suppress noise in `prepare_change` and `effects_of` output.
    /// Low-signal effects are only shown when they are the only effects on a symbol.
    pub fn is_low_signal(&self) -> bool {
        matches!(self, Self::Throw | Self::Random | Self::Log | Self::Pure | Self::TimeRead | Self::TimeSleep)
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "io.fs.read" => EffectCategory::IoFsRead,
            "io.fs.write" => EffectCategory::IoFsWrite,
            "io.net.in" => EffectCategory::IoNetIn,
            "io.net.out" => EffectCategory::IoNetOut,
            "io.db.read" => EffectCategory::IoDbRead,
            "io.db.write" => EffectCategory::IoDbWrite,
            "state.global.read" => EffectCategory::StateGlobalRead,
            "state.global.write" => EffectCategory::StateGlobalWrite,
            "state.process" => EffectCategory::StateProcess,
            "env.read" => EffectCategory::EnvRead,
            "time.read" => EffectCategory::TimeRead,
            "time.sleep" => EffectCategory::TimeSleep,
            "random" => EffectCategory::Random,
            "proc.spawn" => EffectCategory::ProcSpawn,
            "throw" => EffectCategory::Throw,
            "log" => EffectCategory::Log,
            "pure" => EffectCategory::Pure,
            other => EffectCategory::Other(other.to_string()),
        }
    }
}

impl serde::Serialize for EffectCategory {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for EffectCategory {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(EffectCategory::from_str(&s))
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
            LedgerKind::Invariant => "invariant",
            LedgerKind::Ownership => "ownership",
            LedgerKind::Proof => "proof",
            LedgerKind::ValidationScenario => "validation_scenario",
            LedgerKind::KnownBug => "known_bug",
            LedgerKind::Concept => "concept",
        }
    }
}

// ---------------------------------------------------------------------------
// Scratchpad types
// ---------------------------------------------------------------------------

/// Status of a [`ScratchEntry`]. Transitions: Draft → Promoted or Discarded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScratchStatus {
    /// Working note, not yet acted on.
    Draft,
    /// Promoted to a ledger entry; `promoted_to` holds the `entry_id`.
    Promoted,
    /// Explicitly discarded by the author.
    Discarded,
}

/// Ephemeral working note scoped to a symbol and/or named workflow.
///
/// Scratch entries are stored locally at `/asd/v1/scratch/<scratch_id>`.
/// They are **not** synced to the sidecar and not subject to policy gate.
/// Use [`ScratchStatus::Promoted`] + `promoted_to` to link a note that
/// has been elevated to a durable [`LedgerEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchEntry {
    /// Stable ID, format `"scr_<uuid-simple>"`.
    pub scratch_id: String,
    /// Optional: scope to an indexed symbol_id.
    pub symbol_id: Option<String>,
    /// Optional: named investigation context (e.g. `"tracing-sync-bug"`).
    pub workflow: Option<String>,
    /// Agent or user who wrote the note (`agent_id`).
    pub session: String,
    /// Markdown-friendly working notes.
    pub content: String,
    /// Current status.
    pub status: ScratchStatus,
    /// Set when status transitions to `Promoted`; holds the ledger `entry_id`.
    pub promoted_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When set, the entry is considered expired after this timestamp.
    pub expires_at: Option<DateTime<Utc>>,
    /// Freeform tags for grouping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl ScratchEntry {
    /// Create a new draft entry with minimal fields. Caller fills in optional
    /// `symbol_id`, `workflow`, `expires_at`, and `tags` after construction.
    pub fn new(content: impl Into<String>, session: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            scratch_id: format!("scr_{}", Uuid::new_v4().simple()),
            symbol_id: None,
            workflow: None,
            session: session.into(),
            content: content.into(),
            status: ScratchStatus::Draft,
            promoted_to: None,
            created_at: now,
            updated_at: now,
            expires_at: None,
            tags: Vec::new(),
        }
    }

    /// Returns `true` when `expires_at` is set and is in the past.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |t| Utc::now() > t)
    }
}

// ---------------------------------------------------------------------------

/// Record that a symbol was renamed/moved so its ledger history follows it.
/// Written to `/asd/v1/rebinds/<from_symbol_id>` at rename time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rebind {
    pub from_symbol_id: String,
    pub to_symbol_id: String,
    pub to_qname: String,
    pub at: DateTime<Utc>,
    pub by: String,
}

// ---------------------------------------------------------------------------
// Feedback model
// ---------------------------------------------------------------------------

/// Verdict an agent or user assigns to a search result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackVerdict {
    /// This result was relevant and useful for the query.
    Useful,
    /// This result appeared but was unrelated to the query intent.
    Noisy,
    /// This symbol was missing from results but should have appeared.
    Missing,
    /// This symbol appeared in the wrong architectural layer context.
    WrongLayer,
}

impl FeedbackVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Noisy => "noisy",
            Self::Missing => "missing",
            Self::WrongLayer => "wrong-layer",
        }
    }
}

/// A single feedback record: a (query, symbol, verdict) triple stored durably.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEntry {
    pub entry_id: String,
    pub symbol_id: String,
    pub symbol_qname: String,
    /// Normalized (lowercase, trimmed) query that produced this result.
    pub query: String,
    pub verdict: FeedbackVerdict,
    pub author: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
