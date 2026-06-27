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
    /// Plan B t-002: role/intent tag for classification entries
    /// (e.g. "diagnostic-test", "fast-test", "fixture-path", "stale-api").
    /// Optional; only meaningful for kind=Ownership/Concept (the
    /// classification family). Skipped from JSON when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Plan B t-002: canonical reproduction or validation command for
    /// recipe-style entries (e.g. "swift test --filter SongPlayersTests").
    /// Optional; only meaningful for kind=ValidationScenario/Proof or
    /// kind=FollowUp (where it is the command that closes the follow-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
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
    /// Plan B t-002: replacement-coverage mapping. "Legacy SIDFileParserTests
    /// coverage now lives in SIDParserAndPlayerTests." Body holds the cross-
    /// reference in JSON (`from_qname`, `to_qname`, `rationale`).
    Mapping,
    /// Plan B t-002: open follow-up tied to an external task system.
    /// "SID real-file diagnostics still need migration under t-024." The
    /// `command` field on LedgerEntry may carry the closing command; the
    /// `external_task_id` evidence variant carries the task pointer.
    FollowUp,
    /// Plan G t-002: a speculative claim with confidence. Uses the
    /// existing `LedgerEntry.confidence: Option<f64>` (0.0-1.0). Promoted
    /// to Decision once validated. Distinct from Assumption: Assumption
    /// = "we proceed as if true"; Hypothesis = "I suspect, evidence
    /// pending".
    Hypothesis,
    /// Plan G t-002: multi-symbol structural understanding ("the audio
    /// pipeline flows input → mix → out"). Body MAY carry JSON
    /// `{"symbols": [qname...], "diagram": "..."}`. Distinct from
    /// Concept (single domain entity) — MentalModel spans symbols.
    MentalModel,
    /// Plan G t-002: negative evidence. "I tried X; failed because Y;
    /// pivoting to Z." Body MAY carry JSON `{"tried": "...", "because":
    /// "..."}`. Saves the next session from re-treading the same path.
    FailedAttempt,
    /// Plan G t-002: a known unknown blocking confident action. "What
    /// does this magic constant 4096 mean? Need to ask before
    /// changing." Summary is the question; body MAY hold partial
    /// findings. Distinct from FollowUp: FollowUp is "do X under task
    /// T-024"; OpenQuestion is "what does this mean?".
    OpenQuestion,
}

impl LedgerKind {
    /// Plan B t-001/t-002: map a LedgerKind to its conclusion class.
    /// Drives export bucketing into .asd/conclusions/{class}.jsonl.
    pub fn conclusion_class(self) -> ConclusionClass {
        use LedgerKind::*;
        match self {
            Decision | Assumption | Constraint | Rationale | Tradeoff | Invariant => {
                ConclusionClass::Decisions
            }
            Ownership | Concept => ConclusionClass::Classifications,
            Mapping => ConclusionClass::Mappings,
            Hazard | KnownBug => ConclusionClass::Hazards,
            ValidationScenario | Proof => ConclusionClass::Recipes,
            FollowUp => ConclusionClass::FollowUps,
            // Plan G t-002: thinking kinds bucket into a new
            // .asd/conclusions/thinking.jsonl file.
            Hypothesis | MentalModel | FailedAttempt | OpenQuestion => {
                ConclusionClass::Thinking
            }
        }
    }
}

/// The conclusion-class buckets that drive `.asd/conclusions/*.jsonl`
/// export and the Plan B sidecar redesign. Each class corresponds to one
/// JSONL file; LedgerKind variants are bucketed via `conclusion_class()`.
///
/// Plan G t-002 added `Thinking` as the 7th class (Hypothesis,
/// MentalModel, FailedAttempt, OpenQuestion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConclusionClass {
    Decisions,
    Classifications,
    Mappings,
    Hazards,
    Recipes,
    FollowUps,
    Thinking,
}

/// Plan C t-002: first-class role-tag vocabulary. The optional
/// `LedgerEntry.role` field is free-form `String` (so unknown tags don't
/// break old data), but CLI / MCP / API layers validate against this enum
/// at write time and warn on unknown values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoleTag {
    /// Lightweight test, safe in tight feedback loops.
    FastTest,
    /// Debug / instrumentation test; not part of main CI.
    DiagnosticTest,
    /// Shared test fixture; multiple tests depend on it.
    FixturePath,
    /// Deprecated interface; migration tracked.
    StaleApi,
    /// Cross-package facade; changes need coordination.
    PackageBoundary,
    /// Legacy coverage handled by newer code (paired with Mapping kind).
    ReplacementCoverage,
    /// Hot path; changes need perf measurement.
    PerformanceCritical,
    /// Not yet reviewed; pending validation.
    AuditPending,
}

impl RoleTag {
    /// Canonical wire string (kebab-case for human readability).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FastTest => "fast-test",
            Self::DiagnosticTest => "diagnostic-test",
            Self::FixturePath => "fixture-path",
            Self::StaleApi => "stale-api",
            Self::PackageBoundary => "package-boundary",
            Self::ReplacementCoverage => "replacement-coverage",
            Self::PerformanceCritical => "performance-critical",
            Self::AuditPending => "audit-pending",
        }
    }

    /// Parse a wire string. Accepts both kebab-case canonical form and
    /// snake_case for forgiveness. Returns None on unknown input — callers
    /// that need validation handle the warning themselves.
    pub fn from_str(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_lowercase().replace('_', "-");
        match norm.as_str() {
            "fast-test" => Some(Self::FastTest),
            "diagnostic-test" => Some(Self::DiagnosticTest),
            "fixture-path" => Some(Self::FixturePath),
            "stale-api" => Some(Self::StaleApi),
            "package-boundary" => Some(Self::PackageBoundary),
            "replacement-coverage" => Some(Self::ReplacementCoverage),
            "performance-critical" => Some(Self::PerformanceCritical),
            "audit-pending" => Some(Self::AuditPending),
            _ => None,
        }
    }

    /// All canonical tags in stable order — useful for CLI value-enum lists
    /// and probe assertions.
    pub fn all() -> &'static [RoleTag] {
        &[
            Self::FastTest,
            Self::DiagnosticTest,
            Self::FixturePath,
            Self::StaleApi,
            Self::PackageBoundary,
            Self::ReplacementCoverage,
            Self::PerformanceCritical,
            Self::AuditPending,
        ]
    }

    /// Plan C t-003: tags that act as ranking PENALTIES when a
    /// Constraint/Decision entry carries them. Used by the decisions-as-
    /// constraints pipeline to synthesize WrongLayer-like verdicts.
    pub fn is_penalty_role(self) -> bool {
        matches!(self, Self::StaleApi | Self::AuditPending)
    }

    /// Plan C t-003: tags that act as ranking BOOSTS to peer symbols in
    /// the same package/file (e.g. package-boundary surfaces the inside
    /// alternatives).
    pub fn is_boost_role(self) -> bool {
        matches!(self, Self::PackageBoundary | Self::PerformanceCritical)
    }
}

impl ConclusionClass {
    /// Filename stem under `.asd/conclusions/` (e.g. `decisions` → `decisions.jsonl`).
    pub fn filename_stem(self) -> &'static str {
        match self {
            Self::Decisions => "decisions",
            Self::Classifications => "classifications",
            Self::Mappings => "mappings",
            Self::Hazards => "hazards",
            Self::Recipes => "recipes",
            Self::FollowUps => "followups",
            Self::Thinking => "thinking",
        }
    }

    /// All classes in stable order — used by export to walk buckets.
    pub fn all() -> &'static [ConclusionClass] {
        &[
            Self::Decisions,
            Self::Classifications,
            Self::Mappings,
            Self::Hazards,
            Self::Recipes,
            Self::FollowUps,
            Self::Thinking,
        ]
    }
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
    /// Accumulated runtime-trace evidence. When present, `confidence` is
    /// re-derived from it on each `asd trace` ingest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_policy: Option<String>,
}

/// Accumulated runtime-trace evidence for a symbol's declared effects.
///
/// Each `asd trace` ingest classifies the run as either a **confirmation**
/// (every observed effect was already declared) or a **positive contradiction**
/// (an effect was observed at runtime that was NOT declared). A declared effect
/// that simply wasn't exercised is *not* a contradiction — absence of
/// observation is not evidence of absence — so it never lowers confidence.
///
/// `EffectDecl.confidence` is then re-derived from these counts via
/// [`RuntimeEvidence::derive_confidence`], seeded by the static `prior` so that
/// with zero runtime evidence the confidence equals the prior, each
/// confirmation pulls it toward 1.0, and each contradiction toward 0.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvidence {
    /// Traces whose observed effects were all already declared.
    pub confirmations: u64,
    /// Traces that observed an effect not present in the declaration.
    pub contradictions: u64,
    /// Static/policy confidence captured at the first runtime observation. Used
    /// as the prior the runtime evidence updates, and frozen thereafter so the
    /// derivation stays reproducible from (prior, confirmations, contradictions).
    pub prior: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trace_id: Option<String>,
    pub last_observed_at: DateTime<Utc>,
}

impl RuntimeEvidence {
    /// Pseudo-count weight given to the static prior. With zero runtime
    /// evidence the derived confidence equals the prior; each confirmation
    /// pulls it toward 1.0 and each contradiction toward 0.0, with the prior
    /// worth `PRIOR_STRENGTH` observations of resistance.
    pub const PRIOR_STRENGTH: f64 = 2.0;

    /// Neutral prior used when no static/policy confidence is available.
    pub const NEUTRAL_PRIOR: f64 = 0.5;

    /// Derive a confidence in `[0, 1]` from the static prior and accumulated
    /// counts. This is a Laplace-smoothed success rate (equivalently the mean of
    /// a Beta posterior) with the prior seeded as `PRIOR_STRENGTH`
    /// pseudo-observations:
    ///
    /// ```text
    /// confidence = (prior*S + confirmations) / (S + confirmations + contradictions)
    /// ```
    ///
    /// With `confirmations == contradictions == 0` this returns `prior` exactly.
    pub fn derive_confidence(prior: f64, confirmations: u64, contradictions: u64) -> f64 {
        let p0 = prior.clamp(0.0, 1.0);
        let s = Self::PRIOR_STRENGTH;
        let alpha = p0 * s + confirmations as f64;
        let total = s + confirmations as f64 + contradictions as f64;
        alpha / total
    }

    /// Confidence derived from this evidence record.
    pub fn confidence(&self) -> f64 {
        Self::derive_confidence(self.prior, self.confirmations, self.contradictions)
    }
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
        Self {
            effect,
            qualifiers: serde_json::Value::Null,
            note: None,
            adapter: None,
            source_pattern: None,
            verified: None,
        }
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
        matches!(
            self,
            Self::Throw | Self::Random | Self::Log | Self::Pure | Self::TimeRead | Self::TimeSleep
        )
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
            role: None,
            command: None,
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
            LedgerKind::Mapping => "mapping",
            LedgerKind::FollowUp => "follow_up",
            LedgerKind::Hypothesis => "hypothesis",
            LedgerKind::MentalModel => "mental_model",
            LedgerKind::FailedAttempt => "failed_attempt",
            LedgerKind::OpenQuestion => "open_question",
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
    /// Plan C t-005: this symbol's behavior is already covered by another
    /// symbol. Acts as suppression like `Noisy`; callers should also write
    /// a `Mapping` ledger entry pointing at the covering symbol so future
    /// queries see the connection durably.
    AlreadyCovered,
    /// Plan C t-005: this symbol is a diagnostic/instrumentation test, not
    /// production validation. Acts as suppression like `Noisy`; callers
    /// should also write a `Classification` (`role = diagnostic-test`)
    /// ledger entry so the t-003 decisions-as-constraints pipeline can
    /// demote it on future queries.
    DiagnosticOnly,
}

impl FeedbackVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Noisy => "noisy",
            Self::Missing => "missing",
            Self::WrongLayer => "wrong-layer",
            Self::AlreadyCovered => "already-covered",
            Self::DiagnosticOnly => "diagnostic-only",
        }
    }

    /// Parse from the string produced by [`as_str`].  Returns `None` on
    /// unrecognized values so callers can decide on a fallback. Accepts
    /// underscores too (`already_covered` / `diagnostic_only`) for
    /// forgiveness when agents type from memory.
    pub fn from_str(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_lowercase().replace('_', "-");
        match norm.as_str() {
            "useful" => Some(Self::Useful),
            "noisy" => Some(Self::Noisy),
            "missing" => Some(Self::Missing),
            "wrong-layer" => Some(Self::WrongLayer),
            "already-covered" => Some(Self::AlreadyCovered),
            "diagnostic-only" => Some(Self::DiagnosticOnly),
            _ => None,
        }
    }

    /// Plan C t-005: returns true when this verdict should suppress the
    /// symbol from ranked results — used by `apply_feedback_adjustments`
    /// to fold AlreadyCovered + DiagnosticOnly into the existing
    /// NEG_INFINITY suppression path.
    pub fn is_suppression(self) -> bool {
        matches!(
            self,
            Self::Noisy | Self::WrongLayer | Self::AlreadyCovered | Self::DiagnosticOnly
        )
    }
}

/// A single feedback record: a (query, symbol, verdict) triple stored durably.
///
/// When `file_scope` is set the verdict applies to all symbols from files
/// matching that glob pattern, not just the specific symbol.  In that case
/// `symbol_id` is a synthetic `"fs_<uuid>"` identifier and `symbol_qname` is
/// left empty.
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
    /// File path or glob pattern (e.g. `"App/Utility/**"`) that this verdict
    /// applies to.  When set, the verdict is applied to all symbols whose file
    /// path matches this pattern for queries in the same query family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_scope: Option<String>,
    /// Plan J t-014: optional expiry. When set, the verdict is
    /// ignored by `apply_feedback_adjustments` after this timestamp.
    /// Useful for false-positive feedback that should auto-decay
    /// (e.g. "this hit doesn't belong here today, but might next
    /// quarter when the layout shifts"). `None` = persist forever
    /// (current default behavior for backward compat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl FeedbackEntry {
    /// Plan J t-014: true when `expires_at` is set and is in the past.
    /// Filtering helper used by `apply_feedback_adjustments` so old
    /// verdicts naturally lose their ranking influence.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |t| Utc::now() > t)
    }
}

#[cfg(test)]
mod plan_b_schema_tests {
    //! Plan B t-002: regression tests for the new LedgerKind variants and
    //! optional LedgerEntry fields (role, command), and for the
    //! LedgerKind → ConclusionClass mapping that drives JSONL export.

    use super::*;

    #[test]
    fn feedback_is_expired_returns_false_when_unset() {
        // Plan J t-014: backward-compat — entries without expires_at
        // (the default for existing entries) must never count as
        // expired.
        let e = FeedbackEntry {
            entry_id: "fb_x".into(),
            symbol_id: "sym_x".into(),
            symbol_qname: "pkg.x".into(),
            query: "q".into(),
            verdict: FeedbackVerdict::Useful,
            author: "a".into(),
            created_at: Utc::now(),
            note: None,
            file_scope: None,
            expires_at: None,
        };
        assert!(!e.is_expired());
    }

    #[test]
    fn feedback_is_expired_returns_true_when_past() {
        let mut e = FeedbackEntry {
            entry_id: "fb_x".into(),
            symbol_id: "sym_x".into(),
            symbol_qname: "pkg.x".into(),
            query: "q".into(),
            verdict: FeedbackVerdict::Useful,
            author: "a".into(),
            created_at: Utc::now(),
            note: None,
            file_scope: None,
            expires_at: None,
        };
        e.expires_at = Some(Utc::now() - chrono::Duration::days(1));
        assert!(e.is_expired(), "expired 1 day ago must report expired");
    }

    #[test]
    fn feedback_is_expired_returns_false_when_future() {
        let mut e = FeedbackEntry {
            entry_id: "fb_x".into(),
            symbol_id: "sym_x".into(),
            symbol_qname: "pkg.x".into(),
            query: "q".into(),
            verdict: FeedbackVerdict::Useful,
            author: "a".into(),
            created_at: Utc::now(),
            note: None,
            file_scope: None,
            expires_at: None,
        };
        e.expires_at = Some(Utc::now() + chrono::Duration::days(30));
        assert!(!e.is_expired(), "future expiry must NOT report expired");
    }

    #[test]
    fn new_kinds_serialize_to_snake_case() {
        let json = serde_json::to_string(&LedgerKind::Mapping).unwrap();
        assert_eq!(json, "\"mapping\"");
        let json = serde_json::to_string(&LedgerKind::FollowUp).unwrap();
        assert_eq!(json, "\"follow_up\"");
    }

    #[test]
    fn new_kinds_round_trip_through_serde() {
        for kind in [LedgerKind::Mapping, LedgerKind::FollowUp] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: LedgerKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn optional_fields_are_omitted_when_none() {
        let entry = LedgerEntry::new(
            "sym_test",
            LedgerKind::Decision,
            "some decision",
            Author {
                kind: AuthorKind::Agent,
                id: "agent".into(),
            },
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("\"role\""));
        assert!(!json.contains("\"command\""));
    }

    #[test]
    fn optional_fields_round_trip_when_set() {
        let mut entry = LedgerEntry::new(
            "sym_test",
            LedgerKind::FollowUp,
            "SID real-file diagnostics still need migration",
            Author {
                kind: AuthorKind::Human,
                id: "craig".into(),
            },
        );
        entry.role = Some("diagnostic-test".into());
        entry.command = Some("swift test --filter SongPlayersTests".into());
        let json = serde_json::to_string(&entry).unwrap();
        let back: LedgerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role.as_deref(), Some("diagnostic-test"));
        assert_eq!(
            back.command.as_deref(),
            Some("swift test --filter SongPlayersTests")
        );
    }

    #[test]
    fn every_ledger_kind_buckets_to_a_conclusion_class() {
        // If a new variant gets added without updating conclusion_class(),
        // the compiler-exhaustive match in conclusion_class() will catch it.
        // This test just exercises every existing variant to make sure no
        // bucket is empty by accident.
        use ConclusionClass::*;
        let pairs = [
            (LedgerKind::Decision, Decisions),
            (LedgerKind::Assumption, Decisions),
            (LedgerKind::Constraint, Decisions),
            (LedgerKind::Rationale, Decisions),
            (LedgerKind::Tradeoff, Decisions),
            (LedgerKind::Invariant, Decisions),
            (LedgerKind::Ownership, Classifications),
            (LedgerKind::Concept, Classifications),
            (LedgerKind::Mapping, Mappings),
            (LedgerKind::Hazard, Hazards),
            (LedgerKind::KnownBug, Hazards),
            (LedgerKind::ValidationScenario, Recipes),
            (LedgerKind::Proof, Recipes),
            (LedgerKind::FollowUp, FollowUps),
        ];
        for (kind, expected) in pairs {
            assert_eq!(
                kind.conclusion_class(),
                expected,
                "kind {:?} should bucket to {:?}",
                kind,
                expected
            );
        }
    }

    #[test]
    fn conclusion_class_filename_stems_match_design() {
        // Plan A/B/G DESIGN sections name these exactly. Renaming requires
        // a migration; lock them down with this test.
        assert_eq!(ConclusionClass::Decisions.filename_stem(), "decisions");
        assert_eq!(
            ConclusionClass::Classifications.filename_stem(),
            "classifications"
        );
        assert_eq!(ConclusionClass::Mappings.filename_stem(), "mappings");
        assert_eq!(ConclusionClass::Hazards.filename_stem(), "hazards");
        assert_eq!(ConclusionClass::Recipes.filename_stem(), "recipes");
        assert_eq!(ConclusionClass::FollowUps.filename_stem(), "followups");
        // Plan G t-002: thinking-class bucket.
        assert_eq!(ConclusionClass::Thinking.filename_stem(), "thinking");
        assert_eq!(ConclusionClass::all().len(), 7);
    }

    #[test]
    fn plan_g_thinking_kinds_bucket_to_thinking_class() {
        // Lock the t-002 bucketing decision: all 4 thinking-kinds land
        // in ConclusionClass::Thinking, not in Classifications or
        // FollowUps. Failing this means the export pipeline would write
        // them to the wrong .asd/conclusions/*.jsonl file.
        for kind in [
            LedgerKind::Hypothesis,
            LedgerKind::MentalModel,
            LedgerKind::FailedAttempt,
            LedgerKind::OpenQuestion,
        ] {
            assert_eq!(
                kind.conclusion_class(),
                ConclusionClass::Thinking,
                "kind {:?} must bucket to Thinking",
                kind
            );
        }
    }

    #[test]
    fn plan_g_thinking_kinds_wire_strings_match_design() {
        assert_eq!(LedgerKind::Hypothesis.as_str(), "hypothesis");
        assert_eq!(LedgerKind::MentalModel.as_str(), "mental_model");
        assert_eq!(LedgerKind::FailedAttempt.as_str(), "failed_attempt");
        assert_eq!(LedgerKind::OpenQuestion.as_str(), "open_question");
    }

    #[test]
    fn plan_g_thinking_kinds_serde_round_trip() {
        use serde_json::json;
        for kind in [
            LedgerKind::Hypothesis,
            LedgerKind::MentalModel,
            LedgerKind::FailedAttempt,
            LedgerKind::OpenQuestion,
        ] {
            let v = serde_json::to_value(kind).unwrap();
            assert_eq!(v, json!(kind.as_str()));
            let back: LedgerKind = serde_json::from_value(v).unwrap();
            assert_eq!(back, kind);
        }
    }

    // -- Plan C t-002: RoleTag vocabulary lock-down -------------------------

    #[test]
    fn role_tag_wire_strings_match_design() {
        // Locks the canonical wire format to the t-001 design table.
        // Changing any of these is a migration.
        assert_eq!(RoleTag::FastTest.as_str(), "fast-test");
        assert_eq!(RoleTag::DiagnosticTest.as_str(), "diagnostic-test");
        assert_eq!(RoleTag::FixturePath.as_str(), "fixture-path");
        assert_eq!(RoleTag::StaleApi.as_str(), "stale-api");
        assert_eq!(RoleTag::PackageBoundary.as_str(), "package-boundary");
        assert_eq!(
            RoleTag::ReplacementCoverage.as_str(),
            "replacement-coverage"
        );
        assert_eq!(
            RoleTag::PerformanceCritical.as_str(),
            "performance-critical"
        );
        assert_eq!(RoleTag::AuditPending.as_str(), "audit-pending");
        assert_eq!(RoleTag::all().len(), 8);
    }

    #[test]
    fn role_tag_round_trips_via_from_str() {
        for tag in RoleTag::all() {
            assert_eq!(RoleTag::from_str(tag.as_str()), Some(*tag));
        }
    }

    #[test]
    fn role_tag_from_str_accepts_snake_case_and_whitespace() {
        assert_eq!(RoleTag::from_str("fast_test"), Some(RoleTag::FastTest));
        assert_eq!(RoleTag::from_str("  Stale-API  "), Some(RoleTag::StaleApi));
        assert_eq!(
            RoleTag::from_str("audit_pending"),
            Some(RoleTag::AuditPending)
        );
    }

    #[test]
    fn role_tag_from_str_returns_none_on_unknown() {
        assert_eq!(RoleTag::from_str("not-a-real-tag"), None);
        assert_eq!(RoleTag::from_str(""), None);
    }

    #[test]
    fn penalty_and_boost_roles_match_t003_design() {
        // Plan C t-003 expects exactly these two penalty roles.
        let penalties: Vec<&str> = RoleTag::all()
            .iter()
            .filter(|t| t.is_penalty_role())
            .map(|t| t.as_str())
            .collect();
        assert_eq!(penalties, vec!["stale-api", "audit-pending"]);

        // And exactly these two boost-peers roles.
        let boosts: Vec<&str> = RoleTag::all()
            .iter()
            .filter(|t| t.is_boost_role())
            .map(|t| t.as_str())
            .collect();
        assert_eq!(boosts, vec!["package-boundary", "performance-critical"]);
    }
}

#[cfg(test)]
mod runtime_evidence_tests {
    //! t-001: runtime-trace → confidence derivation. Confidence is derived from
    //! accumulated confirmation/contradiction counts, seeded by the static prior.
    //! Absence of observation is handled at the classification layer (it never
    //! becomes a contradiction), so these tests only cover the count math.

    use super::*;

    fn derive(prior: f64, conf: u64, contra: u64) -> f64 {
        RuntimeEvidence::derive_confidence(prior, conf, contra)
    }

    #[test]
    fn zero_evidence_equals_prior() {
        // With no runtime observations the confidence is exactly the prior.
        for p in [0.0, 0.2, 0.5, 0.8, 1.0] {
            assert!((derive(p, 0, 0) - p).abs() < 1e-9, "prior {p} not preserved");
        }
    }

    #[test]
    fn confirmations_raise_contradictions_lower() {
        let prior = 0.5;
        assert!(derive(prior, 3, 0) > prior, "confirmations should raise");
        assert!(derive(prior, 0, 3) < prior, "contradictions should lower");
    }

    #[test]
    fn monotonic_in_each_count() {
        let prior = 0.6;
        // Adding a confirmation never lowers; adding a contradiction never raises.
        for n in 0..20 {
            assert!(derive(prior, n + 1, 0) >= derive(prior, n, 0));
            assert!(derive(prior, 0, n + 1) <= derive(prior, 0, n));
        }
    }

    #[test]
    fn always_within_unit_interval() {
        for &p in &[-1.0, 0.0, 0.3, 1.0, 2.0] {
            for conf in [0u64, 1, 50, 10_000] {
                for contra in [0u64, 1, 50, 10_000] {
                    let c = derive(p, conf, contra);
                    assert!((0.0..=1.0).contains(&c), "out of range: {c} for {p},{conf},{contra}");
                }
            }
        }
    }

    #[test]
    fn evidence_overwhelms_prior_asymptotically() {
        // A confident-but-wrong prior is dragged down by sustained contradiction.
        let c = derive(0.9, 0, 1_000);
        assert!(c < 0.05, "1000 contradictions should crush a 0.9 prior, got {c}");
        // And confirmations push a low prior toward 1.0.
        let c2 = derive(0.1, 1_000, 0);
        assert!(c2 > 0.95, "1000 confirmations should lift a 0.1 prior, got {c2}");
    }

    #[test]
    fn balanced_evidence_trends_to_half() {
        // Equal confirmations/contradictions wash out toward 0.5 regardless of prior.
        let c = derive(0.9, 500, 500);
        assert!((c - 0.5).abs() < 0.02, "balanced evidence should near 0.5, got {c}");
    }

    #[test]
    fn confidence_method_matches_free_fn() {
        let ev = RuntimeEvidence {
            confirmations: 4,
            contradictions: 1,
            prior: 0.5,
            last_trace_id: Some("trc_x".into()),
            last_observed_at: Utc::now(),
        };
        assert!((ev.confidence() - derive(0.5, 4, 1)).abs() < 1e-12);
    }

    #[test]
    fn effect_decl_persists_runtime_and_is_backward_compatible() {
        // New field survives a serde round-trip (the effect store serializes
        // EffectDecl to JSON in git + the SQLite cache).
        let d = EffectDecl {
            symbol_id: "s".into(),
            declared: Vec::new(),
            transitive: Vec::new(),
            verification: None,
            confidence: Some(0.83),
            runtime: Some(RuntimeEvidence {
                confirmations: 3,
                contradictions: 1,
                prior: 0.5,
                last_trace_id: Some("trc_1".into()),
                last_observed_at: Utc::now(),
            }),
            matched_policy: None,
        };
        let back: EffectDecl = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        let rt = back.runtime.expect("runtime survives round-trip");
        assert_eq!((rt.confirmations, rt.contradictions, rt.prior), (3, 1, 0.5));

        // Existing records written before this field must still deserialize.
        let old = r#"{"symbol_id":"s","declared":[],"transitive":[],"confidence":0.5}"#;
        let parsed: EffectDecl = serde_json::from_str(old).unwrap();
        assert!(parsed.runtime.is_none());
    }
}
